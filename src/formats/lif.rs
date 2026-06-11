//! Leica LIF (Leica Image Format) reader.
//!
//! LIF is a binary Leica container: a sequence of memory blocks, the first of
//! which carries a UTF-16LE XML description (`<LMSDataContainerHeader>` with an
//! `<Element>` tree). Each `<Element>` that holds an `<Image>` describes one
//! series: channels (`<ChannelDescription>`) and dimensions
//! (`<DimensionDescription>` with `DimID` 1=X 2=Y 3=Z 4=T, 10=tile). Subsequent
//! binary blocks carry the pixel payloads, matched to the XML `<Memory>`
//! entries by their memory-block IDs.
//!
//! This port parses the block layout, enumerates series from the XML, derives
//! per-series dimensions / pixel type, and maps memory-block IDs to pixel data
//! offsets. Tiled acquisitions are expanded into one series per tile, matching
//! the Java behaviour. Pixel reads are supported for the simple uncompressed
//! non-RGB strided layout confirmed by local fixtures and RGB payloads whose
//! XML strides fully describe repeated interleaved or planar RGB triples; other
//! payload variants return precise `UnsupportedFormat` errors.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use quick_xml::events::Event;

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::ome_metadata::{OmeChannel, OmeImage, OmeMetadata};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;

const LIF_MAGIC_BYTE: u8 = 0x70;
const LIF_MEMORY_BYTE: u8 = 0x2a;

/// One pixel-data memory block: a byte offset into the file plus its ID.
#[derive(Debug, Clone)]
struct MemoryBlock {
    file_offset: u64,
    byte_len: u64,
    id: String,
}

/// Per-series core metadata derived from one `<Image>` element.
#[derive(Debug, Clone)]
struct SeriesInfo {
    meta: ImageMetadata,
    /// Number of tiles this image was split into (>=1).
    tile_count: u32,
    /// OME-level metadata (image name, physical sizes, channel names) derived
    /// from the LIF XML, mirroring Java `LIFReader`.
    ome: OmeImage,
    layout: PixelLayout,
}

/// Byte strides declared by Leica's `<ChannelDescription>` and
/// `<DimensionDescription>` elements.
#[derive(Debug, Clone, Default)]
struct PixelLayout {
    x_stride: u64,
    y_stride: u64,
    channel_offsets: Vec<u64>,
    c_stride: Option<u64>,
    z_stride: Option<u64>,
    t_stride: Option<u64>,
    tile_stride: Option<u64>,
    compression: Option<String>,
}

pub struct LifReader {
    path: Option<PathBuf>,
    /// One entry per (expanded) series; tiled images contribute `tile_count`
    /// identical entries.
    series: Vec<SeriesInfo>,
    /// One memory block per *tile group* (i.e. per original `<Image>` element).
    memory_blocks: Vec<MemoryBlock>,
    /// File offset where pixel data ends (next block or EOF).
    end_pointer: u64,
    current_series: usize,
    file_len: u64,
}

impl LifReader {
    pub fn new() -> Self {
        LifReader {
            path: None,
            series: Vec::new(),
            memory_blocks: Vec::new(),
            end_pointer: 0,
            current_series: 0,
            file_len: 0,
        }
    }

    fn cur(&self) -> Result<&SeriesInfo> {
        self.series
            .get(self.current_series)
            .ok_or(BioFormatsError::SeriesOutOfRange(self.current_series))
    }

    /// Returns one `SeriesInfo` per original `<Image>` element (deduplicated
    /// from the expanded per-tile list).
    fn tile_groups(&self) -> Vec<&SeriesInfo> {
        let mut groups: Vec<&SeriesInfo> = Vec::new();
        let mut i = 0usize;
        while i < self.series.len() {
            groups.push(&self.series[i]);
            i += self.series[i].tile_count.max(1) as usize;
        }
        groups
    }

    fn tile_position(&self, series: usize) -> (usize, usize) {
        let mut count = 0usize;
        for (group, info) in self.tile_groups().iter().enumerate() {
            let tiles = info.tile_count.max(1) as usize;
            if series < count + tiles {
                return (group, series - count);
            }
            count += tiles;
        }
        (0, 0)
    }

    fn parse(&mut self, data: &[u8]) -> Result<()> {
        if data.len() < 13 {
            return Err(BioFormatsError::Format("LIF file too short".into()));
        }
        self.file_len = data.len() as u64;

        // -- header --
        // byte 0: magic; skip 2; byte 3: magic; skip 4; then 0x2A + XML
        let check_one = data[0];
        let check_two = data[3];
        if check_one != LIF_MAGIC_BYTE && check_two != LIF_MAGIC_BYTE {
            return Err(BioFormatsError::Format("Not a valid Leica LIF file".into()));
        }
        let mut off: usize = 8;
        if data[off] != LIF_MEMORY_BYTE {
            return Err(BioFormatsError::Format(
                "Invalid LIF XML description".into(),
            ));
        }
        off += 1;
        let nc = read_i32(data, off)? as i64;
        off += 4;
        let xml_bytes = nc
            .checked_mul(2)
            .filter(|n| *n >= 0)
            .ok_or_else(|| BioFormatsError::Format("Invalid LIF XML length".into()))?
            as usize;
        if off + xml_bytes > data.len() {
            return Err(BioFormatsError::Format("LIF XML extends past EOF".into()));
        }
        let xml = decode_utf16le(&data[off..off + xml_bytes]);
        off += xml_bytes;

        // -- memory blocks --
        let mut raw_blocks: Vec<MemoryBlock> = Vec::new();
        let mut end_pointer: u64 = 0;
        while off < data.len() {
            if off + 4 > data.len() {
                break;
            }
            let check = read_i32(data, off)?;
            off += 4;
            if check != LIF_MAGIC_BYTE as i32 {
                if check == 0 && !raw_blocks.is_empty() {
                    // newer LIF: trailing zeros after the last block
                    end_pointer = off as u64;
                    break;
                }
                return Err(BioFormatsError::Format(format!(
                    "Invalid LIF memory block: magic {check}"
                )));
            }
            off += 4; // skip the per-block size word
            if off >= data.len() || data[off] != LIF_MEMORY_BYTE {
                return Err(BioFormatsError::Format(
                    "Invalid LIF memory description".into(),
                ));
            }
            off += 1;
            let mut block_length: u64 = read_i32(data, off)? as u32 as u64;
            off += 4;
            let test = data.get(off).copied();
            off += 1;
            if test != Some(LIF_MEMORY_BYTE) {
                // BigTIFF-style 64-bit length: rewind to before the int32.
                off -= 5;
                block_length = read_i64(data, off)? as u64;
                off += 8;
                if data.get(off).copied() != Some(LIF_MEMORY_BYTE) {
                    return Err(BioFormatsError::Format(
                        "Invalid LIF memory description (64-bit)".into(),
                    ));
                }
                off += 1;
            }
            let descr_len = (read_i32(data, off)? as usize)
                .checked_mul(2)
                .ok_or_else(|| BioFormatsError::Format("Invalid LIF block ID length".into()))?;
            off += 4;
            if off + descr_len > data.len() {
                return Err(BioFormatsError::Format("LIF block ID past EOF".into()));
            }
            let mem_id = decode_utf16le(&data[off..off + descr_len]);
            off += descr_len;
            if block_length > 0 {
                raw_blocks.push(MemoryBlock {
                    file_offset: off as u64,
                    byte_len: block_length,
                    id: mem_id,
                });
            }
            off = off.saturating_add(block_length as usize);
        }
        if end_pointer == 0 {
            end_pointer = data.len() as u64;
        }
        self.end_pointer = end_pointer;

        // -- XML metadata --
        let (series, ordered_ids) = parse_xml(&xml)?;
        if series.is_empty() {
            return Err(BioFormatsError::Format("No images found in LIF".into()));
        }
        self.series = series;

        // Match memory blocks to image elements by ID, preserving the XML
        // order. Fall back to file order if IDs do not match.
        let mut matched: Vec<MemoryBlock> = Vec::new();
        for id in &ordered_ids {
            if let Some(b) = raw_blocks.iter().find(|b| &b.id == id) {
                matched.push(b.clone());
            }
        }
        self.memory_blocks = if matched.len() == ordered_ids.len() && !matched.is_empty() {
            matched
        } else {
            raw_blocks
        };

        Ok(())
    }
}

impl Default for LifReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for LifReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("lif"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // magic; skip 7; 0x2A; int32 nc; string == "LMS_Object_File" => LOF, not LIF
        if header.len() < 13 {
            return false;
        }
        if header[0] != LIF_MAGIC_BYTE {
            return false;
        }
        if header[8] != LIF_MEMORY_BYTE {
            return false;
        }
        let nc = match read_i32(header, 9) {
            Ok(v) => v as i64,
            Err(_) => return false,
        };
        if nc < 0 {
            return false;
        }
        let want = (nc * 2) as usize;
        let avail = header.len() - 13;
        let take = want.min(avail);
        let desc = decode_utf16le(&header[13..13 + take]);
        desc != "LMS_Object_File"
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.path = Some(path.to_path_buf());
        self.current_series = 0;
        self.series.clear();
        self.memory_blocks.clear();

        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        self.parse(&data)?;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.series.clear();
        self.memory_blocks.clear();
        self.end_pointer = 0;
        self.current_series = 0;
        self.file_len = 0;
        Ok(())
    }

    fn series_count(&self) -> usize {
        self.series.len()
    }

    fn set_series(&mut self, series: usize) -> Result<()> {
        if series >= self.series.len() {
            return Err(BioFormatsError::SeriesOutOfRange(series));
        }
        self.current_series = series;
        Ok(())
    }

    fn series(&self) -> usize {
        self.current_series
    }

    fn metadata(&self) -> &ImageMetadata {
        match self.series.get(self.current_series) {
            Some(info) => &info.meta,
            None => crate::common::reader::uninitialized_metadata(),
        }
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let (w, h) = {
            let m = &self.cur()?.meta;
            (m.size_x, m.size_y)
        };
        self.open_bytes_region(plane_index, 0, 0, w, h)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        if self.path.is_none() {
            return Err(BioFormatsError::NotInitialized);
        }

        let info = self.cur()?;
        let m = &info.meta;
        if plane_index >= m.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        if x.checked_add(w).map_or(true, |end| end > m.size_x)
            || y.checked_add(h).map_or(true, |end| end > m.size_y)
        {
            return Err(BioFormatsError::Format("LIF region out of bounds".into()));
        }

        let (group, tile) = self.tile_position(self.current_series);
        let block = self.memory_blocks.get(group).ok_or_else(|| {
            BioFormatsError::UnsupportedFormat(format!(
                "Leica LIF pixel block for series {} was not discovered",
                self.current_series
            ))
        })?;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        decode_lif_region(&data, block, info, tile as u64, plane_index, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        self.open_bytes(plane_index)
    }

    fn ome_metadata(&self) -> Option<OmeMetadata> {
        if self.series.is_empty() {
            return None;
        }
        Some(OmeMetadata {
            images: self.series.iter().map(|s| s.ome.clone()).collect(),
            ..OmeMetadata::default()
        })
    }
}

fn decode_lif_region(
    data: &[u8],
    block: &MemoryBlock,
    info: &SeriesInfo,
    tile: u64,
    plane_index: u32,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
) -> Result<Vec<u8>> {
    let meta = &info.meta;
    let bps = meta.pixel_type.bytes_per_sample() as u64;
    let plane_layout = lif_plane_layout(meta, &info.layout)?;
    let rgb_samples = lif_rgb_channel_count(meta);
    let samples = match plane_layout {
        LifPlaneLayout::InterleavedRgb => rgb_samples as u64,
        LifPlaneLayout::Scalar | LifPlaneLayout::PlanarRgb => 1,
    };
    let pixel_stride = checked_mul_u64(bps, samples, "Leica LIF pixel stride")?;
    let layout = &info.layout;
    if layout.x_stride != bps {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "Leica LIF unsupported X stride {}; expected {bps} bytes for {:?}",
            layout.x_stride, meta.pixel_type
        )));
    }
    let min_row = u64::from(meta.size_x)
        .checked_mul(pixel_stride)
        .ok_or_else(|| BioFormatsError::Format("Leica LIF row size overflows".into()))?;
    if layout.y_stride < min_row {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "Leica LIF unsupported Y stride {}; expected at least {min_row}",
            layout.y_stride
        )));
    }

    let (z, c, t) = zct_for_plane(plane_index, meta);
    let tile_offset = if tile == 0 {
        0
    } else {
        let stride = info.layout.tile_stride.ok_or_else(|| {
            BioFormatsError::UnsupportedFormat(format!(
                "Leica LIF missing tile byte stride for {} tiles",
                info.tile_count.max(1)
            ))
        })?;
        checked_mul_u64(tile, stride, "Leica LIF tile offset")?
    };
    let plane_base = tile_offset;
    let plane_base = if meta.is_rgb {
        plane_base
    } else {
        checked_add_u64(
            plane_base,
            axis_offset(c, meta.size_c, layout.c_stride, "channel")?,
            "Leica LIF channel offset",
        )?
    };
    let plane_base = checked_add_u64(
        plane_base,
        axis_offset(z, meta.size_z, layout.z_stride, "Z")?,
        "Leica LIF Z offset",
    )?;
    let plane_base = checked_add_u64(
        plane_base,
        axis_offset(t, meta.size_t, layout.t_stride, "T")?,
        "Leica LIF T offset",
    )?;

    if matches!(plane_layout, LifPlaneLayout::PlanarRgb) {
        return decode_lif_planar_rgb_region(
            data,
            block,
            info,
            plane_base,
            c,
            plane_index,
            x,
            y,
            w,
            h,
        );
    }

    let row_start_delta = checked_add_u64(
        checked_mul_u64(u64::from(y), layout.y_stride, "Leica LIF row offset")?,
        checked_mul_u64(u64::from(x), pixel_stride, "Leica LIF column offset")?,
        "Leica LIF region offset",
    )?;
    let rgb_group_base = if matches!(plane_layout, LifPlaneLayout::InterleavedRgb) {
        lif_rgb_group_offsets(layout, c, rgb_samples)?[0]
    } else {
        0
    };
    let out_row = checked_mul_u64(u64::from(w), pixel_stride, "Leica LIF output row")? as usize;
    let mut row_ranges = Vec::with_capacity(h as usize);
    for row in 0..u64::from(h) {
        let src = checked_add_u64(
            checked_add_u64(plane_base, rgb_group_base, "Leica LIF RGB group offset")?,
            checked_add_u64(
                row_start_delta,
                checked_mul_u64(row, layout.y_stride, "Leica LIF row offset")?,
                "Leica LIF row offset",
            )?,
            "Leica LIF source offset",
        )?;
        let end = checked_add_u64(src, out_row as u64, "Leica LIF source end")?;
        row_ranges.push((src, end));
    }

    let mut out = Vec::with_capacity(
        (h as usize)
            .checked_mul(out_row)
            .ok_or_else(|| BioFormatsError::Format("Leica LIF output size overflows".into()))?,
    );

    if let Some(compression) = &info.layout.compression {
        copy_lif_compressed_ranges(data, block, compression, &row_ranges, plane_index, &mut out)?;
        return Ok(out);
    }

    for (src, end) in row_ranges {
        let block_end = block
            .file_offset
            .checked_add(block.byte_len)
            .ok_or_else(|| BioFormatsError::Format("Leica LIF block end overflows".into()))?;
        let abs_src = checked_add_u64(block.file_offset, src, "Leica LIF source offset")?;
        let abs_end = checked_add_u64(block.file_offset, end, "Leica LIF source end")?;
        if abs_end > block_end || abs_end as usize > data.len() {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "Leica LIF plane {plane_index} exceeds memory block {} (offset {abs_src}, end {abs_end}, block end {block_end})",
                block.id
            )));
        }
        out.extend_from_slice(&data[abs_src as usize..abs_end as usize]);
    }
    Ok(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LifPlaneLayout {
    Scalar,
    InterleavedRgb,
    PlanarRgb,
}

fn lif_plane_layout(meta: &ImageMetadata, layout: &PixelLayout) -> Result<LifPlaneLayout> {
    if !meta.is_rgb {
        return Ok(LifPlaneLayout::Scalar);
    }
    let rgb_samples = lif_rgb_channel_count(meta);
    if rgb_samples != 3
        || meta.size_c < rgb_samples
        || meta.size_c % rgb_samples != 0
        || layout.channel_offsets.len() != meta.size_c as usize
    {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "Leica LIF unsupported RGB layout: interleaved={}, channel count={}, channel offsets={:?}",
            meta.is_interleaved, meta.size_c, layout.channel_offsets
        )));
    }

    let bps = meta.pixel_type.bytes_per_sample() as u64;
    if meta.is_interleaved {
        for group in layout.channel_offsets.chunks_exact(rgb_samples as usize) {
            if group[1] != checked_add_u64(group[0], bps, "Leica LIF RGB sample offset")?
                || group[2]
                    != checked_add_u64(
                        group[0],
                        checked_mul_u64(bps, 2, "Leica LIF RGB sample offset")?,
                        "Leica LIF RGB sample offset",
                    )?
            {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "Leica LIF unsupported RGB layout: interleaved={}, channel offsets={:?}, expected repeated RGB triples",
                    meta.is_interleaved, layout.channel_offsets
                )));
            }
        }
        return Ok(LifPlaneLayout::InterleavedRgb);
    } else if layout.x_stride == bps {
        let plane_stride = checked_mul_u64(
            layout.y_stride,
            u64::from(meta.size_y),
            "Leica LIF RGB plane stride",
        )?;
        for group in layout.channel_offsets.chunks_exact(rgb_samples as usize) {
            if group[1] != checked_add_u64(group[0], plane_stride, "Leica LIF RGB plane offset")?
                || group[2]
                    != checked_add_u64(
                        group[0],
                        checked_mul_u64(plane_stride, 2, "Leica LIF RGB plane offset")?,
                        "Leica LIF RGB plane offset",
                    )?
            {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "Leica LIF unsupported RGB layout: interleaved={}, channel offsets={:?}, expected repeated planar RGB triples",
                    meta.is_interleaved, layout.channel_offsets
                )));
            }
        }
        return Ok(LifPlaneLayout::PlanarRgb);
    }

    Err(BioFormatsError::UnsupportedFormat(format!(
        "Leica LIF unsupported RGB layout: interleaved={}, channel offsets={:?}",
        meta.is_interleaved, layout.channel_offsets
    )))
}

fn decode_lif_planar_rgb_region(
    data: &[u8],
    block: &MemoryBlock,
    info: &SeriesInfo,
    plane_base: u64,
    c: u32,
    plane_index: u32,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
) -> Result<Vec<u8>> {
    let meta = &info.meta;
    let layout = &info.layout;
    let bps = meta.pixel_type.bytes_per_sample() as u64;
    let out_row = checked_mul_u64(u64::from(w), bps, "Leica LIF output row")? as usize;
    let mut out = Vec::with_capacity(
        (h as usize)
            .checked_mul(out_row)
            .and_then(|n| n.checked_mul(meta.size_c as usize))
            .ok_or_else(|| BioFormatsError::Format("Leica LIF output size overflows".into()))?,
    );
    let row_start_delta = checked_add_u64(
        checked_mul_u64(u64::from(y), layout.y_stride, "Leica LIF row offset")?,
        checked_mul_u64(u64::from(x), bps, "Leica LIF column offset")?,
        "Leica LIF region offset",
    )?;
    let block_end = block
        .file_offset
        .checked_add(block.byte_len)
        .ok_or_else(|| BioFormatsError::Format("Leica LIF block end overflows".into()))?;

    let mut row_ranges = Vec::with_capacity(meta.size_c as usize * h as usize);
    let rgb_samples = lif_rgb_channel_count(meta) as usize;
    let first_channel = (c as usize)
        .checked_mul(rgb_samples)
        .ok_or_else(|| BioFormatsError::Format("Leica LIF RGB channel index overflows".into()))?;
    let group_offsets = layout
        .channel_offsets
        .get(first_channel..first_channel + rgb_samples)
        .ok_or_else(|| {
            BioFormatsError::UnsupportedFormat(format!(
                "Leica LIF RGB channel group {c} is not described by channel offsets {:?}",
                layout.channel_offsets
            ))
        })?;
    for &channel_offset in group_offsets {
        let channel_base =
            checked_add_u64(plane_base, channel_offset, "Leica LIF RGB channel offset")?;
        for row in 0..u64::from(h) {
            let src = checked_add_u64(
                channel_base,
                checked_add_u64(
                    row_start_delta,
                    checked_mul_u64(row, layout.y_stride, "Leica LIF row offset")?,
                    "Leica LIF row offset",
                )?,
                "Leica LIF source offset",
            )?;
            let end = checked_add_u64(src, out_row as u64, "Leica LIF source end")?;
            row_ranges.push((src, end));
        }
    }

    if let Some(compression) = &info.layout.compression {
        copy_lif_compressed_ranges(data, block, compression, &row_ranges, plane_index, &mut out)?;
        return Ok(out);
    }

    for (src, end) in row_ranges {
        let abs_src = checked_add_u64(block.file_offset, src, "Leica LIF source offset")?;
        let abs_end = checked_add_u64(block.file_offset, end, "Leica LIF source end")?;
        if abs_end > block_end || abs_end as usize > data.len() {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                    "Leica LIF plane {plane_index} exceeds memory block {} (offset {abs_src}, end {abs_end}, block end {block_end})",
                    block.id
                )));
        }
        out.extend_from_slice(&data[abs_src as usize..abs_end as usize]);
    }
    Ok(out)
}

#[derive(Debug, Clone, Copy)]
enum LifCompression {
    Zlib,
    RawDeflate,
}

fn copy_lif_compressed_ranges(
    data: &[u8],
    block: &MemoryBlock,
    compression: &str,
    row_ranges: &[(u64, u64)],
    plane_index: u32,
    out: &mut Vec<u8>,
) -> Result<()> {
    let kind = lif_compression_kind(compression).ok_or_else(|| {
        BioFormatsError::UnsupportedFormat(format!(
            "Leica LIF compressed pixel payload declares unsupported compression hint: {compression}; only zlib/deflate memory blocks are supported"
        ))
    })?;
    let block_end = block
        .file_offset
        .checked_add(block.byte_len)
        .ok_or_else(|| BioFormatsError::Format("Leica LIF block end overflows".into()))?;
    if block_end as usize > data.len() {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "Leica LIF compressed memory block {} exceeds file length (end {block_end}, file {})",
            block.id,
            data.len()
        )));
    }
    let compressed = &data[block.file_offset as usize..block_end as usize];
    stream_lif_compressed_ranges(compressed, kind, row_ranges, &block.id, plane_index, out)
}

fn lif_compression_kind(compression: &str) -> Option<LifCompression> {
    let lower = compression.to_ascii_lowercase();
    if lower.contains("zlib") {
        Some(LifCompression::Zlib)
    } else if lower.contains("deflate") {
        Some(LifCompression::RawDeflate)
    } else {
        None
    }
}

fn stream_lif_compressed_ranges(
    compressed: &[u8],
    kind: LifCompression,
    row_ranges: &[(u64, u64)],
    block_id: &str,
    plane_index: u32,
    out: &mut Vec<u8>,
) -> Result<()> {
    use flate2::read::{DeflateDecoder, ZlibDecoder};

    match kind {
        LifCompression::Zlib => {
            let mut decoder = ZlibDecoder::new(compressed);
            read_lif_compressed_ranges(&mut decoder, row_ranges, block_id, plane_index, out)?;
        }
        LifCompression::RawDeflate => {
            let mut decoder = DeflateDecoder::new(compressed);
            read_lif_compressed_ranges(&mut decoder, row_ranges, block_id, plane_index, out)?;
        }
    }
    Ok(())
}

fn read_lif_compressed_ranges<R: std::io::Read>(
    reader: &mut R,
    row_ranges: &[(u64, u64)],
    block_id: &str,
    plane_index: u32,
    out: &mut Vec<u8>,
) -> Result<()> {
    use std::io::{sink, Read};

    let mut pos = 0u64;
    for &(src, end) in row_ranges {
        if src < pos {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "Leica LIF compressed plane {plane_index} has overlapping requested ranges"
            )));
        }
        let skip = src - pos;
        if skip > 0 {
            let copied = std::io::copy(&mut reader.by_ref().take(skip), &mut sink())
                .map_err(BioFormatsError::Io)?;
            if copied != skip {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "Leica LIF plane {plane_index} exceeds decompressed memory block {block_id} (offset {src}, end {end}, decoded {pos})"
                )));
            }
        }
        let len = usize::try_from(end - src).map_err(|_| {
            BioFormatsError::Format("Leica LIF decoded row slice is too large".into())
        })?;
        let start = out.len();
        out.resize(start + len, 0);
        if let Err(err) = reader.read_exact(&mut out[start..]) {
            out.truncate(start);
            return Err(BioFormatsError::Io(err));
        }
        pos = end;
    }
    Ok(())
}

fn axis_offset(index: u32, size: u32, stride: Option<u64>, axis: &str) -> Result<u64> {
    if size <= 1 || index == 0 {
        return Ok(0);
    }
    let stride = stride.ok_or_else(|| {
        BioFormatsError::UnsupportedFormat(format!(
            "Leica LIF missing {axis} byte stride for {size} positions"
        ))
    })?;
    checked_mul_u64(u64::from(index), stride, "Leica LIF axis offset")
}

fn checked_mul_u64(a: u64, b: u64, what: &str) -> Result<u64> {
    a.checked_mul(b)
        .ok_or_else(|| BioFormatsError::Format(format!("{what} overflows")))
}

fn checked_add_u64(a: u64, b: u64, what: &str) -> Result<u64> {
    a.checked_add(b)
        .ok_or_else(|| BioFormatsError::Format(format!("{what} overflows")))
}

fn zct_for_plane(plane_index: u32, meta: &ImageMetadata) -> (u32, u32, u32) {
    let mut rem = plane_index;
    let mut z = 0;
    let mut c = 0;
    let mut t = 0;
    for axis in dimension_axes(meta.dimension_order) {
        match axis {
            'Z' => {
                z = rem % meta.size_z.max(1);
                rem /= meta.size_z.max(1);
            }
            'C' => {
                let size_c = if meta.is_rgb {
                    lif_effective_size_c(meta)
                } else {
                    meta.size_c.max(1)
                };
                c = rem % size_c;
                rem /= size_c;
            }
            'T' => {
                t = rem % meta.size_t.max(1);
                rem /= meta.size_t.max(1);
            }
            _ => {}
        }
    }
    (z, c, t)
}

fn lif_rgb_channel_count(meta: &ImageMetadata) -> u32 {
    if meta.is_rgb {
        3
    } else {
        1
    }
}

fn lif_effective_size_c(meta: &ImageMetadata) -> u32 {
    if meta.is_rgb {
        (meta.size_c / lif_rgb_channel_count(meta)).max(1)
    } else {
        meta.size_c.max(1)
    }
}

fn lif_rgb_group_offsets(layout: &PixelLayout, c: u32, rgb_samples: u32) -> Result<&[u64]> {
    let first = (c as usize)
        .checked_mul(rgb_samples as usize)
        .ok_or_else(|| BioFormatsError::Format("Leica LIF RGB channel index overflows".into()))?;
    layout
        .channel_offsets
        .get(first..first + rgb_samples as usize)
        .ok_or_else(|| {
            BioFormatsError::UnsupportedFormat(format!(
                "Leica LIF RGB channel group {c} is not described by channel offsets {:?}",
                layout.channel_offsets
            ))
        })
}

fn dimension_axes(order: DimensionOrder) -> [char; 3] {
    match order {
        DimensionOrder::XYCTZ => ['C', 'T', 'Z'],
        DimensionOrder::XYCZT => ['C', 'Z', 'T'],
        DimensionOrder::XYTCZ => ['T', 'C', 'Z'],
        DimensionOrder::XYTZC => ['T', 'Z', 'C'],
        DimensionOrder::XYZCT => ['Z', 'C', 'T'],
        DimensionOrder::XYZTC => ['Z', 'T', 'C'],
    }
}

// -- byte helpers --

fn read_i32(data: &[u8], off: usize) -> Result<i32> {
    if off + 4 > data.len() {
        return Err(BioFormatsError::Format("LIF: read past EOF (i32)".into()));
    }
    Ok(i32::from_le_bytes([
        data[off],
        data[off + 1],
        data[off + 2],
        data[off + 3],
    ]))
}

fn read_i64(data: &[u8], off: usize) -> Result<i64> {
    if off + 8 > data.len() {
        return Err(BioFormatsError::Format("LIF: read past EOF (i64)".into()));
    }
    let mut b = [0u8; 8];
    b.copy_from_slice(&data[off..off + 8]);
    Ok(i64::from_le_bytes(b))
}

/// Decode UTF-16LE, stripping trailing/leading NULs (Java `stripString`).
fn decode_utf16le(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16_lossy(&units)
        .trim_matches(|c: char| c == '\u{0}')
        .to_string()
}

// -- XML parsing --

/// A minimal DOM node built from the LIF XML so we can walk parent/child
/// relationships the way the Java reader does.
struct Node {
    name: String,
    attrs: BTreeMap<String, String>,
    children: Vec<usize>,
    parent: Option<usize>,
}

struct Dom {
    nodes: Vec<Node>,
}

impl Dom {
    fn parse(xml: &str) -> Result<Dom> {
        let mut reader = quick_xml::Reader::from_str(xml);
        reader.config_mut().trim_text(false);
        let mut nodes: Vec<Node> = Vec::new();
        let mut stack: Vec<usize> = Vec::new();

        let push_node =
            |nodes: &mut Vec<Node>, stack: &[usize], e: &quick_xml::events::BytesStart| -> usize {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                let mut attrs = BTreeMap::new();
                for a in e.attributes().flatten() {
                    let key = String::from_utf8_lossy(a.key.as_ref()).to_string();
                    let val = a
                        .unescape_value()
                        .map(|v| v.to_string())
                        .unwrap_or_default();
                    attrs.insert(key, val);
                }
                let parent = stack.last().copied();
                let idx = nodes.len();
                nodes.push(Node {
                    name,
                    attrs,
                    children: Vec::new(),
                    parent,
                });
                if let Some(p) = parent {
                    nodes[p].children.push(idx);
                }
                idx
            };

        loop {
            match reader.read_event() {
                Ok(Event::Start(e)) => {
                    let idx = push_node(&mut nodes, &stack, &e);
                    stack.push(idx);
                }
                Ok(Event::Empty(e)) => {
                    push_node(&mut nodes, &stack, &e);
                }
                Ok(Event::End(_)) => {
                    stack.pop();
                }
                Ok(Event::Eof) => break,
                Ok(_) => {}
                Err(e) => return Err(BioFormatsError::Format(format!("LIF XML parse error: {e}"))),
            }
        }
        Ok(Dom { nodes })
    }

    fn children_named<'a>(&'a self, idx: usize, name: &'a str) -> impl Iterator<Item = usize> + 'a {
        self.nodes[idx]
            .children
            .iter()
            .copied()
            .filter(move |c| self.nodes[*c].name == name)
    }

    /// First descendant (any depth) with the given tag name.
    fn first_descendant(&self, idx: usize, name: &str) -> Option<usize> {
        for &c in &self.nodes[idx].children {
            if self.nodes[c].name == name {
                return Some(c);
            }
            if let Some(found) = self.first_descendant(c, name) {
                return Some(found);
            }
        }
        None
    }

    /// All descendants (any depth) with the given tag name.
    fn descendants(&self, idx: usize, name: &str, out: &mut Vec<usize>) {
        for &c in &self.nodes[idx].children {
            if self.nodes[c].name == name {
                out.push(c);
            }
            self.descendants(c, name, out);
        }
    }
}

/// Parse the LIF XML, returning (expanded series list, ordered memory-block IDs
/// one per original image element).
fn parse_xml(xml: &str) -> Result<(Vec<SeriesInfo>, Vec<String>)> {
    let dom = Dom::parse(xml)?;
    if dom.nodes.is_empty() {
        return Err(BioFormatsError::Format("Empty LIF XML".into()));
    }
    let root = 0usize;

    let mut image_nodes: Vec<usize> = Vec::new();
    dom.descendants(root, "Image", &mut image_nodes);

    let mut series: Vec<SeriesInfo> = Vec::new();
    let mut ordered_ids: Vec<String> = Vec::new();

    for &img in &image_nodes {
        // Java: grandparent = image.parent.parent (the owning <Element>).
        let parent = match dom.nodes[img].parent {
            Some(p) => p,
            None => continue,
        };
        let grandparent = match dom.nodes[parent].parent {
            Some(g) => g,
            None => continue,
        };
        // Skip event-list references (grandparent named ProcessingHistory).
        if dom.nodes[grandparent].name == "ProcessingHistory" {
            continue;
        }
        // Find the Memory child of the grandparent.
        let mem_node = dom.children_named(grandparent, "Memory").next();
        let mem_id = mem_node.and_then(|m| dom.nodes[m].attrs.get("MemoryBlockID").cloned());

        let mut info = translate_image(&dom, img)?;
        info.ome.name = Some(image_name(&dom, img));
        if let Some(compression) = compression_hint(&dom, img, mem_node) {
            info.meta.series_metadata.insert(
                "lif.compression".to_string(),
                MetadataValue::String(compression.clone()),
            );
            info.meta
                .series_metadata
                .insert("lif.compressed".to_string(), MetadataValue::Bool(true));
            info.layout.compression = Some(compression);
        }
        let tiles = info.tile_count.max(1);
        for _ in 0..tiles {
            series.push(info.clone());
        }
        ordered_ids.push(mem_id.unwrap_or_default());
    }

    Ok((series, ordered_ids))
}

fn compression_hint(dom: &Dom, img: usize, mem_node: Option<usize>) -> Option<String> {
    let mut nodes = Vec::new();
    if let Some(mem) = mem_node {
        nodes.push(mem);
    }
    nodes.push(img);
    dom.descendants(img, "ImageDescription", &mut nodes);
    dom.descendants(img, "ChannelDescription", &mut nodes);
    dom.descendants(img, "DimensionDescription", &mut nodes);

    for node in nodes {
        for (key, value) in &dom.nodes[node].attrs {
            if let Some(hint) = compression_attr_hint(key, value) {
                return Some(hint);
            }
        }
    }
    None
}

fn compression_attr_hint(key: &str, value: &str) -> Option<String> {
    let key_lc = key.to_ascii_lowercase();
    let value_lc = value.trim().to_ascii_lowercase();
    let value_trimmed = value.trim();

    if key_lc.contains("compression") || key_lc.contains("compressor") {
        if !is_uncompressed_hint(&value_lc) {
            return Some(format!("{key}={value_trimmed}"));
        }
    }
    if key_lc.contains("compressed") && !is_uncompressed_hint(&value_lc) {
        return Some(format!("{key}={value_trimmed}"));
    }
    if value_lc.contains("compressed") && !value_lc.contains("uncompressed") {
        return Some(format!("{key}={value_trimmed}"));
    }
    None
}

fn is_uncompressed_hint(value: &str) -> bool {
    matches!(
        value,
        "" | "0" | "false" | "no" | "none" | "raw" | "uncompressed" | "not compressed"
    )
}

/// Mirror of Java `translateImageNames`: walk the ancestor chain of an
/// `<Image>` element, collecting the `Name` attribute of every enclosing
/// `<Element>` (innermost first) until the `LEICA` root (or top) is reached,
/// then concatenate them — dropping the outermost (experiment) name — joined by
/// `/`. For a top-level image this yields just the image element's own name.
fn image_name(dom: &Dom, img: usize) -> String {
    let mut names: Vec<String> = Vec::new();
    let mut cur = dom.nodes[img].parent;
    while let Some(idx) = cur {
        let node = &dom.nodes[idx];
        if node.name == "LEICA" {
            break;
        }
        if node.name == "Element" {
            names.push(node.attrs.get("Name").cloned().unwrap_or_default());
        }
        cur = node.parent;
    }
    // Java: name = ""; for (k = names.size()-2; k >= 0; k--) { name += names[k]; if (k>0) name += "/"; }
    if names.len() < 2 {
        return String::new();
    }
    let mut out = String::new();
    let mut k = names.len() as isize - 2;
    while k >= 0 {
        out.push_str(&names[k as usize]);
        if k > 0 {
            out.push('/');
        }
        k -= 1;
    }
    out
}

/// Mirror of Java `translateImageNodes`: derive dimensions/pixel type from the
/// `<ImageDescription>` of one `<Image>` element.
fn translate_image(dom: &Dom, img: usize) -> Result<SeriesInfo> {
    let mut m = ImageMetadata {
        is_little_endian: true,
        ..ImageMetadata::default()
    };

    let image_desc = dom
        .first_descendant(img, "ImageDescription")
        .ok_or_else(|| BioFormatsError::Format("LIF image has no ImageDescription".into()))?;

    // Channels.
    let channels_node = dom.first_descendant(image_desc, "Channels");
    let mut channel_nodes: Vec<usize> = Vec::new();
    if let Some(cn) = channels_node {
        dom.descendants(cn, "ChannelDescription", &mut channel_nodes);
    }
    m.size_c = channel_nodes.len().max(1) as u32;

    // bytesPerAxis: sorted map nBytes -> axis, used to derive dimension order.
    let mut bytes_per_axis: BTreeMap<u64, char> = BTreeMap::new();
    let mut channel_offsets: Vec<u64> = Vec::with_capacity(channel_nodes.len());
    let mut c_stride: Option<u64> = None;
    for &ch in &channel_nodes {
        if let Some(bi) = dom.nodes[ch].attrs.get("BytesInc") {
            if let Ok(b) = bi.trim().parse::<u64>() {
                channel_offsets.push(b);
                if b > 0 {
                    bytes_per_axis.insert(b, 'C');
                    c_stride = Some(c_stride.map_or(b, |prev| prev.min(b)));
                }
            }
        }
    }

    // Dimensions.
    let dims_node = dom.first_descendant(image_desc, "Dimensions");
    let mut dim_nodes: Vec<usize> = Vec::new();
    if let Some(dn) = dims_node {
        dom.descendants(dn, "DimensionDescription", &mut dim_nodes);
    }

    let mut tile_count: u32 = 1;
    let mut tile_bytes_inc: u64 = 0;
    let mut extras: u64 = 1;
    let mut size_z: u32 = 0;
    let mut size_t: u32 = 0;
    let mut size_x: u32 = 0;
    let mut size_y: u32 = 0;
    let mut is_rgb = false;
    let mut pixel_type = PixelType::Uint8;
    let mut x_stride: u64 = 0;
    let mut y_stride: u64 = 0;
    let mut z_stride: Option<u64> = None;
    let mut t_stride: Option<u64> = None;

    // Physical pixel sizes (µm), mirroring Java `translateImageNodes`:
    // length / (numElements - 1), unit-normalised to µm (Unit="m" → ×1e6).
    let mut physical_size_x: Option<f64> = None;
    let mut physical_size_y: Option<f64> = None;
    let mut physical_size_z: Option<f64> = None;

    for &d in &dim_nodes {
        let attrs = &dom.nodes[d].attrs;
        let id: i32 = attrs
            .get("DimID")
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0);
        let len: u32 = attrs
            .get("NumberOfElements")
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0);
        let mut n_bytes: u64 = attrs
            .get("BytesInc")
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0);

        // Calibration: length / (numElements - 1), normalised to µm.
        let phys = physical_size_um(attrs, len);

        match id {
            1 => {
                size_x = len;
                physical_size_x = phys;
                is_rgb = n_bytes > 0 && n_bytes % 3 == 0;
                if is_rgb {
                    n_bytes /= 3;
                }
                x_stride = n_bytes;
                pixel_type = pixel_type_from_bytes(n_bytes);
            }
            2 => {
                if size_y != 0 {
                    if size_z <= 1 {
                        size_z = len;
                        physical_size_z = phys.map(f64::abs);
                        bytes_per_axis.insert(n_bytes, 'Z');
                        z_stride = Some(n_bytes);
                    } else if size_t <= 1 {
                        size_t = len;
                        bytes_per_axis.insert(n_bytes, 'T');
                        t_stride = Some(n_bytes);
                    }
                } else {
                    size_y = len;
                    physical_size_y = phys;
                    y_stride = n_bytes;
                }
            }
            3 => {
                if size_y == 0 {
                    // XZ scan: swap Y and Z
                    size_y = len;
                    size_z = 1;
                    physical_size_y = phys;
                    bytes_per_axis.insert(n_bytes, 'Y');
                    y_stride = n_bytes;
                } else {
                    size_z = len;
                    physical_size_z = phys.map(f64::abs);
                    bytes_per_axis.insert(n_bytes, 'Z');
                    z_stride = Some(n_bytes);
                }
            }
            4 => {
                if size_y == 0 {
                    // XT scan: swap Y and T
                    size_y = len;
                    size_t = 1;
                    physical_size_y = phys;
                    bytes_per_axis.insert(n_bytes, 'Y');
                    y_stride = n_bytes;
                } else {
                    size_t = len;
                    bytes_per_axis.insert(n_bytes, 'T');
                    t_stride = Some(n_bytes);
                }
            }
            10 => {
                tile_count = tile_count.saturating_mul(len.max(1));
                tile_bytes_inc = n_bytes;
            }
            _ => {
                extras = extras.saturating_mul(len.max(1) as u64);
            }
        }
    }

    if extras > 1 {
        if size_z <= 1 {
            size_z = extras as u32;
        } else if size_t == 0 {
            size_t = extras as u32;
        } else {
            size_t = size_t.saturating_mul(extras as u32);
        }
    }

    if m.size_c == 0 {
        m.size_c = 1;
    }
    let size_z = size_z.max(1);
    let size_t = size_t.max(1);
    let size_x = size_x.max(1);
    let size_y = size_y.max(1);

    m.size_x = size_x;
    m.size_y = size_y;
    m.size_z = size_z;
    m.size_t = size_t;
    m.pixel_type = pixel_type;
    m.bits_per_pixel = (pixel_type.bytes_per_sample() * 8) as u8;
    m.dimension_order = dimension_order_from_bytes(&bytes_per_axis);
    m.is_rgb = is_rgb;
    m.is_interleaved = is_rgb
        && !is_direct_planar_rgb(
            is_rgb,
            m.size_c,
            pixel_type.bytes_per_sample() as u64,
            size_y,
            x_stride,
            y_stride,
            &channel_offsets,
        )?;
    m.is_indexed = !is_rgb;
    m.series_metadata.insert(
        "lif.x_bytes_inc".to_string(),
        MetadataValue::Int(x_stride.min(i64::MAX as u64) as i64),
    );
    m.series_metadata.insert(
        "lif.y_bytes_inc".to_string(),
        MetadataValue::Int(y_stride.min(i64::MAX as u64) as i64),
    );
    if let Some(stride) = z_stride {
        m.series_metadata.insert(
            "lif.z_bytes_inc".to_string(),
            MetadataValue::Int(stride.min(i64::MAX as u64) as i64),
        );
    }
    if let Some(stride) = t_stride {
        m.series_metadata.insert(
            "lif.t_bytes_inc".to_string(),
            MetadataValue::Int(stride.min(i64::MAX as u64) as i64),
        );
    }
    m.series_metadata.insert(
        "lif.channel_bytes_inc".to_string(),
        MetadataValue::String(
            channel_offsets
                .iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join(","),
        ),
    );
    m.series_metadata.insert(
        "lif.tile_count".to_string(),
        MetadataValue::Int(i64::from(tile_count.max(1))),
    );
    if tile_bytes_inc > 0 {
        m.series_metadata.insert(
            "lif.tile_bytes_inc".to_string(),
            MetadataValue::Int(tile_bytes_inc.min(i64::MAX as u64) as i64),
        );
    }

    let rgb_channel_count = if is_rgb { 3 } else { 1 };
    m.image_count = size_z * size_t * (m.size_c / rgb_channel_count.max(1));
    m.series_metadata.insert(
        "lif.rgb_samples_per_pixel".to_string(),
        MetadataValue::Int(i64::from(rgb_channel_count)),
    );
    m.series_metadata.insert(
        "lif.effective_size_c".to_string(),
        MetadataValue::Int(i64::from((m.size_c / rgb_channel_count.max(1)).max(1))),
    );

    // Effective channel count (OME channels): one per ChannelDescription for
    // non-RGB, or the RGB group count otherwise.
    let effective_c = (m.size_c / rgb_channel_count.max(1)).max(1) as usize;
    let ch_names = channel_names(dom, img, effective_c);
    let channels: Vec<OmeChannel> = ch_names
        .into_iter()
        .map(|name| OmeChannel {
            name,
            samples_per_pixel: rgb_channel_count.max(1),
            ..OmeChannel::default()
        })
        .collect();

    let ome = OmeImage {
        physical_size_x: physical_size_x.filter(|v| *v > 0.0),
        physical_size_y: physical_size_y.filter(|v| *v > 0.0),
        physical_size_z: physical_size_z.filter(|v| *v > 0.0),
        channels,
        ..OmeImage::default()
    };

    Ok(SeriesInfo {
        meta: m,
        tile_count,
        ome,
        layout: PixelLayout {
            x_stride,
            y_stride,
            channel_offsets,
            c_stride,
            z_stride,
            t_stride,
            tile_stride: (tile_bytes_inc > 0).then_some(tile_bytes_inc),
            compression: None,
        },
    })
}

fn is_direct_planar_rgb(
    is_rgb: bool,
    size_c: u32,
    bps: u64,
    size_y: u32,
    x_stride: u64,
    y_stride: u64,
    channel_offsets: &[u64],
) -> Result<bool> {
    if !is_rgb
        || size_c < 3
        || size_c % 3 != 0
        || channel_offsets.len() != size_c as usize
        || x_stride != bps
    {
        return Ok(false);
    }
    let plane_stride = checked_mul_u64(y_stride, u64::from(size_y), "Leica LIF RGB plane stride")?;
    for group in channel_offsets.chunks_exact(3) {
        if group[1] != checked_add_u64(group[0], plane_stride, "Leica LIF RGB plane offset")?
            || group[2]
                != checked_add_u64(
                    group[0],
                    checked_mul_u64(plane_stride, 2, "Leica LIF RGB plane offset")?,
                    "Leica LIF RGB plane offset",
                )?
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn dimension_order_from_bytes(bytes_per_axis: &BTreeMap<u64, char>) -> DimensionOrder {
    let mut axes: Vec<char> = bytes_per_axis
        .values()
        .copied()
        .filter(|axis| matches!(axis, 'C' | 'Z' | 'T'))
        .collect();
    for axis in ['C', 'Z', 'T'] {
        if !axes.contains(&axis) {
            axes.push(axis);
        }
    }
    match (axes.first(), axes.get(1), axes.get(2)) {
        (Some('C'), Some('Z'), Some('T')) => DimensionOrder::XYCZT,
        (Some('C'), Some('T'), Some('Z')) => DimensionOrder::XYCTZ,
        (Some('Z'), Some('C'), Some('T')) => DimensionOrder::XYZCT,
        (Some('Z'), Some('T'), Some('C')) => DimensionOrder::XYZTC,
        (Some('T'), Some('C'), Some('Z')) => DimensionOrder::XYTCZ,
        (Some('T'), Some('Z'), Some('C')) => DimensionOrder::XYTZC,
        _ => DimensionOrder::XYCZT,
    }
}

/// Compute the physical pixel size in micrometres for one
/// `<DimensionDescription>`, mirroring Java `translateImageNodes` with the
/// default (non-legacy) calculation: `length / (numElements - 1)`, then
/// unit-normalised (`Unit="m"` → ×1e6, `Unit="Ks"` → ÷1000). Returns `None`
/// when there is no calibration (≤1 element or blank length).
fn physical_size_um(attrs: &BTreeMap<String, String>, num_elements: u32) -> Option<f64> {
    if num_elements <= 1 {
        return None;
    }
    let raw = attrs.get("Length").map(|s| s.trim()).unwrap_or("");
    if raw.is_empty() {
        return None;
    }
    let length: f64 = raw.parse().ok()?;
    let mut value = length / (num_elements as f64 - 1.0);
    match attrs.get("Unit").map(String::as_str) {
        Some("Ks") => value /= 1000.0,
        Some("m") => value *= 1_000_000.0,
        _ => {}
    }
    if value.is_finite() {
        Some(value)
    } else {
        None
    }
}

/// Derive per-channel names, mirroring the subset of Java
/// `LIFReader.translateDetectors` that populates `channelNames`. LIF stores dye
/// names on `<MultiBand>` elements; Java collects the distinct dye names (an
/// empty `DyeName` is kept as `""`) and assigns them to the *trailing*
/// channels: channel `c` receives `dyeNames[c + dyeNames.len() - effectiveC]`
/// when that index is valid, leaving leading channels unnamed (`None`).
fn channel_names(dom: &Dom, img: usize, effective_c: usize) -> Vec<Option<String>> {
    // Distinct dye names across all <MultiBand> descendants (dedup, keep "").
    let mut multibands: Vec<usize> = Vec::new();
    dom.descendants(img, "MultiBand", &mut multibands);
    let mut dye_names: Vec<String> = Vec::new();
    for &mb in &multibands {
        let dye = dom.nodes[mb]
            .attrs
            .get("DyeName")
            .cloned()
            .unwrap_or_default();
        if !dye_names.contains(&dye) {
            dye_names.push(dye);
        }
    }

    let mut names = vec![None; effective_c];
    if !dye_names.is_empty() {
        for (c, slot) in names.iter_mut().enumerate() {
            let idx = c as isize + dye_names.len() as isize - effective_c as isize;
            if idx >= 0 && (idx as usize) < dye_names.len() {
                *slot = Some(dye_names[idx as usize].clone());
            }
        }
    }
    names
}

/// Java `FormatTools.pixelTypeFromBytes(nBytes, signed=false, fp=true)`:
/// LIF channels are unsigned integer.
fn pixel_type_from_bytes(n_bytes: u64) -> PixelType {
    match n_bytes {
        0 | 1 => PixelType::Uint8,
        2 => PixelType::Uint16,
        4 => PixelType::Uint32,
        8 => PixelType::Float64,
        _ => PixelType::Uint8,
    }
}

#[cfg(test)]
mod tests {
    use super::LifReader;
    use crate::common::error::BioFormatsError;
    use crate::common::metadata::MetadataValue;
    use crate::common::pixel_type::PixelType;
    use crate::common::reader::FormatReader;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_lif_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("bioformats_lif_{nanos}_{name}.lif"))
    }

    fn utf16le(text: &str) -> Vec<u8> {
        let mut out = Vec::new();
        for unit in text.encode_utf16() {
            out.extend_from_slice(&unit.to_le_bytes());
        }
        out
    }

    fn synthetic_lif_bytes() -> Vec<u8> {
        let xml = r#"<LMSDataContainerHeader><Element Name="Experiment"><Element Name="Scan"><Memory MemoryBlockID="Mem1"/><Data><Image Name="Image A"><ImageDescription><Channels><ChannelDescription BytesInc="0"><Detector><MultiBand DyeName="DAPI"/></Detector></ChannelDescription><ChannelDescription BytesInc="24"><Detector><MultiBand DyeName="FITC"/></Detector></ChannelDescription></Channels><Dimensions><DimensionDescription DimID="1" NumberOfElements="4" BytesInc="2" Length="0.000003" Unit="m"/><DimensionDescription DimID="2" NumberOfElements="3" BytesInc="8" Length="0.000002" Unit="m"/><DimensionDescription DimID="3" NumberOfElements="2" BytesInc="48" Length="0.000004" Unit="m"/><DimensionDescription DimID="4" NumberOfElements="2" BytesInc="96"/></Dimensions></ImageDescription></Image></Data></Element></Element></LMSDataContainerHeader>"#;
        let xml = utf16le(xml);

        let mut bytes = vec![0x70, 0, 0, 0x70, 0, 0, 0, 0, 0x2a];
        bytes.extend_from_slice(&((xml.len() / 2) as i32).to_le_bytes());
        bytes.extend_from_slice(&xml);

        let id = utf16le("Mem1");
        bytes.extend_from_slice(&(0x70_i32).to_le_bytes());
        bytes.extend_from_slice(&0_i32.to_le_bytes());
        bytes.push(0x2a);
        bytes.extend_from_slice(&192_i32.to_le_bytes());
        bytes.push(0x2a);
        bytes.extend_from_slice(&((id.len() / 2) as i32).to_le_bytes());
        bytes.extend_from_slice(&id);
        let mut payload = vec![0u8; 192];
        for t in 0..2usize {
            for z in 0..2usize {
                for c in 0..2usize {
                    let base = t * 96 + z * 48 + c * 24;
                    for y in 0..3usize {
                        for x in 0..4usize {
                            let p = base + y * 8 + x * 2;
                            let value = (t * 100 + z * 40 + c * 20 + y * 4 + x) as u16;
                            payload[p..p + 2].copy_from_slice(&value.to_le_bytes());
                        }
                    }
                }
            }
        }
        bytes.extend_from_slice(&payload);
        bytes
    }

    fn synthetic_rgb_lif_bytes() -> Vec<u8> {
        let xml = r#"<LMSDataContainerHeader><Element Name="Experiment"><Element Name="RGB Scan"><Memory MemoryBlockID="RgbMem"/><Data><Image Name="RGB Image"><ImageDescription><Channels><ChannelDescription BytesInc="0"/><ChannelDescription BytesInc="1"/><ChannelDescription BytesInc="2"/></Channels><Dimensions><DimensionDescription DimID="1" NumberOfElements="2" BytesInc="3"/><DimensionDescription DimID="2" NumberOfElements="2" BytesInc="6"/><DimensionDescription DimID="3" NumberOfElements="2" BytesInc="12"/></Dimensions></ImageDescription></Image></Data></Element></Element></LMSDataContainerHeader>"#;
        let xml = utf16le(xml);

        let mut bytes = vec![0x70, 0, 0, 0x70, 0, 0, 0, 0, 0x2a];
        bytes.extend_from_slice(&((xml.len() / 2) as i32).to_le_bytes());
        bytes.extend_from_slice(&xml);

        let id = utf16le("RgbMem");
        bytes.extend_from_slice(&(0x70_i32).to_le_bytes());
        bytes.extend_from_slice(&0_i32.to_le_bytes());
        bytes.push(0x2a);
        bytes.extend_from_slice(&24_i32.to_le_bytes());
        bytes.push(0x2a);
        bytes.extend_from_slice(&((id.len() / 2) as i32).to_le_bytes());
        bytes.extend_from_slice(&id);
        bytes.extend_from_slice(&[
            1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 101, 102, 103, 104, 105, 106, 107, 108, 109,
            110, 111, 112,
        ]);
        bytes
    }

    fn synthetic_planar_rgb_lif_bytes(channel_offsets: [u64; 3]) -> Vec<u8> {
        let xml = format!(
            r#"<LMSDataContainerHeader><Element Name="Experiment"><Element Name="Planar RGB Scan"><Memory MemoryBlockID="PlanarRgbMem"/><Data><Image Name="Planar RGB Image"><ImageDescription><Channels><ChannelDescription BytesInc="{}"/><ChannelDescription BytesInc="{}"/><ChannelDescription BytesInc="{}"/></Channels><Dimensions><DimensionDescription DimID="1" NumberOfElements="2" BytesInc="3"/><DimensionDescription DimID="2" NumberOfElements="2" BytesInc="2"/><DimensionDescription DimID="3" NumberOfElements="2" BytesInc="12"/></Dimensions></ImageDescription></Image></Data></Element></Element></LMSDataContainerHeader>"#,
            channel_offsets[0], channel_offsets[1], channel_offsets[2]
        );
        let xml = utf16le(&xml);

        let mut bytes = vec![0x70, 0, 0, 0x70, 0, 0, 0, 0, 0x2a];
        bytes.extend_from_slice(&((xml.len() / 2) as i32).to_le_bytes());
        bytes.extend_from_slice(&xml);

        let id = utf16le("PlanarRgbMem");
        bytes.extend_from_slice(&(0x70_i32).to_le_bytes());
        bytes.extend_from_slice(&0_i32.to_le_bytes());
        bytes.push(0x2a);
        bytes.extend_from_slice(&24_i32.to_le_bytes());
        bytes.push(0x2a);
        bytes.extend_from_slice(&((id.len() / 2) as i32).to_le_bytes());
        bytes.extend_from_slice(&id);
        bytes.extend_from_slice(&[
            1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 101, 102, 103, 104, 105, 106, 107, 108, 109,
            110, 111, 112,
        ]);
        bytes
    }

    fn synthetic_two_rgb_group_lif_bytes(channel_offsets: [u64; 6], y_stride: u64) -> Vec<u8> {
        let xml = format!(
            r#"<LMSDataContainerHeader><Element Name="Experiment"><Element Name="Two RGB Scan"><Memory MemoryBlockID="TwoRgbMem"/><Data><Image Name="Two RGB Image"><ImageDescription><Channels><ChannelDescription BytesInc="{}"/><ChannelDescription BytesInc="{}"/><ChannelDescription BytesInc="{}"/><ChannelDescription BytesInc="{}"/><ChannelDescription BytesInc="{}"/><ChannelDescription BytesInc="{}"/></Channels><Dimensions><DimensionDescription DimID="1" NumberOfElements="2" BytesInc="3"/><DimensionDescription DimID="2" NumberOfElements="2" BytesInc="{y_stride}"/></Dimensions></ImageDescription></Image></Data></Element></Element></LMSDataContainerHeader>"#,
            channel_offsets[0],
            channel_offsets[1],
            channel_offsets[2],
            channel_offsets[3],
            channel_offsets[4],
            channel_offsets[5]
        );
        let xml = utf16le(&xml);

        let mut bytes = vec![0x70, 0, 0, 0x70, 0, 0, 0, 0, 0x2a];
        bytes.extend_from_slice(&((xml.len() / 2) as i32).to_le_bytes());
        bytes.extend_from_slice(&xml);

        let id = utf16le("TwoRgbMem");
        bytes.extend_from_slice(&(0x70_i32).to_le_bytes());
        bytes.extend_from_slice(&0_i32.to_le_bytes());
        bytes.push(0x2a);
        bytes.extend_from_slice(&24_i32.to_le_bytes());
        bytes.push(0x2a);
        bytes.extend_from_slice(&((id.len() / 2) as i32).to_le_bytes());
        bytes.extend_from_slice(&id);
        bytes.extend_from_slice(&[
            1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 101, 102, 103, 104, 105, 106, 107, 108, 109,
            110, 111, 112,
        ]);
        bytes
    }

    fn synthetic_compressed_planar_rgb_lif_bytes(payload: &[u8]) -> Vec<u8> {
        let xml = r#"<LMSDataContainerHeader><Element Name="Experiment"><Element Name="Compressed Planar RGB Scan"><Memory MemoryBlockID="CompressedPlanarRgbMem" Compression="zlib"/><Data><Image Name="Compressed Planar RGB Image"><ImageDescription><Channels><ChannelDescription BytesInc="0"/><ChannelDescription BytesInc="4"/><ChannelDescription BytesInc="8"/></Channels><Dimensions><DimensionDescription DimID="1" NumberOfElements="2" BytesInc="3"/><DimensionDescription DimID="2" NumberOfElements="2" BytesInc="2"/><DimensionDescription DimID="3" NumberOfElements="2" BytesInc="12"/></Dimensions></ImageDescription></Image></Data></Element></Element></LMSDataContainerHeader>"#;
        let xml = utf16le(xml);

        let mut bytes = vec![0x70, 0, 0, 0x70, 0, 0, 0, 0, 0x2a];
        bytes.extend_from_slice(&((xml.len() / 2) as i32).to_le_bytes());
        bytes.extend_from_slice(&xml);

        let id = utf16le("CompressedPlanarRgbMem");
        bytes.extend_from_slice(&(0x70_i32).to_le_bytes());
        bytes.extend_from_slice(&0_i32.to_le_bytes());
        bytes.push(0x2a);
        bytes.extend_from_slice(&(payload.len() as i32).to_le_bytes());
        bytes.push(0x2a);
        bytes.extend_from_slice(&((id.len() / 2) as i32).to_le_bytes());
        bytes.extend_from_slice(&id);
        bytes.extend_from_slice(payload);
        bytes
    }

    fn synthetic_compressed_lif_bytes(compression: &str, payload: &[u8]) -> Vec<u8> {
        let xml = format!(
            r#"<LMSDataContainerHeader><Element Name="Experiment"><Element Name="Compressed Scan"><Memory MemoryBlockID="ZipMem" Compression="{compression}"/><Data><Image Name="Compressed Image"><ImageDescription><Channels><ChannelDescription BytesInc="0"/></Channels><Dimensions><DimensionDescription DimID="1" NumberOfElements="2" BytesInc="1"/><DimensionDescription DimID="2" NumberOfElements="2" BytesInc="2"/></Dimensions></ImageDescription></Image></Data></Element></Element></LMSDataContainerHeader>"#
        );
        let xml = utf16le(&xml);

        let mut bytes = vec![0x70, 0, 0, 0x70, 0, 0, 0, 0, 0x2a];
        bytes.extend_from_slice(&((xml.len() / 2) as i32).to_le_bytes());
        bytes.extend_from_slice(&xml);

        let id = utf16le("ZipMem");
        bytes.extend_from_slice(&(0x70_i32).to_le_bytes());
        bytes.extend_from_slice(&0_i32.to_le_bytes());
        bytes.push(0x2a);
        bytes.extend_from_slice(&(payload.len() as i32).to_le_bytes());
        bytes.push(0x2a);
        bytes.extend_from_slice(&((id.len() / 2) as i32).to_le_bytes());
        bytes.extend_from_slice(&id);
        bytes.extend_from_slice(payload);
        bytes
    }

    fn deflate_stored(raw: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut chunks = raw.chunks(u16::MAX as usize).peekable();
        while let Some(chunk) = chunks.next() {
            out.push(if chunks.peek().is_none() { 0x01 } else { 0x00 });
            let len = chunk.len() as u16;
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&(!len).to_le_bytes());
            out.extend_from_slice(chunk);
        }
        out
    }

    fn zlib_stored(raw: &[u8]) -> Vec<u8> {
        let mut out = vec![0x78, 0x01];
        out.extend_from_slice(&deflate_stored(raw));
        let mut a: u32 = 1;
        let mut b: u32 = 0;
        for &byte in raw {
            a = (a + u32::from(byte)) % 65521;
            b = (b + a) % 65521;
        }
        out.extend_from_slice(&((b << 16) | a).to_be_bytes());
        out
    }

    fn synthetic_tiled_lif_bytes(include_tile_stride: bool) -> Vec<u8> {
        let tile_dim = if include_tile_stride {
            r#"<DimensionDescription DimID="10" NumberOfElements="2" BytesInc="4"/>"#
        } else {
            r#"<DimensionDescription DimID="10" NumberOfElements="2"/>"#
        };
        let xml = format!(
            r#"<LMSDataContainerHeader><Element Name="Experiment"><Element Name="Tile Scan"><Memory MemoryBlockID="TileMem"/><Data><Image Name="Tile Image"><ImageDescription><Channels><ChannelDescription BytesInc="0"/></Channels><Dimensions><DimensionDescription DimID="1" NumberOfElements="2" BytesInc="1"/><DimensionDescription DimID="2" NumberOfElements="2" BytesInc="2"/>{tile_dim}</Dimensions></ImageDescription></Image></Data></Element></Element></LMSDataContainerHeader>"#
        );
        let xml = utf16le(&xml);

        let mut bytes = vec![0x70, 0, 0, 0x70, 0, 0, 0, 0, 0x2a];
        bytes.extend_from_slice(&((xml.len() / 2) as i32).to_le_bytes());
        bytes.extend_from_slice(&xml);

        let id = utf16le("TileMem");
        bytes.extend_from_slice(&(0x70_i32).to_le_bytes());
        bytes.extend_from_slice(&0_i32.to_le_bytes());
        bytes.push(0x2a);
        bytes.extend_from_slice(&8_i32.to_le_bytes());
        bytes.push(0x2a);
        bytes.extend_from_slice(&((id.len() / 2) as i32).to_le_bytes());
        bytes.extend_from_slice(&id);
        bytes.extend_from_slice(&[1, 2, 3, 4, 11, 12, 13, 14]);
        bytes
    }

    fn assert_close(actual: Option<f64>, expected: f64) {
        let actual = actual.expect("physical size");
        assert!(
            (actual - expected).abs() < 1e-9,
            "expected {expected}, got {actual}"
        );
    }

    #[test]
    fn rejects_non_lif_bytes() {
        let reader = LifReader::new();
        assert!(!reader.is_this_type_by_bytes(b"not a real lif file at all!!"));
    }

    #[test]
    fn detects_lif_extension() {
        let reader = LifReader::new();
        assert!(reader.is_this_type_by_name(std::path::Path::new("foo.lif")));
        assert!(reader.is_this_type_by_name(std::path::Path::new("FOO.LIF")));
        assert!(!reader.is_this_type_by_name(std::path::Path::new("foo.tif")));
    }

    #[test]
    fn set_id_fails_cleanly_on_garbage() {
        let path = std::env::temp_dir().join("bioformats_lif_garbage.lif");
        std::fs::write(&path, b"not a real lif").unwrap();
        let mut reader = LifReader::new();
        assert!(reader.set_id(&path).is_err());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn parses_container_xml_metadata_and_reads_uncompressed_pixels() {
        let bytes = synthetic_lif_bytes();
        let path = temp_lif_path("metadata");
        std::fs::write(&path, &bytes).unwrap();

        let mut reader = LifReader::new();
        assert!(reader.is_this_type_by_bytes(&bytes));
        reader.set_id(&path).unwrap();

        assert_eq!(reader.series_count(), 1);
        let meta = reader.metadata();
        assert_eq!(meta.size_x, 4);
        assert_eq!(meta.size_y, 3);
        assert_eq!(meta.size_z, 2);
        assert_eq!(meta.size_c, 2);
        assert_eq!(meta.size_t, 2);
        assert_eq!(meta.image_count, 8);
        assert_eq!(meta.pixel_type, PixelType::Uint16);
        assert_eq!(meta.bits_per_pixel, 16);
        assert!(!meta.is_rgb);
        assert!(meta.is_indexed);
        assert!(meta.is_little_endian);

        let ome = reader.ome_metadata().expect("OME metadata");
        assert_eq!(ome.images.len(), 1);
        assert_eq!(ome.images[0].name.as_deref(), Some("Scan"));
        assert_close(ome.images[0].physical_size_x, 1.0);
        assert_close(ome.images[0].physical_size_y, 1.0);
        assert_close(ome.images[0].physical_size_z, 4.0);
        assert_eq!(ome.images[0].channels.len(), 2);
        assert_eq!(ome.images[0].channels[0].name.as_deref(), Some("DAPI"));
        assert_eq!(ome.images[0].channels[1].name.as_deref(), Some("FITC"));

        assert_eq!(
            reader.open_bytes(0).unwrap(),
            [0, 0, 1, 0, 2, 0, 3, 0, 4, 0, 5, 0, 6, 0, 7, 0, 8, 0, 9, 0, 10, 0, 11, 0]
        );
        assert_eq!(
            reader.open_bytes_region(5, 1, 1, 2, 2).unwrap(),
            [125, 0, 126, 0, 129, 0, 130, 0]
        );
        assert!(matches!(
            reader.open_bytes(99),
            Err(BioFormatsError::PlaneOutOfRange(99))
        ));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn reports_precise_error_for_truncated_pixel_block() {
        let mut bytes = synthetic_lif_bytes();
        bytes.truncate(bytes.len() - 192);
        let path = temp_lif_path("missing_block");
        std::fs::write(&path, &bytes).unwrap();

        let mut reader = LifReader::new();
        reader.set_id(&path).unwrap();
        let err = reader.open_bytes(0).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::UnsupportedFormat(ref message)
                if message.contains("exceeds memory block") && message.contains("Mem1")),
            "{err:?}"
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn reads_zlib_compressed_lif_payload() {
        let bytes = synthetic_compressed_lif_bytes("zlib", &zlib_stored(&[1, 2, 3, 4]));
        let path = temp_lif_path("compressed");
        std::fs::write(&path, &bytes).unwrap();

        let mut reader = LifReader::new();
        reader.set_id(&path).unwrap();

        let meta = reader.metadata();
        assert!(matches!(
            meta.series_metadata.get("lif.compressed"),
            Some(MetadataValue::Bool(true))
        ));
        assert!(matches!(
            meta.series_metadata.get("lif.compression"),
            Some(MetadataValue::String(value)) if value == "Compression=zlib"
        ));

        assert_eq!(reader.open_bytes(0).unwrap(), [1, 2, 3, 4]);
        assert_eq!(reader.open_bytes_region(0, 1, 0, 1, 2).unwrap(), [2, 4]);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn reads_raw_deflate_compressed_lif_payload() {
        let bytes = synthetic_compressed_lif_bytes("deflate", &deflate_stored(&[5, 6, 7, 8]));
        let path = temp_lif_path("deflate");
        std::fs::write(&path, &bytes).unwrap();

        let mut reader = LifReader::new();
        reader.set_id(&path).unwrap();

        assert_eq!(reader.open_bytes(0).unwrap(), [5, 6, 7, 8]);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn rejects_unknown_compressed_lif_payload_hint() {
        let bytes = synthetic_compressed_lif_bytes("LeicaMagic", &[1, 2, 3]);
        let path = temp_lif_path("unknown_compressed");
        std::fs::write(&path, &bytes).unwrap();

        let mut reader = LifReader::new();
        reader.set_id(&path).unwrap();

        let err = reader.open_bytes(0).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::UnsupportedFormat(ref message)
                if message.contains("unsupported compression hint")
                    && message.contains("Compression=LeicaMagic")
                    && message.contains("zlib/deflate")),
            "{err:?}"
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn expands_tiled_uncompressed_lif_using_declared_tile_stride() {
        let bytes = synthetic_tiled_lif_bytes(true);
        let path = temp_lif_path("tiled");
        std::fs::write(&path, &bytes).unwrap();

        let mut reader = LifReader::new();
        reader.set_id(&path).unwrap();

        assert_eq!(reader.series_count(), 2);
        assert!(matches!(
            reader.metadata().series_metadata.get("lif.tile_count"),
            Some(MetadataValue::Int(2))
        ));
        assert!(matches!(
            reader.metadata().series_metadata.get("lif.tile_bytes_inc"),
            Some(MetadataValue::Int(4))
        ));
        assert_eq!(reader.open_bytes(0).unwrap(), [1, 2, 3, 4]);

        reader.set_series(1).unwrap();
        assert_eq!(reader.open_bytes(0).unwrap(), [11, 12, 13, 14]);
        assert_eq!(reader.open_bytes_region(0, 1, 0, 1, 2).unwrap(), [12, 14]);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn rejects_later_tiled_series_without_declared_tile_stride() {
        let bytes = synthetic_tiled_lif_bytes(false);
        let path = temp_lif_path("tiled_missing_stride");
        std::fs::write(&path, &bytes).unwrap();

        let mut reader = LifReader::new();
        reader.set_id(&path).unwrap();

        assert_eq!(reader.series_count(), 2);
        assert_eq!(reader.open_bytes(0).unwrap(), [1, 2, 3, 4]);

        reader.set_series(1).unwrap();
        let err = reader.open_bytes(0).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::UnsupportedFormat(ref message)
                if message.contains("missing tile byte stride")
                    && message.contains("2 tiles")),
            "{err:?}"
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn reads_uncompressed_interleaved_rgb_pixels() {
        let bytes = synthetic_rgb_lif_bytes();
        let path = temp_lif_path("rgb");
        std::fs::write(&path, &bytes).unwrap();

        let mut reader = LifReader::new();
        reader.set_id(&path).unwrap();

        let meta = reader.metadata();
        assert_eq!(meta.size_x, 2);
        assert_eq!(meta.size_y, 2);
        assert_eq!(meta.size_z, 2);
        assert_eq!(meta.size_c, 3);
        assert_eq!(meta.image_count, 2);
        assert!(meta.is_rgb);
        assert!(meta.is_interleaved);
        assert!(!meta.is_indexed);

        let ome = reader.ome_metadata().expect("OME metadata");
        assert_eq!(ome.images[0].channels.len(), 1);
        assert_eq!(ome.images[0].channels[0].samples_per_pixel, 3);

        assert_eq!(
            reader.open_bytes(0).unwrap(),
            [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]
        );
        assert_eq!(
            reader.open_bytes_region(0, 1, 0, 1, 2).unwrap(),
            [4, 5, 6, 10, 11, 12]
        );
        assert_eq!(
            reader.open_bytes(1).unwrap(),
            [101, 102, 103, 104, 105, 106, 107, 108, 109, 110, 111, 112]
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn reads_uncompressed_planar_rgb_pixels_from_channel_strides() {
        let bytes = synthetic_planar_rgb_lif_bytes([0, 4, 8]);
        let path = temp_lif_path("planar_rgb");
        std::fs::write(&path, &bytes).unwrap();

        let mut reader = LifReader::new();
        reader.set_id(&path).unwrap();

        let meta = reader.metadata();
        assert_eq!(meta.size_x, 2);
        assert_eq!(meta.size_y, 2);
        assert_eq!(meta.size_z, 2);
        assert_eq!(meta.size_c, 3);
        assert_eq!(meta.image_count, 2);
        assert!(meta.is_rgb);
        assert!(!meta.is_interleaved);
        assert!(!meta.is_indexed);

        let ome = reader.ome_metadata().expect("OME metadata");
        assert_eq!(ome.images[0].channels.len(), 1);
        assert_eq!(ome.images[0].channels[0].samples_per_pixel, 3);

        assert_eq!(
            reader.open_bytes(0).unwrap(),
            [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]
        );
        assert_eq!(
            reader.open_bytes_region(0, 1, 0, 1, 2).unwrap(),
            [2, 4, 6, 8, 10, 12]
        );
        assert_eq!(
            reader.open_bytes(1).unwrap(),
            [101, 102, 103, 104, 105, 106, 107, 108, 109, 110, 111, 112]
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn reads_zlib_compressed_planar_rgb_pixels_from_channel_strides() {
        let raw = [
            1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 101, 102, 103, 104, 105, 106, 107, 108, 109,
            110, 111, 112,
        ];
        let bytes = synthetic_compressed_planar_rgb_lif_bytes(&zlib_stored(&raw));
        let path = temp_lif_path("compressed_planar_rgb");
        std::fs::write(&path, &bytes).unwrap();

        let mut reader = LifReader::new();
        reader.set_id(&path).unwrap();

        let meta = reader.metadata();
        assert!(meta.is_rgb);
        assert!(!meta.is_interleaved);
        assert!(matches!(
            meta.series_metadata.get("lif.compression"),
            Some(MetadataValue::String(value)) if value == "Compression=zlib"
        ));
        assert_eq!(
            reader.open_bytes(0).unwrap(),
            [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]
        );
        assert_eq!(
            reader.open_bytes_region(0, 1, 0, 1, 2).unwrap(),
            [2, 4, 6, 8, 10, 12]
        );
        assert_eq!(
            reader.open_bytes(1).unwrap(),
            [101, 102, 103, 104, 105, 106, 107, 108, 109, 110, 111, 112]
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn reads_two_interleaved_rgb_groups_from_repeated_channel_triples() {
        let bytes = synthetic_two_rgb_group_lif_bytes([0, 1, 2, 12, 13, 14], 6);
        let path = temp_lif_path("two_rgb_interleaved");
        std::fs::write(&path, &bytes).unwrap();

        let mut reader = LifReader::new();
        reader.set_id(&path).unwrap();

        let meta = reader.metadata();
        assert_eq!(meta.size_c, 6);
        assert_eq!(meta.image_count, 2);
        assert!(meta.is_rgb);
        assert!(meta.is_interleaved);
        assert!(matches!(
            meta.series_metadata.get("lif.rgb_samples_per_pixel"),
            Some(MetadataValue::Int(3))
        ));
        assert!(matches!(
            meta.series_metadata.get("lif.effective_size_c"),
            Some(MetadataValue::Int(2))
        ));

        let ome = reader.ome_metadata().expect("OME metadata");
        assert_eq!(ome.images[0].channels.len(), 2);
        assert_eq!(ome.images[0].channels[0].samples_per_pixel, 3);
        assert_eq!(ome.images[0].channels[1].samples_per_pixel, 3);

        assert_eq!(
            reader.open_bytes(0).unwrap(),
            [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]
        );
        assert_eq!(
            reader.open_bytes(1).unwrap(),
            [101, 102, 103, 104, 105, 106, 107, 108, 109, 110, 111, 112]
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn reads_two_planar_rgb_groups_from_repeated_channel_triples() {
        let bytes = synthetic_two_rgb_group_lif_bytes([0, 4, 8, 12, 16, 20], 2);
        let path = temp_lif_path("two_rgb_planar");
        std::fs::write(&path, &bytes).unwrap();

        let mut reader = LifReader::new();
        reader.set_id(&path).unwrap();

        let meta = reader.metadata();
        assert_eq!(meta.size_c, 6);
        assert_eq!(meta.image_count, 2);
        assert!(meta.is_rgb);
        assert!(!meta.is_interleaved);

        assert_eq!(
            reader.open_bytes(0).unwrap(),
            [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]
        );
        assert_eq!(
            reader.open_bytes_region(1, 1, 0, 1, 2).unwrap(),
            [102, 104, 106, 108, 110, 112]
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn rejects_unknown_rgb_channel_stride_layout() {
        let bytes = synthetic_planar_rgb_lif_bytes([0, 5, 10]);
        let path = temp_lif_path("unknown_rgb_stride");
        std::fs::write(&path, &bytes).unwrap();

        let mut reader = LifReader::new();
        reader.set_id(&path).unwrap();

        let err = reader.open_bytes(0).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::UnsupportedFormat(ref message)
                if message.contains("unsupported RGB layout")
                    && message.contains("channel offsets")),
            "{err:?}"
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn reads_local_pr2729_fixture_when_available() {
        let path = std::path::Path::new("testdata/lif/PR2729.lif");
        if !path.exists() {
            return;
        }

        let mut reader = LifReader::new();
        reader.set_id(path).unwrap();
        assert_eq!(reader.series_count(), 4);
        let meta = reader.metadata();
        assert_eq!(
            (
                meta.size_x,
                meta.size_y,
                meta.size_z,
                meta.size_c,
                meta.size_t
            ),
            (64, 64, 3, 2, 2)
        );
        assert_eq!(reader.open_bytes(0).unwrap().len(), 4096);
        assert_eq!(reader.open_bytes_region(11, 4, 5, 7, 3).unwrap().len(), 21);
    }
}
