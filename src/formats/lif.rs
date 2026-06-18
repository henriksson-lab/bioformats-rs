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
//! non-RGB strided layout confirmed by local fixtures and color payloads whose
//! XML strides describe repeated interleaved, planar, or ordered padded-planar
//! sample groups; other payload variants return precise `UnsupportedFormat`
//! errors.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use quick_xml::events::Event;

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::ome_metadata::{
    create_lsid, OmeChannel, OmeDetector, OmeFilter, OmeImage, OmeInstrument, OmeLightSource,
    OmeMetadata, OmeObjective, OmePlane, OmeROI, OmeShape,
};
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
    /// Instrument (objective, detectors, lasers, filters) for this series,
    /// mirroring the per-series `<Instrument>` Java builds in `initMetadata`.
    instrument: OmeInstrument,
    /// Regions of interest parsed from `<Annotation>` / `<ROISingle>` nodes.
    rois: Vec<OmeROI>,
    layout: PixelLayout,
    /// Per-channel laser index for the channel's `<LightSourceSettings>` (Java
    /// `laser` in `setChannelLightSourceSettingsID`). `None` means no light
    /// source settings were assigned to that channel. Used by `ome_metadata`
    /// to build the series-qualified `LightSource:i:laser` LSID.
    light_source_settings_laser: Vec<Option<usize>>,
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
        let (mut series, ordered_ids) = parse_xml(&xml)?;
        if series.is_empty() {
            return Err(BioFormatsError::Format("No images found in LIF".into()));
        }

        // Match memory blocks to image elements by ID, preserving the XML
        // order. Fall back to file order if IDs do not match.
        let mut matched: Vec<MemoryBlock> = Vec::new();
        for id in &ordered_ids {
            if let Some(b) = raw_blocks.iter().find(|b| &b.id == id) {
                matched.push(b.clone());
            }
        }
        let matched_by_id = matched.len() == ordered_ids.len() && !matched.is_empty();
        self.memory_blocks = if matched_by_id { matched } else { raw_blocks };
        annotate_lif_storage(
            &mut series,
            &ordered_ids,
            &self.memory_blocks,
            matched_by_id,
        );
        annotate_lif_compression_payloads(&mut series, &self.memory_blocks, data);
        self.series = series;

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
        let block = match self.memory_blocks.get(group) {
            Some(block) => block,
            None => return blank_lif_region(m, &info.layout, w, h),
        };
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
        // Each series owns its own instrument and ROI set; collect them globally
        // and link each image to its instrument and ROIs by index, mirroring the
        // per-series Instrument/ROI the Java reader registers in `initMetadata`.
        let mut images: Vec<OmeImage> = Vec::with_capacity(self.series.len());
        let mut instruments: Vec<OmeInstrument> = Vec::new();
        let mut rois: Vec<OmeROI> = Vec::new();
        for (i, s) in self.series.iter().enumerate() {
            let mut image = s.ome.clone();
            // Finalize each channel's <LightSourceSettings> ID now that the
            // series index `i` is known (Java MetadataTools.createLSID(
            // "LightSource", i, laser)).
            for (c, channel) in image.channels.iter_mut().enumerate() {
                if let Some(Some(laser)) = s.light_source_settings_laser.get(c) {
                    channel.light_source_settings_id =
                        Some(create_lsid("LightSource", &[i, *laser]));
                }
            }
            let instrument_id = create_lsid("Instrument", &[i]);
            let mut instrument = s.instrument.clone();
            instrument.id = Some(instrument_id.clone());
            image.instrument_ref = Some(instruments.len());
            instruments.push(instrument);
            for roi in &s.rois {
                let mut roi = roi.clone();
                roi.id = Some(create_lsid("ROI", &[rois.len()]));
                rois.push(roi);
            }
            images.push(image);
        }
        Some(OmeMetadata {
            images,
            instruments,
            rois,
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
        LifPlaneLayout::InterleavedColor => rgb_samples as u64,
        LifPlaneLayout::Scalar
        | LifPlaneLayout::PlanarColor
        | LifPlaneLayout::PaddedPlanarColor => 1,
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

    if matches!(
        plane_layout,
        LifPlaneLayout::PlanarColor | LifPlaneLayout::PaddedPlanarColor
    ) {
        return decode_lif_planar_color_region(
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
    let rgb_group_base = if matches!(plane_layout, LifPlaneLayout::InterleavedColor) {
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
        // color planes are stored in BGR order (Java openBytes); only for
        // three-sample RGB groups, matching the getRGBChannelCount() == 3 gate.
        if matches!(plane_layout, LifPlaneLayout::InterleavedColor) && rgb_samples == 3 {
            bgr_to_rgb(&mut out, true, bps as usize, rgb_samples as usize);
        }
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
            return blank_lif_region(meta, layout, w, h);
        }
        out.extend_from_slice(&data[abs_src as usize..abs_end as usize]);
    }
    // color planes are stored in BGR order (Java openBytes); only for
    // three-sample RGB groups, matching the getRGBChannelCount() == 3 gate.
    if matches!(plane_layout, LifPlaneLayout::InterleavedColor) && rgb_samples == 3 {
        bgr_to_rgb(&mut out, true, bps as usize, rgb_samples as usize);
    }
    Ok(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LifPlaneLayout {
    Scalar,
    InterleavedColor,
    PlanarColor,
    PaddedPlanarColor,
}

fn lif_plane_layout(meta: &ImageMetadata, layout: &PixelLayout) -> Result<LifPlaneLayout> {
    if !meta.is_rgb {
        return Ok(LifPlaneLayout::Scalar);
    }
    let rgb_samples = lif_rgb_channel_count(meta);
    if rgb_samples < 2
        || meta.size_c < rgb_samples
        || meta.size_c % rgb_samples != 0
        || layout.channel_offsets.len() != meta.size_c as usize
    {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "Leica LIF unsupported color layout: interleaved={}, samples per pixel={}, channel count={}, channel offsets={:?}",
            meta.is_interleaved, rgb_samples, meta.size_c, layout.channel_offsets
        )));
    }

    let bps = meta.pixel_type.bytes_per_sample() as u64;
    if meta.is_interleaved {
        for group in layout.channel_offsets.chunks_exact(rgb_samples as usize) {
            for sample in 1..rgb_samples as usize {
                let expected = checked_add_u64(
                    group[0],
                    checked_mul_u64(bps, sample as u64, "Leica LIF color sample offset")?,
                    "Leica LIF color sample offset",
                )?;
                if group[sample] != expected {
                    return Err(BioFormatsError::UnsupportedFormat(format!(
                        "Leica LIF unsupported irregular/non-contiguous color layout: interleaved={}, samples per pixel={}, channel offsets={:?}, expected repeated contiguous sample groups",
                        meta.is_interleaved, rgb_samples, layout.channel_offsets
                    )));
                }
            }
        }
        return Ok(LifPlaneLayout::InterleavedColor);
    } else if layout.x_stride == bps {
        let plane_stride = checked_mul_u64(
            layout.y_stride,
            u64::from(meta.size_y),
            "Leica LIF color plane stride",
        )?;
        let mut is_contiguous_planar = true;
        for group in layout.channel_offsets.chunks_exact(rgb_samples as usize) {
            for sample in 1..rgb_samples as usize {
                let expected = checked_add_u64(
                    group[0],
                    checked_mul_u64(plane_stride, sample as u64, "Leica LIF color plane offset")?,
                    "Leica LIF color plane offset",
                )?;
                if group[sample] != expected {
                    is_contiguous_planar = false;
                    break;
                }
            }
            if !is_contiguous_planar {
                break;
            }
        }
        if is_contiguous_planar {
            return Ok(LifPlaneLayout::PlanarColor);
        }
        if is_ordered_non_overlapping_planar_color(meta, layout)? {
            return Ok(LifPlaneLayout::PaddedPlanarColor);
        }
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "Leica LIF unsupported irregular/non-contiguous color layout: interleaved={}, samples per pixel={}, channel offsets={:?}, expected repeated planar sample groups or ordered non-overlapping padded planes",
            meta.is_interleaved, rgb_samples, layout.channel_offsets
        )));
    }

    Err(BioFormatsError::UnsupportedFormat(format!(
        "Leica LIF unsupported irregular/non-contiguous color layout: interleaved={}, samples per pixel={}, channel offsets={:?}",
        meta.is_interleaved, rgb_samples, layout.channel_offsets
    )))
}

fn is_ordered_non_overlapping_planar_color(
    meta: &ImageMetadata,
    layout: &PixelLayout,
) -> Result<bool> {
    if layout.channel_offsets.len() != meta.size_c as usize || layout.x_stride == 0 {
        return Ok(false);
    }
    if !layout
        .channel_offsets
        .windows(2)
        .all(|pair| pair[0] <= pair[1])
    {
        return Ok(false);
    }
    let bps = meta.pixel_type.bytes_per_sample() as u64;
    let row_bytes = checked_mul_u64(u64::from(meta.size_x), bps, "Leica LIF row bytes")?;
    if layout.y_stride < row_bytes {
        return Ok(false);
    }
    let footprint = checked_add_u64(
        checked_mul_u64(
            u64::from(meta.size_y.saturating_sub(1)),
            layout.y_stride,
            "Leica LIF color plane footprint",
        )?,
        row_bytes,
        "Leica LIF color plane footprint",
    )?;
    for pair in layout.channel_offsets.windows(2) {
        let prev_end = checked_add_u64(pair[0], footprint, "Leica LIF color plane end")?;
        if prev_end > pair[1] {
            return Ok(false);
        }
    }
    Ok(true)
}

fn decode_lif_planar_color_region(
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
        // color planes are stored in BGR order (Java openBytes); only for
        // three-sample RGB groups, matching the getRGBChannelCount() == 3 gate.
        if rgb_samples == 3 {
            bgr_to_rgb(&mut out, false, bps as usize, rgb_samples);
        }
        return Ok(out);
    }

    for (src, end) in row_ranges {
        let abs_src = checked_add_u64(block.file_offset, src, "Leica LIF source offset")?;
        let abs_end = checked_add_u64(block.file_offset, end, "Leica LIF source end")?;
        if abs_end > block_end || abs_end as usize > data.len() {
            return blank_lif_region(meta, layout, w, h);
        }
        out.extend_from_slice(&data[abs_src as usize..abs_end as usize]);
    }
    // color planes are stored in BGR order (Java openBytes); only for
    // three-sample RGB groups, matching the getRGBChannelCount() == 3 gate.
    if rgb_samples == 3 {
        bgr_to_rgb(&mut out, false, bps as usize, rgb_samples);
    }
    Ok(out)
}

/// Mirror of Java `ImageTools.bgrToRgb`: Leica stores colour samples in BGR
/// order, so swap the first and third samples to obtain RGB. Buffers with
/// fewer than three samples (`c < 3`) are left untouched, matching Java. For
/// the interleaved case the first and third sample of every pixel are swapped
/// in place; for the planar case the entire first and third sample planes are
/// exchanged. The callers only invoke this for three-sample RGB groups,
/// matching Java `openBytes`' `getRGBChannelCount() == 3` gate.
fn bgr_to_rgb(buf: &mut [u8], interleaved: bool, bpp: usize, c: usize) {
    if c < 3 {
        return;
    }
    if interleaved {
        let pixel = bpp * c;
        if pixel == 0 {
            return;
        }
        let mut i = 0;
        while i + bpp * 2 + bpp <= buf.len() {
            for b in 0..bpp {
                buf.swap(i + b, i + b + bpp * 2);
            }
            i += pixel;
        }
    } else {
        let channel_len = buf.len() / (bpp * c);
        if channel_len == 0 {
            return;
        }
        for k in 0..channel_len {
            buf.swap(k, channel_len * 2 + k);
        }
    }
}

fn blank_lif_region(meta: &ImageMetadata, layout: &PixelLayout, w: u32, h: u32) -> Result<Vec<u8>> {
    let plane_layout = lif_plane_layout(meta, layout)?;
    let bps = meta.pixel_type.bytes_per_sample() as u64;
    let samples = match plane_layout {
        LifPlaneLayout::InterleavedColor
        | LifPlaneLayout::PlanarColor
        | LifPlaneLayout::PaddedPlanarColor => lif_rgb_channel_count(meta) as u64,
        LifPlaneLayout::Scalar => 1,
    };
    let len = checked_mul_u64(u64::from(w), u64::from(h), "Leica LIF blank plane pixels")?;
    let len = checked_mul_u64(len, bps, "Leica LIF blank plane bytes")?;
    let len = checked_mul_u64(len, samples, "Leica LIF blank plane samples")?;
    let len = usize::try_from(len)
        .map_err(|_| BioFormatsError::Format("Leica LIF blank plane is too large".into()))?;
    Ok(vec![0; len])
}

#[derive(Debug, Clone, Copy)]
enum LifCompression {
    Zlib,
    RawDeflate,
    Gzip,
}

impl LifCompression {
    fn metadata_status(self) -> &'static str {
        match self {
            LifCompression::Zlib => "supported_zlib",
            LifCompression::RawDeflate => "supported_raw_deflate",
            LifCompression::Gzip => "supported_gzip",
        }
    }
}

fn copy_lif_compressed_ranges(
    data: &[u8],
    block: &MemoryBlock,
    compression: &str,
    row_ranges: &[(u64, u64)],
    plane_index: u32,
    out: &mut Vec<u8>,
) -> Result<()> {
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
    let kind = lif_compression_kind_for_payload(compression, compressed).ok_or_else(|| {
        BioFormatsError::UnsupportedFormat(format!(
            "Leica LIF compressed pixel payload declares unsupported compression hint: {compression}; payload signature {}; first bytes [{}]; only zlib/deflate/gzip memory blocks are supported",
            lif_payload_signature(compressed),
            lif_payload_first_bytes(compressed)
        ))
    })?;
    stream_lif_compressed_ranges(compressed, kind, row_ranges, &block.id, plane_index, out)
}

fn lif_compression_kind(compression: &str) -> Option<LifCompression> {
    let lower = compression.to_ascii_lowercase();
    let value = compression
        .split_once('=')
        .map(|(_, value)| value)
        .unwrap_or(compression)
        .trim()
        .to_ascii_lowercase();
    let normalized_value: String = value
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    if lower.contains("zlib") {
        Some(LifCompression::Zlib)
    } else if normalized_value.contains("gzip") || normalized_value == "gz" {
        Some(LifCompression::Gzip)
    } else if lower.contains("deflate") {
        Some(LifCompression::RawDeflate)
    } else {
        None
    }
}

fn lif_compression_kind_for_payload(compression: &str, payload: &[u8]) -> Option<LifCompression> {
    lif_compression_kind(compression).or_else(|| {
        if !is_generic_compressed_hint(compression) {
            return None;
        }
        match lif_payload_signature(payload) {
            "zlib stream" => Some(LifCompression::Zlib),
            "gzip stream" => Some(LifCompression::Gzip),
            _ => None,
        }
    })
}

fn is_generic_compressed_hint(compression: &str) -> bool {
    let value = compression
        .split_once('=')
        .map(|(_, value)| value)
        .unwrap_or(compression)
        .trim()
        .to_ascii_lowercase();
    matches!(value.as_str(), "1" | "true" | "yes" | "compressed")
}

fn lif_block_payload<'a>(data: &'a [u8], block: &MemoryBlock) -> Option<&'a [u8]> {
    let start = usize::try_from(block.file_offset).ok()?;
    let len = usize::try_from(block.byte_len).ok()?;
    let end = start.checked_add(len)?;
    data.get(start..end)
}

fn lif_payload_signature(payload: &[u8]) -> &'static str {
    if payload.is_empty() {
        "empty payload"
    } else if payload.starts_with(&[0x1f, 0x8b]) {
        "gzip stream"
    } else if payload.len() >= 2
        && payload[0] & 0x0f == 8
        && payload[0] >> 4 <= 7
        && (((payload[0] as u16) << 8) | payload[1] as u16) % 31 == 0
    {
        "zlib stream"
    } else if payload.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        "Zstandard frame"
    } else if payload.starts_with(&[0xfd, b'7', b'z', b'X', b'Z', 0x00]) {
        "XZ stream"
    } else if payload.starts_with(b"BZh") {
        "bzip2 stream"
    } else if payload.starts_with(&[0x04, 0x22, 0x4d, 0x18]) {
        "LZ4 frame"
    } else {
        "unknown payload"
    }
}

fn lif_payload_first_bytes(payload: &[u8]) -> String {
    payload
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn stream_lif_compressed_ranges(
    compressed: &[u8],
    kind: LifCompression,
    row_ranges: &[(u64, u64)],
    block_id: &str,
    plane_index: u32,
    out: &mut Vec<u8>,
) -> Result<()> {
    use flate2::read::{DeflateDecoder, GzDecoder, ZlibDecoder};

    match kind {
        LifCompression::Zlib => {
            let mut decoder = ZlibDecoder::new(compressed);
            read_lif_compressed_ranges(&mut decoder, row_ranges, block_id, plane_index, out)?;
        }
        LifCompression::RawDeflate => {
            let mut decoder = DeflateDecoder::new(compressed);
            read_lif_compressed_ranges(&mut decoder, row_ranges, block_id, plane_index, out)?;
        }
        LifCompression::Gzip => {
            let mut decoder = GzDecoder::new(compressed);
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
    if !meta.is_rgb {
        return 1;
    }
    let zt = meta.size_z.max(1).saturating_mul(meta.size_t.max(1));
    let effective_c = if zt == 0 { 0 } else { meta.image_count / zt };
    if effective_c > 0 {
        (meta.size_c / effective_c).max(1)
    } else {
        3
    }
}

fn lif_effective_size_c(meta: &ImageMetadata) -> u32 {
    if meta.is_rgb {
        let zt = meta.size_z.max(1).saturating_mul(meta.size_t.max(1));
        if zt > 0 && meta.image_count >= zt {
            (meta.image_count / zt).max(1)
        } else {
            (meta.size_c / lif_rgb_channel_count(meta)).max(1)
        }
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
                "Leica LIF color channel group {c} is not described by channel offsets {:?}",
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
    /// Concatenated text content directly inside this element (mirrors
    /// `getTextContent()`, used by the LAS AF 3.1 `<TimeStampList>` path).
    text: String,
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
                        .normalized_value(quick_xml::XmlVersion::Implicit1_0)
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
                    text: String::new(),
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
                Ok(Event::Text(t)) => {
                    if let Some(&top) = stack.last() {
                        if let Ok(value) = t.decode() {
                            nodes[top].text.push_str(value.as_ref());
                        }
                    }
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
            annotate_lif_compression(&mut info.meta, &compression);
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

fn annotate_lif_compression(meta: &mut ImageMetadata, compression: &str) {
    let (status, diagnostic) = match lif_compression_kind(compression) {
        Some(kind) => (kind.metadata_status(), None),
        None => (
            "unsupported_hint",
            Some(format!(
                "Leica LIF compressed pixel payload declares unsupported compression hint: {compression}; only zlib/deflate/gzip memory blocks are supported"
            )),
        ),
    };
    meta.series_metadata.insert(
        "lif.compression.status".to_string(),
        MetadataValue::String(status.to_string()),
    );
    if let Some(diagnostic) = diagnostic {
        meta.series_metadata.insert(
            "lif.compression.diagnostic".to_string(),
            MetadataValue::String(diagnostic),
        );
    }
}

fn annotate_lif_storage(
    series: &mut [SeriesInfo],
    ordered_ids: &[String],
    memory_blocks: &[MemoryBlock],
    matched_by_id: bool,
) {
    let mut series_index = 0usize;
    for (group_index, requested_id) in ordered_ids.iter().enumerate() {
        if series_index >= series.len() {
            break;
        }
        let tiles = series[series_index].tile_count.max(1) as usize;
        let end = (series_index + tiles).min(series.len());
        let block = memory_blocks.get(group_index);

        for info in &mut series[series_index..end] {
            info.meta.series_metadata.insert(
                "lif.memory_block.requested_id".to_string(),
                MetadataValue::String(requested_id.clone()),
            );

            match block {
                Some(block) => {
                    let status = if matched_by_id {
                        "matched_by_id"
                    } else {
                        "fallback_file_order"
                    };
                    info.meta.series_metadata.insert(
                        "lif.memory_block.status".to_string(),
                        MetadataValue::String(status.to_string()),
                    );
                    info.meta.series_metadata.insert(
                        "lif.memory_block.resolved_id".to_string(),
                        MetadataValue::String(block.id.clone()),
                    );
                    info.meta.series_metadata.insert(
                        "lif.memory_block.file_offset".to_string(),
                        MetadataValue::Int(block.file_offset.min(i64::MAX as u64) as i64),
                    );
                    info.meta.series_metadata.insert(
                        "lif.memory_block.byte_length".to_string(),
                        MetadataValue::Int(block.byte_len.min(i64::MAX as u64) as i64),
                    );
                    if !matched_by_id {
                        info.meta.series_metadata.insert(
                            "lif.memory_block.diagnostic".to_string(),
                            MetadataValue::String(
                                "XML MemoryBlockID entries did not all match file memory block IDs; using file order"
                                    .to_string(),
                            ),
                        );
                    }
                }
                None => {
                    info.meta.series_metadata.insert(
                        "lif.memory_block.status".to_string(),
                        MetadataValue::String("missing".to_string()),
                    );
                    info.meta.series_metadata.insert(
                        "lif.memory_block.diagnostic".to_string(),
                        MetadataValue::String(
                            "No file memory block was available for this XML image".to_string(),
                        ),
                    );
                }
            }
        }

        series_index = end;
    }
}

fn annotate_lif_compression_payloads(
    series: &mut [SeriesInfo],
    memory_blocks: &[MemoryBlock],
    data: &[u8],
) {
    let mut series_index = 0usize;
    let mut group_index = 0usize;
    while series_index < series.len() {
        let tiles = series[series_index].tile_count.max(1) as usize;
        let end = (series_index + tiles).min(series.len());

        if series[series_index].layout.compression.is_some() {
            if let Some(block) = memory_blocks.get(group_index) {
                if let Some(payload) = lif_block_payload(data, block) {
                    let signature = lif_payload_signature(payload);
                    let first_bytes = lif_payload_first_bytes(payload);
                    for info in &mut series[series_index..end] {
                        if let Some(compression) = info.layout.compression.as_deref() {
                            if lif_compression_kind(compression).is_none() {
                                if let Some(kind) =
                                    lif_compression_kind_for_payload(compression, payload)
                                {
                                    info.meta.series_metadata.insert(
                                        "lif.compression.status".to_string(),
                                        MetadataValue::String(format!(
                                            "{}_payload_signature",
                                            kind.metadata_status()
                                        )),
                                    );
                                    info.meta.series_metadata.insert(
                                        "lif.compression.diagnostic".to_string(),
                                        MetadataValue::String(format!(
                                            "Leica LIF compressed pixel payload declares generic/unsupported compression hint {compression}; routing by bounded payload signature {signature}"
                                        )),
                                    );
                                }
                            }
                        }
                        info.meta.series_metadata.insert(
                            "lif.compression.payload_signature".to_string(),
                            MetadataValue::String(signature.to_string()),
                        );
                        info.meta.series_metadata.insert(
                            "lif.compression.payload_first_bytes".to_string(),
                            MetadataValue::String(first_bytes.clone()),
                        );
                    }
                }
            }
        }

        series_index = end;
        group_index += 1;
    }
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
    // lutNames mirrors Java's per-channel LUTName collection, used below to
    // build the realChannel colour mapping.
    let mut bytes_per_axis: BTreeMap<u64, char> = BTreeMap::new();
    let mut channel_offsets: Vec<u64> = Vec::with_capacity(channel_nodes.len());
    let mut lut_names: Vec<String> = Vec::with_capacity(channel_nodes.len());
    let mut c_stride: Option<u64> = None;
    for &ch in &channel_nodes {
        lut_names.push(
            dom.nodes[ch]
                .attrs
                .get("LUTName")
                .cloned()
                .unwrap_or_default(),
        );
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
        && !is_decodable_planar_rgb(
            is_rgb,
            m.size_c,
            pixel_type.bytes_per_sample() as u64,
            size_x,
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

    let effective_size_c = if is_rgb {
        (m.size_c / 3).max(1)
    } else {
        m.size_c.max(1)
    };
    let rgb_channel_count = if is_rgb {
        (m.size_c / effective_size_c).max(1)
    } else {
        1
    };
    m.image_count = size_z * size_t * effective_size_c;
    m.series_metadata.insert(
        "lif.rgb_samples_per_pixel".to_string(),
        MetadataValue::Int(i64::from(rgb_channel_count)),
    );
    m.series_metadata.insert(
        "lif.effective_size_c".to_string(),
        MetadataValue::Int(i64::from(effective_size_c)),
    );

    // Effective channel count (OME channels): one per ChannelDescription for
    // non-RGB, or the RGB group count otherwise.
    let effective_c = effective_size_c.max(1) as usize;
    let ch_names = channel_names(dom, img, effective_c);

    // realChannel mapping (Java initMetadata): map each channel's LUTName to a
    // CHANNEL_PRIORITIES rank, then derive the OME channel colour. The Java
    // `nextLut` counter advances across all series, but because LUT names are
    // appended per image in channel order, each image consumes exactly its own
    // channel LUT names in order — so the mapping is computable locally.
    let real_channel: Vec<i32> = (0..(m.size_c as usize))
        .map(|q| {
            let lut = lut_names
                .get(q)
                .map(|s| s.to_ascii_lowercase())
                .unwrap_or_default();
            channel_priority(&lut)
        })
        .collect();

    let mut channels: Vec<OmeChannel> = ch_names
        .into_iter()
        .enumerate()
        .map(|(c, name)| {
            // Java only sets the channel colour when the image is not stored as
            // RGB (channel colouring is implicit for RGB).
            let color = if !is_rgb {
                real_channel.get(c).map(|code| channel_color(*code))
            } else {
                None
            };
            OmeChannel {
                name,
                samples_per_pixel: rgb_channel_count.max(1),
                color,
                ..OmeChannel::default()
            }
        })
        .collect();

    // -- remaining metadata (mirrors Java translate* + initMetadata store) --
    // Build the per-image accumulator the way Java's translate* methods fill the
    // per-image arrays, then assemble the OME instrument / planes / ROIs below.
    let image_count = m.image_count;
    let mut acc = LifMetaAcc::new(effective_c);
    translate_scanner_settings(dom, img, &mut acc);
    translate_filter_settings(dom, img, &mut acc);
    translate_timestamps(dom, img, image_count as usize, &mut acc);
    translate_laser_lines(dom, img, &mut acc);
    let lif_rois = translate_rois(dom, img, &mut acc, &m, physical_size_x, physical_size_y);
    translate_detectors(dom, img, effective_c, &mut acc);

    // Channel names: Java fills channelNames in scanner/filter/detector passes.
    // The earlier `channel_names()`-based names take priority; fill blanks.
    for (c, channel) in channels.iter_mut().enumerate() {
        if channel.name.is_none() {
            if let Some(Some(name)) = acc.channel_names.get(c) {
                if !name.trim().is_empty() {
                    channel.name = Some(name.clone());
                }
            }
        }
        // Java applies a single pinhole value to every channel.
        if let Some(pinhole) = acc.pinhole {
            channel.pinhole_size = Some(pinhole);
        }
        // WFC...WaveLength excitation wavelengths, per channel.
        if let Some(Some(ex)) = acc.excitation_wavelengths.get(c) {
            if *ex > 1.0 {
                channel.excitation_wavelength = Some(*ex);
            }
        }
        // Detector settings gain/offset, per channel.
        if let Some(Some(gain)) = acc.gains.get(c) {
            channel.detector_settings_gain = Some(*gain);
        }
        if let Some(Some(offset)) = acc.detector_offsets.get(c) {
            channel.detector_settings_offset = Some(*offset);
        }
    }

    // Per-channel laser <LightSourceSettings> (Java initMetadata ~746-893,
    // the live validIntensities algorithm). Fills channel attenuation and
    // returns the per-channel laser index used to build the LSID later.
    let light_source_settings_laser =
        apply_laser_light_source_settings(&acc, effective_c, &mut channels);

    let z_step = acc.z_step;
    let instrument = build_instrument(&acc, effective_c);
    let planes = build_planes(&acc, &m);
    let acquisition_date = acc.acquired_date.filter(|d| *d > 0.0).map(cobol_iso8601);

    // ROI shapes need the series sizeX/sizeY (centre point); convert now that
    // we have the final core dimensions.
    let rois = lif_rois_to_ome(&lif_rois, &acc, m.size_x, m.size_y);

    let ome = OmeImage {
        description: image_description(dom, img),
        acquisition_date,
        physical_size_x: physical_size_x.filter(|v| *v > 0.0),
        physical_size_y: physical_size_y.filter(|v| *v > 0.0),
        physical_size_z: z_step.filter(|v| *v > 0.0).or(physical_size_z.filter(|v| *v > 0.0)),
        time_increment: acc.time_increment,
        channels,
        planes,
        ..OmeImage::default()
    };

    let layout = PixelLayout {
        x_stride,
        y_stride,
        channel_offsets,
        c_stride,
        z_stride,
        t_stride,
        tile_stride: (tile_bytes_inc > 0).then_some(tile_bytes_inc),
        compression: None,
    };
    annotate_lif_color_layout(&mut m, &layout);

    Ok(SeriesInfo {
        meta: m,
        tile_count,
        ome,
        instrument,
        rois,
        layout,
        light_source_settings_laser,
    })
}

/// Port of the live laser `<LightSourceSettings>` block in Java
/// `LIFReader.initMetadata` (~746-893). Builds `validIntensities`, applies the
/// active/two-wavelength `toRemove` dedup and the noNames/FRAP logic, then walks
/// `nextChannel` forward, setting each surviving channel's attenuation
/// (`PercentFraction(intensity / 100)`) and recording its laser index for the
/// `LightSource:i:laser` LSID (built by `ome_metadata`, where the series index
/// `i` is known).
///
/// The channel excitation wavelength (Java ~862-865) is handled separately in
/// `translate_image`, and the emission/filter cross-walk (Java ~867-890) is
/// `//`-commented out upstream (`setLightPathEmissionFilterRef`), so neither is
/// reproduced here.
fn apply_laser_light_source_settings(
    acc: &LifMetaAcc,
    effective_c: usize,
    channels: &mut [OmeChannel],
) -> Vec<Option<usize>> {
    let mut per_channel_laser: Vec<Option<usize>> = vec![None; channels.len()];

    // final List<Double> lasers = laserWavelength[index];  (zero wavelengths
    // removed in place, Java ~755-762)
    let lasers: Vec<f64> = acc
        .laser_wavelength
        .iter()
        .copied()
        .filter(|w| *w != 0.0)
        .collect();
    let laser_intensities = &acc.laser_intensity;
    let active = &acc.laser_active;
    let frap = &acc.laser_frap;

    if lasers.is_empty() {
        return per_channel_laser;
    }

    let size = lasers.len();

    // validIntensities + ignoredChannels (Java ~776-792)
    let mut ignored_channels: HashSet<i64> = HashSet::new();
    let mut valid_intensities: Vec<usize> = Vec::new();
    let mut channels_set: HashSet<i64> = HashSet::new();
    for (laser, &intensity) in laser_intensities.iter().enumerate() {
        let channel = (laser / size) as i64;
        if intensity < 100.0 {
            valid_intensities.push(laser);
            channels_set.insert(channel);
        }
        ignored_channels.insert(channel);
    }
    // ignoredChannels.removeAll(channels);
    ignored_channels.retain(|c| !channels_set.contains(c));

    // remove entries if channel has 2 wavelengths (Java ~793-818)
    let s = valid_intensities.len();
    let mut to_remove: HashSet<usize> = HashSet::new();
    let as_len = active.len();
    for j in 0..s {
        if j < as_len && !active[j] {
            to_remove.insert(valid_intensities[j]);
        }
        let jj = j + 1;
        if jj < s {
            let v = valid_intensities[j] / size;
            let vv = valid_intensities[jj] / size;
            if vv == v {
                // do not consider that channel.
                to_remove.insert(valid_intensities[j]);
                to_remove.insert(valid_intensities[jj]);
                ignored_channels.insert(j as i64);
            }
        }
    }
    if !to_remove.is_empty() {
        valid_intensities.retain(|vi| !to_remove.contains(vi));
    }

    // noNames / FRAP logic (Java ~820-836)
    let mut no_names = true;
    for name in channels_iter_names(acc, effective_c) {
        if let Some(name) = name {
            if !name.is_empty() {
                no_names = false;
                break;
            }
        }
    }
    if !no_names && !frap.is_empty() {
        // only use name for frap.
        for &is_frap in frap {
            if !is_frap {
                no_names = true;
                break;
            }
        }
    }

    // nextChannel-advancement loop (Java ~838-893). The commented-out
    // `nextFilter = cutIns[i].size() - getEffectiveSizeC()` is ignored; the
    // live `int nextFilter = 0;` (unused once the filter cross-walk is dropped).
    let mut next_channel: usize = 0;
    for &laser_array_index in &valid_intensities {
        let intensity = laser_intensities.get(laser_array_index).copied().unwrap_or(0.0);
        let laser = laser_array_index % lasers.len();
        let wavelength = lasers[laser];
        if wavelength != 0.0 {
            while ignored_channels.contains(&(next_channel as i64)) {
                next_channel += 1;
            }
            while next_channel < effective_c && {
                let nm = channel_name_at(acc, next_channel);
                (nm.is_none() || nm.as_deref() == Some("")) && !no_names
            } {
                next_channel += 1;
            }
            if next_channel < effective_c {
                if let Some(channel) = channels.get_mut(next_channel) {
                    // PercentFraction((float) intensity / 100f)
                    channel.light_source_settings_attenuation = Some(intensity / 100.0);
                    per_channel_laser[next_channel] = Some(laser);
                }
            }
        }
        next_channel += 1;
    }

    per_channel_laser
}

/// Channel names as Java's `channelNames[index]` array: prefer the parsed
/// `OmeChannel.name`, falling back to the scanner/filter accumulator names,
/// padded to `getEffectiveSizeC()`.
fn channels_iter_names<'a>(
    acc: &'a LifMetaAcc,
    effective_c: usize,
) -> impl Iterator<Item = Option<&'a str>> + 'a {
    (0..effective_c).map(move |c| {
        acc.channel_names
            .get(c)
            .and_then(|n| n.as_deref())
            .filter(|s| !s.is_empty())
    })
}

fn channel_name_at(acc: &LifMetaAcc, c: usize) -> Option<String> {
    acc.channel_names
        .get(c)
        .and_then(|n| n.clone())
        .filter(|s| !s.is_empty())
}

fn annotate_lif_color_layout(meta: &mut ImageMetadata, layout: &PixelLayout) {
    if !meta.is_rgb {
        return;
    }
    let (status, diagnostic) = match lif_plane_layout(meta, layout) {
        Ok(LifPlaneLayout::InterleavedColor) => ("interleaved_contiguous", None),
        Ok(LifPlaneLayout::PlanarColor) => ("planar_contiguous", None),
        Ok(LifPlaneLayout::PaddedPlanarColor) => ("planar_padded_non_contiguous", None),
        Ok(LifPlaneLayout::Scalar) => ("scalar", None),
        Err(err) => (
            "unsupported_irregular_non_contiguous",
            Some(format!("{err}")),
        ),
    };
    meta.series_metadata.insert(
        "lif.color_layout.status".to_string(),
        MetadataValue::String(status.to_string()),
    );
    if let Some(diagnostic) = diagnostic {
        meta.series_metadata.insert(
            "lif.color_layout.diagnostic".to_string(),
            MetadataValue::String(diagnostic),
        );
    }
}

fn is_decodable_planar_rgb(
    is_rgb: bool,
    size_c: u32,
    bps: u64,
    size_x: u32,
    size_y: u32,
    x_stride: u64,
    y_stride: u64,
    channel_offsets: &[u64],
) -> Result<bool> {
    if !is_rgb || size_c < 3 || channel_offsets.len() != size_c as usize || x_stride != bps {
        return Ok(false);
    }
    let effective_c = (size_c / 3).max(1);
    let samples_per_pixel = (size_c / effective_c).max(1);
    if samples_per_pixel < 2 || size_c % samples_per_pixel != 0 {
        return Ok(false);
    }
    let plane_stride =
        checked_mul_u64(y_stride, u64::from(size_y), "Leica LIF color plane stride")?;
    let mut is_contiguous_planar = true;
    for group in channel_offsets.chunks_exact(samples_per_pixel as usize) {
        for sample in 1..samples_per_pixel as usize {
            let expected = checked_add_u64(
                group[0],
                checked_mul_u64(plane_stride, sample as u64, "Leica LIF color plane offset")?,
                "Leica LIF color plane offset",
            )?;
            if group[sample] != expected {
                is_contiguous_planar = false;
                break;
            }
        }
        if !is_contiguous_planar {
            break;
        }
    }
    if is_contiguous_planar {
        return Ok(true);
    }
    if channel_offsets_are_ordered_non_overlapping(size_x, size_y, bps, y_stride, channel_offsets)?
    {
        return Ok(true);
    }
    Ok(false)
}

fn channel_offsets_are_ordered_non_overlapping(
    size_x: u32,
    size_y: u32,
    bps: u64,
    y_stride: u64,
    channel_offsets: &[u64],
) -> Result<bool> {
    if !channel_offsets.windows(2).all(|pair| pair[0] <= pair[1]) {
        return Ok(false);
    }
    let row_bytes = checked_mul_u64(u64::from(size_x), bps, "Leica LIF row bytes")?;
    if y_stride < row_bytes {
        return Ok(false);
    }
    let footprint = checked_add_u64(
        checked_mul_u64(
            u64::from(size_y.saturating_sub(1)),
            y_stride,
            "Leica LIF color plane footprint",
        )?,
        row_bytes,
        "Leica LIF color plane footprint",
    )?;
    for pair in channel_offsets.windows(2) {
        let prev_end = checked_add_u64(pair[0], footprint, "Leica LIF color plane end")?;
        if prev_end > pair[1] {
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

/// Mirror of Java `LIFReader.CHANNEL_PRIORITIES`: map a (lower-cased) LUT name
/// to its colour priority. Unknown names map to the empty-name priority (8),
/// matching Java's `if (!CHANNEL_PRIORITIES.containsKey(lut)) lut = ""`.
fn channel_priority(lut: &str) -> i32 {
    match lut {
        "red" => 0,
        "green" => 1,
        "blue" => 2,
        "cyan" => 3,
        "magenta" => 4,
        "yellow" => 5,
        "black" => 6,
        "gray" => 7,
        _ => 8,
    }
}

/// Mirror of Java `LIFReader.getChannelColor`: map a CHANNEL_PRIORITIES colour
/// code to a packed RGBA colour (`R<<24 | G<<16 | B<<8 | A`), matching OME
/// `Color.getValue()`. Codes outside 0..=5 fall back to opaque white.
fn channel_color(color_code: i32) -> i32 {
    let (r, g, b, a): (i32, i32, i32, i32) = match color_code {
        0 => (255, 0, 0, 255),   // red
        1 => (0, 255, 0, 255),   // green
        2 => (0, 0, 255, 255),   // blue
        3 => (0, 255, 255, 255), // cyan
        4 => (255, 0, 255, 255), // magenta
        5 => (255, 255, 0, 255), // yellow
        _ => (255, 255, 255, 255),
    };
    (r << 24) | (g << 16) | (b << 8) | a
}

/// Derive the image description, mirroring the attribute-based portion of Java
/// `LIFReader.translateAttachmentNodes`: the `<Attachment Name="ContextDescription">`
/// element's `Content` attribute. (Java's `addUserCommentMeta` text-content
/// fallback is not ported here because the lightweight DOM does not retain
/// element text content.)
fn image_description(dom: &Dom, img: usize) -> Option<String> {
    let mut attachments: Vec<usize> = Vec::new();
    dom.descendants(img, "Attachment", &mut attachments);
    for &att in &attachments {
        if dom.nodes[att].attrs.get("Name").map(String::as_str) == Some("ContextDescription") {
            if let Some(content) = dom.nodes[att].attrs.get("Content") {
                return Some(content.clone());
            }
        }
    }
    None
}

const LIF_METER_MULTIPLY: f64 = 1_000_000.0;

/// One ROI parsed from the LIF XML, mirroring the Java inner `ROI` class.
/// Vertices and the centre-point translation are in physical (metre) units until
/// `normalize()` scales them by `METER_MULTIPLY`.
#[derive(Default, Clone)]
struct LifRoi {
    roi_type: i32,
    x: Vec<f64>,
    y: Vec<f64>,
    trans_x: f64,
    trans_y: f64,
    scale_x: f64,
    scale_y: f64,
    rotation: f64,
    color: i64,
    linewidth: i32,
    text: Option<String>,
    font_size: Option<String>,
    name: Option<String>,
}

/// Per-image accumulator mirroring the per-image LIFReader arrays populated by
/// the `translate*` methods, in the form needed to build OME metadata.
struct LifMetaAcc {
    // scanner settings
    time_increment: Option<f64>,
    pinhole: Option<f64>,
    zoom: Option<f64>,
    z_step: Option<f64>,
    microscope_model: Option<String>,
    flip_x: bool,
    flip_y: bool,
    swap_xy: bool,
    excitation_wavelengths: Vec<Option<f64>>,
    exp_times: Vec<Option<f64>>,
    detector_models: Vec<String>,
    // filter settings
    lens_na: Option<f64>,
    serial_number: Option<String>,
    magnification: Option<f64>,
    immersion: Option<String>,
    correction: Option<String>,
    objective_model: Option<String>,
    refractive_index: Option<f64>,
    pos_x: Option<f64>,
    pos_y: Option<f64>,
    pos_z: Option<f64>,
    cut_ins: Vec<f64>,
    cut_outs: Vec<f64>,
    filter_models: Vec<String>,
    active_detector: Vec<bool>,
    detector_indexes: BTreeMap<i64, String>,
    // detectors / scanner
    gains: Vec<Option<f64>>,
    detector_offsets: Vec<Option<f64>>,
    channel_names: Vec<Option<String>>,
    // lasers
    laser_wavelength: Vec<f64>,
    /// Per-laser-line `IntensityDev`-derived intensity (Java `laserIntensity`).
    laser_intensity: Vec<f64>,
    /// Per-AotfList FRAP-master flag (Java `laserFrap`).
    laser_frap: Vec<bool>,
    /// Per-non-master detector definition: whether any detector was active
    /// (Java `laserActive`).
    laser_active: Vec<bool>,
    // timestamps
    timestamps: Vec<Option<f64>>,
    acquired_date: Option<f64>,
    // alternate ROI centre flag (set when an <ROI> sibling exists)
    alternate_center: bool,
}

impl LifMetaAcc {
    fn new(effective_c: usize) -> Self {
        LifMetaAcc {
            time_increment: None,
            pinhole: None,
            zoom: None,
            z_step: None,
            microscope_model: None,
            flip_x: false,
            flip_y: false,
            swap_xy: false,
            excitation_wavelengths: vec![None; effective_c],
            exp_times: vec![None; effective_c],
            detector_models: Vec::new(),
            lens_na: None,
            serial_number: None,
            magnification: None,
            immersion: None,
            correction: None,
            objective_model: None,
            refractive_index: None,
            pos_x: None,
            pos_y: None,
            pos_z: None,
            cut_ins: Vec::new(),
            cut_outs: Vec::new(),
            filter_models: Vec::new(),
            active_detector: Vec::new(),
            detector_indexes: BTreeMap::new(),
            gains: vec![None; effective_c],
            detector_offsets: vec![None; effective_c],
            channel_names: vec![None; effective_c],
            laser_wavelength: Vec::new(),
            laser_intensity: Vec::new(),
            laser_frap: Vec::new(),
            laser_active: Vec::new(),
            timestamps: Vec::new(),
            acquired_date: None,
            alternate_center: false,
        }
    }
}

/// Port of Java `LIFReader.translateScannerSettings`. Reads
/// `<ScannerSettingRecord>` and the `<ATLConfocalSettingDefinition>` reachable
/// from the `HardwareSetting` attachment, populating the scanner-related fields.
fn translate_scanner_settings(dom: &Dom, img: usize, acc: &mut LifMetaAcc) {
    let mut records: Vec<usize> = Vec::new();
    dom.descendants(img, "ScannerSettingRecord", &mut records);

    let mut attachments: Vec<usize> = Vec::new();
    dom.descendants(img, "Attachment", &mut attachments);
    if attachments.is_empty() {
        return;
    }
    let mut confocal_settings: Vec<usize> = Vec::new();
    for &att in &attachments {
        if dom.nodes[att].attrs.get("Name").map(String::as_str) == Some("HardwareSetting") {
            dom.descendants(att, "ATLConfocalSettingDefinition", &mut confocal_settings);
        }
    }

    if records.is_empty() && confocal_settings.is_empty() {
        return;
    }
    let effective_c = acc.excitation_wavelengths.len();

    for &rec in &records {
        let attrs = &dom.nodes[rec].attrs;
        let id = attrs.get("Identifier").map(String::as_str).unwrap_or("");
        let value = attrs.get("Variant").map(|s| s.trim()).unwrap_or("");
        if value.is_empty() {
            continue;
        }

        if id == "SystemType" {
            acc.microscope_model = Some(value.to_string());
        } else if id == "dblPinhole" {
            if let Ok(v) = value.parse::<f64>() {
                acc.pinhole = Some(v * LIF_METER_MULTIPLY);
            }
        } else if id == "dblZoom" {
            acc.zoom = value.parse::<f64>().ok();
        } else if id == "dblStepSize" {
            if let Ok(v) = value.parse::<f64>() {
                acc.z_step = Some(v * LIF_METER_MULTIPLY);
            }
        } else if id == "nDelayTime_s" {
            acc.time_increment = value.parse::<f64>().ok();
        } else if id == "CameraName" {
            acc.detector_models.push(value.to_string());
        } else if id.find("WFC") == Some(1) {
            // Channel index from the digits in the identifier.
            let digits: String = id.chars().filter(|c| c.is_ascii_digit()).collect();
            let c: i32 = digits.parse().unwrap_or(0);
            if c < 0 || c as usize >= effective_c {
                continue;
            }
            let c = c as usize;
            if id.ends_with("ExposureTime") {
                acc.exp_times[c] = value.parse::<f64>().ok();
            } else if id.ends_with("Gain") {
                acc.gains[c] = value.parse::<f64>().ok();
            } else if id.ends_with("WaveLength") {
                if let Ok(ex) = value.parse::<f64>() {
                    if ex > 0.0 {
                        acc.excitation_wavelengths[c] = Some(ex);
                    }
                }
            } else if (id.ends_with("UesrDefName") || id.ends_with("UserDefName"))
                && value != "None"
            {
                if acc.channel_names[c]
                    .as_ref()
                    .map(|n| n.trim().is_empty())
                    .unwrap_or(true)
                {
                    acc.channel_names[c] = Some(value.to_string());
                }
            }
        }
    }

    for &cs in &confocal_settings {
        let attrs = &dom.nodes[cs].attrs;
        if let Some(v) = attrs.get("Pinhole").map(|s| s.trim()).filter(|s| !s.is_empty()) {
            if let Ok(v) = v.parse::<f64>() {
                acc.pinhole = Some(v * LIF_METER_MULTIPLY);
            }
        }
        if let Some(v) = attrs.get("Zoom").map(|s| s.trim()).filter(|s| !s.is_empty()) {
            acc.zoom = v.parse::<f64>().ok();
        }
        if let Some(v) = attrs
            .get("ObjectiveName")
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        {
            acc.objective_model = Some(v.to_string());
        }
        if let Some(v) = attrs.get("FlipX").map(|s| s.trim()).filter(|s| !s.is_empty()) {
            acc.flip_x = v == "1";
        }
        if let Some(v) = attrs.get("FlipY").map(|s| s.trim()).filter(|s| !s.is_empty()) {
            acc.flip_y = v == "1";
        }
        if let Some(v) = attrs.get("SwapXY").map(|s| s.trim()).filter(|s| !s.is_empty()) {
            acc.swap_xy = v == "1";
        }
    }
}

/// Java `getChannelIndex`: returns `data - 1` (or `-1` when `data < 0`).
fn get_channel_index(attrs: &BTreeMap<String, String>) -> i64 {
    let data = attrs
        .get("data")
        .filter(|s| !s.is_empty())
        .or_else(|| attrs.get("Data"))
        .map(String::as_str)
        .unwrap_or("");
    let channel: i64 = if data.is_empty() {
        0
    } else {
        data.parse().unwrap_or(0)
    };
    if channel < 0 {
        return -1;
    }
    channel - 1
}

/// Port of Java `LIFReader.translateFilterSettings`. Reads
/// `<FilterSettingRecord>` nodes to populate objective, stage position, filter
/// cut-in/out, and detector-index metadata.
fn translate_filter_settings(dom: &Dom, img: usize, acc: &mut LifMetaAcc) {
    let mut filter_settings: Vec<usize> = Vec::new();
    dom.descendants(img, "FilterSettingRecord", &mut filter_settings);
    if filter_settings.is_empty() {
        return;
    }

    acc.active_detector.clear();
    acc.cut_ins.clear();
    acc.cut_outs.clear();
    acc.filter_models.clear();
    acc.detector_indexes.clear();

    let mut next_channel = 0usize;
    let channel_count = acc.channel_names.len();

    for &fs in &filter_settings {
        let attrs = &dom.nodes[fs].attrs;
        let object = attrs.get("ObjectName").cloned().unwrap_or_default();
        let attribute = attrs.get("Attribute").map(String::as_str).unwrap_or("");
        let object_class = attrs.get("ClassName").map(String::as_str).unwrap_or("");
        let variant = attrs.get("Variant").cloned().unwrap_or_default();
        let data = attrs.get("Data").cloned().unwrap_or_default();

        if attribute == "NumericalAperture" {
            if !variant.trim().is_empty() {
                acc.lens_na = variant.trim().parse::<f64>().ok();
            }
        } else if attribute == "OrderNumber" {
            if !variant.trim().is_empty() {
                acc.serial_number = Some(variant.trim().to_string());
            }
        } else if object_class == "CDetectionUnit" {
            if attribute == "State" {
                let channel = get_channel_index(attrs);
                if channel < 0 {
                    continue;
                }
                if let Ok(d) = data.parse::<i64>() {
                    acc.detector_indexes.insert(d, object.clone());
                }
                acc.active_detector.push(variant.trim() == "Active");
            }
        } else if attribute == "Objective" {
            // Tokenise on spaces; find the "<mag>x<na>" token, then immersion and
            // correction tokens, mirroring the Java StringTokenizer walk.
            let tokens: Vec<&str> = variant.split(' ').filter(|t| !t.is_empty()).collect();
            let mut model = String::new();
            let mut idx = 0usize;
            let mut found_mag = false;
            while !found_mag && idx < tokens.len() {
                let token = tokens[idx];
                idx += 1;
                if let Some(x) = token.find('x') {
                    found_mag = true;
                    let na = &token[x + 1..];
                    if !na.trim().is_empty() {
                        acc.lens_na = na.trim().parse::<f64>().ok();
                    }
                    let mag = &token[..x];
                    if !mag.trim().is_empty() {
                        acc.magnification = mag.trim().parse::<f64>().ok();
                    }
                } else {
                    model.push_str(token);
                    model.push(' ');
                }
            }
            let mut immersion = "Other".to_string();
            if idx < tokens.len() {
                let t = tokens[idx];
                idx += 1;
                if !t.trim().is_empty() {
                    immersion = t.to_string();
                }
            }
            acc.immersion = Some(immersion);
            let mut correction = "Other".to_string();
            if idx < tokens.len() {
                let t = tokens[idx];
                if !t.trim().is_empty() {
                    correction = t.to_string();
                }
            }
            acc.correction = Some(correction);
            acc.objective_model = Some(model.trim().to_string());
        } else if attribute == "RefractionIndex" {
            if !variant.trim().is_empty() {
                acc.refractive_index = variant.trim().parse::<f64>().ok();
            }
        } else if attribute == "XPos" {
            if !variant.trim().is_empty() {
                acc.pos_x = variant.trim().parse::<f64>().ok();
            }
        } else if attribute == "YPos" {
            if !variant.trim().is_empty() {
                acc.pos_y = variant.trim().parse::<f64>().ok();
            }
        } else if attribute == "ZPos" {
            if !variant.trim().is_empty() {
                acc.pos_z = variant.trim().parse::<f64>().ok();
            }
        } else if object_class == "CSpectrophotometerUnit" {
            let v = variant.trim().parse::<f64>().ok();
            let description = attrs.get("Description").map(String::as_str).unwrap_or("");
            if description.ends_with("(left)") {
                acc.filter_models.push(object.clone());
                if let Some(v) = v {
                    if v > 0.0 {
                        acc.cut_ins.push(v.round());
                    }
                }
            } else if description.ends_with("(right)") {
                if let Some(v) = v {
                    if v > 0.0 {
                        acc.cut_outs.push(v.round());
                    }
                }
            } else if attribute == "Stain" && next_channel < channel_count {
                acc.channel_names[next_channel] = Some(variant.clone());
                next_channel += 1;
            }
        }
    }
}

/// Port of Java `LIFReader.translateLaserLines`. Reads `<AotfList>` /
/// `<LaserLineSetting>` to collect laser wavelengths.
fn translate_laser_lines(dom: &Dom, img: usize, acc: &mut LifMetaAcc) {
    let mut aotf_lists: Vec<usize> = Vec::new();
    dom.descendants(img, "AotfList", &mut aotf_lists);
    if aotf_lists.is_empty() {
        return;
    }
    acc.laser_wavelength.clear();
    acc.laser_intensity.clear();
    acc.laser_frap.clear();

    let mut base_intensity_index: usize = 0;

    for &aotf in &aotf_lists {
        let mut laser_lines: Vec<usize> = Vec::new();
        dom.descendants(aotf, "LaserLineSetting", &mut laser_lines);
        // grandparent name: parent.parent of the AotfList.
        let gp_name = dom.nodes[aotf]
            .parent
            .and_then(|p| dom.nodes[p].parent)
            .map(|gp| dom.nodes[gp].name.as_str())
            .unwrap_or("");
        let is_master =
            gp_name.ends_with("Sequential_Master") || gp_name.ends_with("Attachment");
        acc.laser_frap.push(gp_name.ends_with("FRAP_Master"));
        for &ll in &laser_lines {
            if is_master {
                continue;
            }
            let attrs = &dom.nodes[ll].attrs;
            let line_index = attrs
                .get("LineIndex")
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(0);
            let qualifier = attrs
                .get("Qualifier")
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(0);
            let index = line_index + (2 - (qualifier / 10));
            if index < 0 {
                continue;
            }
            let index = index as usize;
            let wavelength = attrs
                .get("LaserLine")
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(0.0);
            if index < acc.laser_wavelength.len() {
                acc.laser_wavelength[index] = wavelength;
            } else {
                while acc.laser_wavelength.len() < index {
                    acc.laser_wavelength.push(0.0);
                }
                acc.laser_wavelength.push(wavelength);
            }

            // IntensityDev → laserIntensity (Java ~1717-1736). Java stores
            // 100 - IntensityDev at realIndex = baseIntensityIndex + index.
            let real_intensity = 100.0
                - acc_attr_intensity(&dom.nodes[ll].attrs);
            let real_index = base_intensity_index + index;
            if real_index < acc.laser_intensity.len() {
                acc.laser_intensity[real_index] = real_intensity;
            } else {
                while real_index < acc.laser_intensity.len() {
                    acc.laser_intensity.push(100.0);
                }
                acc.laser_intensity.push(real_intensity);
            }
        }

        base_intensity_index += acc.laser_wavelength.len();
    }
}

/// Parse the `IntensityDev` attribute of a `<LaserLineSetting>` (Java ~1717-1724,
/// before the `100 -` inversion). Missing/blank/unparseable → 0.
fn acc_attr_intensity(attrs: &BTreeMap<String, String>) -> f64 {
    attrs
        .get("IntensityDev")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0)
}

/// Port of Java `LIFReader.translateTimestamps`. Supports both the LAS AF 3.1+
/// space-separated `<TimeStampList>` text content and the older per-element
/// `<TimeStamp HighInteger=.. LowInteger=..>` form.
fn translate_timestamps(dom: &Dom, img: usize, image_count: usize, acc: &mut LifMetaAcc) {
    let mut time_stamp_lists: Vec<usize> = Vec::new();
    dom.descendants(img, "TimeStampList", &mut time_stamp_lists);
    if time_stamp_lists.is_empty() {
        return;
    }
    let list = time_stamp_lists[0];
    acc.timestamps = vec![None; image_count];

    let number_of = dom.nodes[list]
        .attrs
        .get("NumberOfTimeStamps")
        .cloned()
        .unwrap_or_default();
    if !number_of.is_empty() {
        // LAS AF 3.1 (or newer): timestamps in the element text content.
        let raw = dom.nodes[list].text.clone();
        for (stamp, ts) in raw.split(' ').enumerate() {
            if stamp < image_count {
                acc.timestamps[stamp] = Some(translate_single_timestamp_hex(ts));
            }
        }
    } else {
        let mut timestamp_nodes: Vec<usize> = Vec::new();
        dom.descendants(img, "TimeStamp", &mut timestamp_nodes);
        if timestamp_nodes.is_empty() {
            return;
        }
        for (stamp, &node) in timestamp_nodes.iter().enumerate() {
            if stamp < image_count {
                acc.timestamps[stamp] = Some(translate_single_timestamp_attrs(&dom.nodes[node].attrs));
            }
        }
    }

    acc.acquired_date = acc.timestamps.first().copied().flatten();
}

/// Java `translateSingleTimestamp(String)`: split the trailing 8 hex digits as
/// the low word, the remainder as the high word, both base-16.
fn translate_single_timestamp_hex(timestamp: &str) -> f64 {
    let timestamp = timestamp.trim();
    let stamp_low_start = timestamp.len().saturating_sub(8);
    let stamp_high = &timestamp[..stamp_low_start];
    let stamp_low = &timestamp[stamp_low_start..];
    let high = if stamp_high.trim().is_empty() {
        0
    } else {
        i64::from_str_radix(stamp_high.trim(), 16).unwrap_or(0)
    };
    let low = if stamp_low.trim().is_empty() {
        0
    } else {
        i64::from_str_radix(stamp_low.trim(), 16).unwrap_or(0)
    };
    millis_from_ticks(high, low) as f64 / 1000.0
}

/// Java `translateSingleTimestamp(Element)`: the high/low words are decimal
/// `HighInteger`/`LowInteger` attributes.
fn translate_single_timestamp_attrs(attrs: &BTreeMap<String, String>) -> f64 {
    let high = attrs
        .get("HighInteger")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);
    let low = attrs
        .get("LowInteger")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);
    millis_from_ticks(high, low) as f64 / 1000.0
}

/// Port of `DateTools.getMillisFromTicks`: combine high/low 32-bit words into a
/// 64-bit Windows FILETIME (100-ns ticks since 1601), convert to Unix millis.
fn millis_from_ticks(high: i64, low: i64) -> i64 {
    let ticks = (high << 32) | (low & 0xffff_ffff);
    ticks / 10_000 - 11_644_473_600_000
}

/// Convert a COBOL-epoch second value (Java `acquiredDate`) to an ISO-8601
/// timestamp string. `DateTools.COBOL` is the millisecond offset of the COBOL
/// epoch (1582-10-15) relative to the Unix epoch; convertDate adds it.
fn cobol_iso8601(acquired_seconds: f64) -> String {
    const COBOL_OFFSET_MS: i64 = -12_219_292_800_000;
    let unix_ms = (acquired_seconds * 1000.0) as i64 + COBOL_OFFSET_MS;
    format_iso8601(unix_ms)
}

/// Format Unix milliseconds as an ISO-8601 `YYYY-MM-DDTHH:MM:SS` string (UTC),
/// matching `DateTools.ISO8601_FORMAT` without milliseconds/zone.
fn format_iso8601(unix_ms: i64) -> String {
    let mut secs = unix_ms.div_euclid(1000);
    let mut days = secs.div_euclid(86_400);
    secs = secs.rem_euclid(86_400);
    let hour = secs / 3600;
    let minute = (secs % 3600) / 60;
    let second = secs % 60;

    // Civil-from-days (Howard Hinnant's algorithm), days since 1970-01-01.
    days += 719_468;
    let era = days.div_euclid(146_097);
    let doe = days.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        year, month, day, hour, minute, second
    )
}

/// Port of Java `LIFReader.translateROIs` and `translateSingleROIs`. Reads
/// `<Annotation>` nodes (or `<ROISingle>` children) into `LifRoi`s, which are
/// finalised into OME shapes in `finalize_rois`.
fn translate_rois(
    dom: &Dom,
    img: usize,
    acc: &mut LifMetaAcc,
    _meta: &ImageMetadata,
    physical_size_x: Option<f64>,
    physical_size_y: Option<f64>,
) -> Vec<LifRoi> {
    let mut annotations: Vec<usize> = Vec::new();
    dom.descendants(img, "Annotation", &mut annotations);
    if !annotations.is_empty() {
        let mut roi_nodes: Vec<usize> = Vec::new();
        dom.descendants(img, "ROI", &mut roi_nodes);
        if !roi_nodes.is_empty() {
            acc.alternate_center = true;
        }
        let mut out = Vec::with_capacity(annotations.len());
        for &node in &annotations {
            let attrs = &dom.nodes[node].attrs;
            let mut roi = LifRoi::default();
            if let Some(t) = attrs.get("type").map(|s| s.trim()).filter(|s| !s.is_empty()) {
                roi.roi_type = t.parse().unwrap_or(0);
            }
            if let Some(c) = attrs.get("color").map(|s| s.trim()).filter(|s| !s.is_empty()) {
                roi.color = c.parse().unwrap_or(0);
            }
            roi.name = attrs.get("name").cloned();
            roi.font_size = attrs.get("fontSize").cloned();
            if let Some(v) = parse_attr_f64(attrs, "transTransX") {
                roi.trans_x = v;
            }
            if let Some(v) = parse_attr_f64(attrs, "transTransY") {
                roi.trans_y = v;
            }
            if let Some(v) = parse_attr_f64(attrs, "transScalingX") {
                roi.scale_x = v;
            }
            if let Some(v) = parse_attr_f64(attrs, "transScalingY") {
                roi.scale_y = v;
            }
            if let Some(v) = parse_attr_f64(attrs, "transRotation") {
                roi.rotation = v;
            }
            if let Some(lw) = attrs.get("linewidth").map(|s| s.trim()).filter(|s| !s.is_empty()) {
                if let Ok(v) = lw.parse::<i32>() {
                    roi.linewidth = v;
                }
            }
            roi.text = attrs.get("text").cloned();

            let mut vertices: Vec<usize> = Vec::new();
            dom.descendants(node, "Vertex", &mut vertices);
            if vertices.is_empty() {
                continue;
            }
            for &v in &vertices {
                let vattrs = &dom.nodes[v].attrs;
                if let Some(x) = parse_attr_f64(vattrs, "x") {
                    roi.x.push(x);
                }
                if let Some(y) = parse_attr_f64(vattrs, "y") {
                    roi.y.push(y);
                }
            }
            out.push(roi);
        }
        return out;
    }

    // translateSingleROIs path: <ROI><Children><Element><ROISingle>.
    let size_x = physical_size_x.unwrap_or(0.0);
    let size_y = physical_size_y.unwrap_or(0.0);
    let mut roi_nodes: Vec<usize> = Vec::new();
    dom.descendants(img, "ROI", &mut roi_nodes);
    if roi_nodes.is_empty() {
        return Vec::new();
    }
    let mut children: Vec<usize> = Vec::new();
    dom.descendants(roi_nodes[0], "Children", &mut children);
    if children.is_empty() {
        return Vec::new();
    }
    let mut elements: Vec<usize> = Vec::new();
    dom.descendants(children[0], "Element", &mut elements);
    if elements.is_empty() {
        return Vec::new();
    }

    let mut out = Vec::with_capacity(elements.len());
    for &elem in &elements {
        let mut singles: Vec<usize> = Vec::new();
        dom.descendants(elem, "ROISingle", &mut singles);
        let roi_node = match singles.first() {
            Some(&n) => n,
            None => continue,
        };
        let attrs = &dom.nodes[roi_node].attrs;
        let mut roi = LifRoi::default();
        if let Some(t) = attrs.get("RoiType").map(|s| s.trim()).filter(|s| !s.is_empty()) {
            roi.roi_type = t.parse().unwrap_or(0);
        }
        if let Some(c) = attrs.get("Color").map(|s| s.trim()).filter(|s| !s.is_empty()) {
            roi.color = c.parse().unwrap_or(0);
        }
        // name = roiNode.parent.parent.Name
        roi.name = dom.nodes[roi_node]
            .parent
            .and_then(|p| dom.nodes[p].parent)
            .and_then(|gp| dom.nodes[gp].attrs.get("Name").cloned());

        let mut vertices: Vec<usize> = Vec::new();
        dom.descendants(roi_node, "P", &mut vertices);
        for &v in &vertices {
            let vattrs = &dom.nodes[v].attrs;
            if let Some(x) = parse_attr_f64(vattrs, "X") {
                if size_x != 0.0 {
                    roi.x.push(x / size_x);
                }
            }
            if let Some(y) = parse_attr_f64(vattrs, "Y") {
                if size_y != 0.0 {
                    roi.y.push(y / size_y);
                }
            }
        }

        let mut transforms: Vec<usize> = Vec::new();
        dom.descendants(roi_node, "Transformation", &mut transforms);
        if let Some(&transform) = transforms.first() {
            if let Some(v) = parse_attr_f64(&dom.nodes[transform].attrs, "Rotation") {
                roi.rotation = v;
            }
            let mut scalings: Vec<usize> = Vec::new();
            dom.descendants(transform, "Scaling", &mut scalings);
            if let Some(&scaling) = scalings.first() {
                if let Some(v) = parse_attr_f64(&dom.nodes[scaling].attrs, "XScale") {
                    roi.scale_x = v;
                }
                if let Some(v) = parse_attr_f64(&dom.nodes[scaling].attrs, "YScale") {
                    roi.scale_y = v;
                }
            }
            let mut translations: Vec<usize> = Vec::new();
            dom.descendants(transform, "Translation", &mut translations);
            if let Some(&translation) = translations.first() {
                if let Some(v) = parse_attr_f64(&dom.nodes[translation].attrs, "X") {
                    if size_x != 0.0 {
                        roi.trans_x = v / size_x;
                    }
                }
                if let Some(v) = parse_attr_f64(&dom.nodes[translation].attrs, "Y") {
                    if size_y != 0.0 {
                        roi.trans_y = v / size_y;
                    }
                }
            }
        }

        out.push(roi);
    }
    out
}

fn parse_attr_f64(attrs: &BTreeMap<String, String>, key: &str) -> Option<f64> {
    attrs
        .get(key)
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse::<f64>().ok())
}

/// Port of Java `LIFReader.translateDetectors`: walks
/// `<ATLConfocalSettingDefinition>` / `<Detector>` / `<MultiBand>` to fill in
/// per-channel gains/offsets, cut-in/out filter ranges, detector models, and
/// channel names (dye names).
fn translate_detectors(dom: &Dom, img: usize, effective_c: usize, acc: &mut LifMetaAcc) {
    let mut definitions: Vec<usize> = Vec::new();
    dom.descendants(img, "ATLConfocalSettingDefinition", &mut definitions);
    if definitions.is_empty() {
        return;
    }

    let mut channels: Vec<String> = Vec::new();
    let mut next_channel = 0usize;
    acc.laser_active.clear();

    for &definition in &definitions {
        let parent_name = dom.nodes[definition]
            .parent
            .map(|p| dom.nodes[p].name.as_str())
            .unwrap_or("");
        let is_master = parent_name.ends_with("Master");
        let mut detectors: Vec<usize> = Vec::new();
        dom.descendants(definition, "Detector", &mut detectors);
        if detectors.is_empty() {
            return;
        }
        let mut count = 0usize;
        let mut multibands: Vec<usize> = Vec::new();
        if !is_master {
            dom.descendants(definition, "MultiBand", &mut multibands);
        }

        for &det in &detectors {
            let attrs = &dom.nodes[det].attrs;
            let gain = attrs
                .get("Gain")
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .and_then(|s| s.parse::<f64>().ok());
            let offset = attrs
                .get("Offset")
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .and_then(|s| s.parse::<f64>().ok());
            let active = attrs.get("IsActive").map(String::as_str) == Some("1");
            let channel: i64 = attrs
                .get("Channel")
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(0);

            if active {
                if let Some(model) = acc.detector_indexes.get(&channel).cloned() {
                    acc.detector_models.push(model);
                }

                let mut multiband: Option<usize> = None;
                if !is_master {
                    for &mb in &multibands {
                        let mc: i64 = dom.nodes[mb]
                            .attrs
                            .get("Channel")
                            .and_then(|s| s.parse::<i64>().ok())
                            .unwrap_or(0);
                        if channel == mc {
                            multiband = Some(mb);
                            break;
                        }
                    }
                }

                if let Some(mb) = multiband {
                    let mattrs = &dom.nodes[mb].attrs;
                    let dye = mattrs.get("DyeName").cloned().unwrap_or_default();
                    if !channels.contains(&dye) {
                        channels.push(dye);
                    }
                    let cut_in = parse_attr_f64(mattrs, "LeftWorld");
                    let cut_out = parse_attr_f64(mattrs, "RightWorld");
                    if let Some(ci) = cut_in {
                        if ci as i64 > 0 {
                            acc.cut_ins.push(ci.round());
                        }
                    }
                    if let Some(co) = cut_out {
                        if co as i64 > 0 {
                            acc.cut_outs.push(co.round());
                        }
                    }
                } else {
                    channels.push(String::new());
                }

                if !is_master {
                    if channel < next_channel as i64 {
                        next_channel = 0;
                    }
                    if next_channel < effective_c {
                        if let Some(g) = gain {
                            acc.gains[next_channel] = Some(g);
                        }
                        if let Some(o) = offset {
                            acc.detector_offsets[next_channel] = Some(o);
                        }
                    }
                    next_channel += 1;
                }
            } else {
                count += 1;
            }
            if active {
                acc.active_detector.push(active);
            }
        }
        // Store whether this (non-master) definition had any active detector.
        if !is_master {
            acc.laser_active.push(count < detectors.len());
        }
    }

    // Assign trailing dye names to channels lacking names.
    if !channels.is_empty() {
        for i in 0..effective_c {
            let index = i as isize + channels.len() as isize - effective_c as isize;
            if index >= 0 && (index as usize) < channels.len() {
                let blank = acc.channel_names[i]
                    .as_ref()
                    .map(|n| n.trim().is_empty())
                    .unwrap_or(true);
                if blank {
                    acc.channel_names[i] = Some(channels[index as usize].clone());
                }
            }
        }
    }
}

/// Build the per-series OME instrument from the accumulated metadata, mirroring
/// the `MetadataStore` population in Java `initMetadata`: objective (NA, model,
/// magnification, immersion, correction), filters (cut-in/out), lasers
/// (wavelengths), and detectors (model, zoom, gain/offset).
fn build_instrument(acc: &LifMetaAcc, effective_c: usize) -> OmeInstrument {
    let mut inst = OmeInstrument {
        microscope_model: acc.microscope_model.clone(),
        ..OmeInstrument::default()
    };

    // Objective.
    let objective = OmeObjective {
        lens_na: acc.lens_na,
        nominal_magnification: acc.magnification,
        immersion: acc.immersion.clone(),
        correction: acc.correction.clone(),
        model: acc.objective_model.clone(),
        ..OmeObjective::default()
    };
    inst.objectives.push(objective);

    // Filters: trim cut-ins to filterModels.size() the way Java does, then emit.
    let mut cut_ins = acc.cut_ins.clone();
    if !acc.filter_models.is_empty() && cut_ins.len() >= acc.filter_models.len() * 2 {
        let diff = cut_ins.len() - acc.filter_models.len();
        for _ in 0..diff {
            if acc.filter_models.len() < cut_ins.len() {
                cut_ins.remove(acc.filter_models.len());
            }
        }
    }
    for filter in 0..cut_ins.len() {
        inst.filters.push(OmeFilter {
            model: acc.filter_models.get(filter).cloned(),
            cut_in: cut_ins.get(filter).copied(),
            cut_out: acc.cut_outs.get(filter).copied(),
            ..OmeFilter::default()
        });
    }

    // Lasers: drop zero wavelengths, then one light source per remaining laser.
    let lasers: Vec<f64> = acc
        .laser_wavelength
        .iter()
        .copied()
        .filter(|w| *w != 0.0)
        .collect();
    for w in &lasers {
        // Java sets LaserWavelength on the <Laser> light source.
        inst.light_sources.push(OmeLightSource {
            light_source_type: Some("Laser".to_string()),
            model: None,
            wavelength: Some(*w),
            ..OmeLightSource::default()
        });
    }

    // Detectors: the trailing effectiveC entries, with zoom + per-channel offset.
    let detectors = &acc.detector_models;
    let start = detectors.len().saturating_sub(effective_c);
    let mut next_channel = 0usize;
    for detector in start..detectors.len() {
        let d_index = detector - start;
        let mut det = OmeDetector {
            model: Some(detectors[detector].clone()),
            detector_type: Some("PMT".to_string()),
            ..OmeDetector::default()
        };
        let _ = d_index;
        if !acc.active_detector.is_empty() {
            let detector_index =
                acc.active_detector.len() as isize - effective_c as isize + d_index as isize;
            if detector_index >= 0
                && (detector_index as usize) < acc.active_detector.len()
                && acc.active_detector[detector_index as usize]
                && next_channel < acc.detector_offsets.len()
            {
                det.offset = acc.detector_offsets[next_channel];
                next_channel += 1;
            }
        }
        inst.detectors.push(det);
    }

    inst
}

/// Build the per-plane OME metadata (positions, deltaT, exposure time),
/// mirroring the `image=0..getImageCount()` loop in Java `initMetadata`.
fn build_planes(acc: &LifMetaAcc, meta: &ImageMetadata) -> Vec<OmePlane> {
    let image_count = meta.image_count;
    if image_count == 0 {
        return Vec::new();
    }

    // Stage position, with swapXY and flip applied (Java checkFlip negates).
    let mut x_pos = acc.pos_x;
    let mut y_pos = acc.pos_y;
    if acc.swap_xy {
        std::mem::swap(&mut x_pos, &mut y_pos);
    }
    let x_pos = check_flip(acc.flip_x, x_pos);
    let y_pos = check_flip(acc.flip_y, y_pos);
    let z_pos = acc.pos_z;

    let mut planes = Vec::with_capacity(image_count as usize);
    for image in 0..image_count {
        let (z, c, t) = zct_for_plane(image, meta);
        let mut plane = OmePlane {
            the_z: z,
            the_c: c,
            the_t: t,
            position_x: x_pos,
            position_y: y_pos,
            position_z: z_pos,
            ..OmePlane::default()
        };

        // DeltaT: Java subtracts the acquisition date when the first timestamp
        // equals it, else clamps to the first timestamp.
        if !acc.timestamps.is_empty() {
            if let Some(Some(ts)) = acc.timestamps.get(image as usize) {
                let mut timestamp = *ts;
                let first = acc.timestamps.first().copied().flatten();
                let acquired = acc.acquired_date;
                if first == acquired {
                    if let Some(a) = acquired {
                        timestamp -= a;
                    }
                } else if Some(timestamp) == acquired && image > 0 {
                    if let Some(f) = first {
                        timestamp = f;
                    }
                }
                plane.delta_t = Some(timestamp);
            }
        }

        // Exposure time: per-channel.
        if let Some(Some(exp)) = acc.exp_times.get(c as usize) {
            plane.exposure_time = Some(*exp);
        }

        planes.push(plane);
    }
    planes
}

/// Java `LIFReader.checkFlip`: negate the position when the flip flag is set.
fn check_flip(flip: bool, pos: Option<f64>) -> Option<f64> {
    match (flip, pos) {
        (true, Some(p)) => Some(-p),
        _ => pos,
    }
}

/// Convert parsed `LifRoi`s into OME ROIs (one ROI, one shape) mirroring
/// `ROI.normalize()` + `ROI.storeROI`. `size_x`/`size_y` are the series core
/// pixel dimensions used to compute the ROI centre point.
fn lif_rois_to_ome(rois: &[LifRoi], acc: &LifMetaAcc, size_x: u32, size_y: u32) -> Vec<OmeROI> {
    const POLYGON: i32 = 32;
    const RECTANGLE: i32 = 16;
    const TEXT: i32 = 512;
    const LINE: i32 = 256;
    const ARROW: i32 = 2;
    const SCALE_BAR: i32 = 8192;

    let mut out = Vec::new();
    for roi in rois {
        // normalize(): scale vertices and translation by METER_MULTIPLY.
        let trans_x = roi.trans_x * LIF_METER_MULTIPLY;
        let trans_y = roi.trans_y * LIF_METER_MULTIPLY;
        let xs: Vec<f64> = roi.x.iter().map(|v| v * LIF_METER_MULTIPLY).collect();
        let ys: Vec<f64> = roi.y.iter().map(|v| v * LIF_METER_MULTIPLY).collect();
        if xs.is_empty() || ys.is_empty() {
            continue;
        }

        let corner_x = xs[0];
        let corner_y = ys[0];
        let center_x = (size_x as f64 / 2.0) - 1.0;
        let center_y = (size_y as f64 / 2.0) - 1.0;
        let (roi_x, roi_y) = if acc.alternate_center {
            (trans_x - 2.0 * corner_x, trans_y - 2.0 * corner_y)
        } else {
            (center_x + trans_x, center_y + trans_y)
        };

        let shape = match roi.roi_type {
            POLYGON => {
                let points: Vec<(f64, f64)> = xs
                    .iter()
                    .zip(ys.iter())
                    .map(|(x, y)| (x * roi.scale_x + roi_x, y * roi.scale_y + roi_y))
                    .collect();
                Some(OmeShape::Polygon {
                    points,
                    the_z: None,
                    the_t: None,
                    the_c: None,
                })
            }
            TEXT | RECTANGLE => Some(OmeShape::Rectangle {
                x: roi_x - corner_x.abs(),
                y: roi_y - corner_y.abs(),
                width: 2.0 * corner_x.abs(),
                height: 2.0 * corner_y.abs(),
                the_z: None,
                the_t: None,
                the_c: None,
            }),
            SCALE_BAR | ARROW | LINE => {
                if xs.len() >= 2 && ys.len() >= 2 {
                    Some(OmeShape::Line {
                        x1: roi_x + xs[0],
                        y1: roi_y + ys[0],
                        x2: roi_x + xs[1],
                        y2: roi_y + ys[1],
                        the_z: None,
                        the_t: None,
                        the_c: None,
                    })
                } else {
                    None
                }
            }
            _ => None,
        };

        if let Some(shape) = shape {
            out.push(OmeROI {
                id: None,
                name: roi.text.clone().or_else(|| roi.name.clone()),
                shapes: vec![shape],
            });
        } else {
            out.push(OmeROI {
                id: None,
                name: roi.text.clone().or_else(|| roi.name.clone()),
                shapes: Vec::new(),
            });
        }
    }
    out
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

    #[test]
    fn light_source_settings_attenuation_is_percent_fraction() {
        use super::{apply_laser_light_source_settings, LifMetaAcc, OmeChannel};

        let effective_c = 2;
        let mut acc = LifMetaAcc::new(effective_c);
        // Two non-zero lasers → size == 2.
        acc.laser_wavelength = vec![488.0, 561.0];
        // Channel 0 uses laser 0 at 70% intensity (<100 → valid), channel 1
        // uses laser 1 at full power (==100 → ignored).
        acc.laser_intensity = vec![70.0, 100.0, 100.0, 100.0];
        acc.laser_active = vec![true, true];
        acc.laser_frap = vec![false];
        acc.channel_names = vec![None, None];

        let mut channels = vec![OmeChannel::default(), OmeChannel::default()];
        let lasers = apply_laser_light_source_settings(&acc, effective_c, &mut channels);

        // Channel 0: 70% → PercentFraction 0.70, laser index 0.
        assert_eq!(channels[0].light_source_settings_attenuation, Some(0.7));
        assert_eq!(lasers[0], Some(0));
        // Channel 1: intensity 100 was filtered out (not < 100).
        assert_eq!(channels[1].light_source_settings_attenuation, None);
        assert_eq!(lasers[1], None);
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

    fn synthetic_rgba_lif_bytes() -> Vec<u8> {
        let xml = r#"<LMSDataContainerHeader><Element Name="Experiment"><Element Name="RGBA Scan"><Memory MemoryBlockID="RgbaMem"/><Data><Image Name="RGBA Image"><ImageDescription><Channels><ChannelDescription BytesInc="0"/><ChannelDescription BytesInc="1"/><ChannelDescription BytesInc="2"/><ChannelDescription BytesInc="3"/></Channels><Dimensions><DimensionDescription DimID="1" NumberOfElements="2" BytesInc="3"/><DimensionDescription DimID="2" NumberOfElements="2" BytesInc="8"/></Dimensions></ImageDescription></Image></Data></Element></Element></LMSDataContainerHeader>"#;
        let xml = utf16le(xml);

        let mut bytes = vec![0x70, 0, 0, 0x70, 0, 0, 0, 0, 0x2a];
        bytes.extend_from_slice(&((xml.len() / 2) as i32).to_le_bytes());
        bytes.extend_from_slice(&xml);

        let id = utf16le("RgbaMem");
        bytes.extend_from_slice(&(0x70_i32).to_le_bytes());
        bytes.extend_from_slice(&0_i32.to_le_bytes());
        bytes.push(0x2a);
        bytes.extend_from_slice(&16_i32.to_le_bytes());
        bytes.push(0x2a);
        bytes.extend_from_slice(&((id.len() / 2) as i32).to_le_bytes());
        bytes.extend_from_slice(&id);
        bytes.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8, 11, 12, 13, 14, 15, 16, 17, 18]);
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

    fn synthetic_padded_planar_rgb_lif_bytes(channel_offsets: [u64; 3]) -> Vec<u8> {
        let xml = format!(
            r#"<LMSDataContainerHeader><Element Name="Experiment"><Element Name="Padded Planar RGB Scan"><Memory MemoryBlockID="PaddedPlanarRgbMem"/><Data><Image Name="Padded Planar RGB Image"><ImageDescription><Channels><ChannelDescription BytesInc="{}"/><ChannelDescription BytesInc="{}"/><ChannelDescription BytesInc="{}"/></Channels><Dimensions><DimensionDescription DimID="1" NumberOfElements="2" BytesInc="3"/><DimensionDescription DimID="2" NumberOfElements="2" BytesInc="2"/></Dimensions></ImageDescription></Image></Data></Element></Element></LMSDataContainerHeader>"#,
            channel_offsets[0], channel_offsets[1], channel_offsets[2]
        );
        let xml = utf16le(&xml);

        let mut bytes = vec![0x70, 0, 0, 0x70, 0, 0, 0, 0, 0x2a];
        bytes.extend_from_slice(&((xml.len() / 2) as i32).to_le_bytes());
        bytes.extend_from_slice(&xml);

        let id = utf16le("PaddedPlanarRgbMem");
        bytes.extend_from_slice(&(0x70_i32).to_le_bytes());
        bytes.extend_from_slice(&0_i32.to_le_bytes());
        bytes.push(0x2a);
        bytes.extend_from_slice(&15_i32.to_le_bytes());
        bytes.push(0x2a);
        bytes.extend_from_slice(&((id.len() / 2) as i32).to_le_bytes());
        bytes.extend_from_slice(&id);
        bytes.extend_from_slice(&[1, 2, 3, 4, 99, 5, 6, 7, 8, 88, 9, 10, 11, 12, 77]);
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

    fn synthetic_memory_id_lif_bytes(xml_id: &str, block_id: &str) -> Vec<u8> {
        let xml = format!(
            r#"<LMSDataContainerHeader><Element Name="Experiment"><Element Name="Storage Scan"><Memory MemoryBlockID="{xml_id}"/><Data><Image Name="Storage Image"><ImageDescription><Channels><ChannelDescription BytesInc="0"/></Channels><Dimensions><DimensionDescription DimID="1" NumberOfElements="2" BytesInc="1"/><DimensionDescription DimID="2" NumberOfElements="2" BytesInc="2"/></Dimensions></ImageDescription></Image></Data></Element></Element></LMSDataContainerHeader>"#
        );
        let xml = utf16le(&xml);

        let mut bytes = vec![0x70, 0, 0, 0x70, 0, 0, 0, 0, 0x2a];
        bytes.extend_from_slice(&((xml.len() / 2) as i32).to_le_bytes());
        bytes.extend_from_slice(&xml);

        let id = utf16le(block_id);
        bytes.extend_from_slice(&(0x70_i32).to_le_bytes());
        bytes.extend_from_slice(&0_i32.to_le_bytes());
        bytes.push(0x2a);
        bytes.extend_from_slice(&4_i32.to_le_bytes());
        bytes.push(0x2a);
        bytes.extend_from_slice(&((id.len() / 2) as i32).to_le_bytes());
        bytes.extend_from_slice(&id);
        bytes.extend_from_slice(&[1, 2, 3, 4]);
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

    fn gzip_compressed(raw: &[u8]) -> Vec<u8> {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use std::io::Write;

        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(raw).unwrap();
        encoder.finish().unwrap()
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
        assert!(matches!(
            meta.series_metadata.get("lif.memory_block.requested_id"),
            Some(MetadataValue::String(value)) if value == "Mem1"
        ));
        assert!(matches!(
            meta.series_metadata.get("lif.memory_block.resolved_id"),
            Some(MetadataValue::String(value)) if value == "Mem1"
        ));
        assert!(matches!(
            meta.series_metadata.get("lif.memory_block.status"),
            Some(MetadataValue::String(value)) if value == "matched_by_id"
        ));
        assert!(matches!(
            meta.series_metadata.get("lif.memory_block.byte_length"),
            Some(MetadataValue::Int(192))
        ));
        assert!(matches!(
            meta.series_metadata.get("lif.memory_block.file_offset"),
            Some(MetadataValue::Int(value)) if *value > 0
        ));

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
    fn records_memory_block_file_order_fallback_provenance() {
        let bytes = synthetic_memory_id_lif_bytes("XmlMem", "ActualMem");
        let path = temp_lif_path("memory_fallback");
        std::fs::write(&path, &bytes).unwrap();

        let mut reader = LifReader::new();
        reader.set_id(&path).unwrap();

        let meta = reader.metadata();
        assert!(matches!(
            meta.series_metadata.get("lif.memory_block.requested_id"),
            Some(MetadataValue::String(value)) if value == "XmlMem"
        ));
        assert!(matches!(
            meta.series_metadata.get("lif.memory_block.resolved_id"),
            Some(MetadataValue::String(value)) if value == "ActualMem"
        ));
        assert!(matches!(
            meta.series_metadata.get("lif.memory_block.status"),
            Some(MetadataValue::String(value)) if value == "fallback_file_order"
        ));
        assert!(matches!(
            meta.series_metadata.get("lif.memory_block.byte_length"),
            Some(MetadataValue::Int(4))
        ));
        assert!(matches!(
            meta.series_metadata.get("lif.memory_block.diagnostic"),
            Some(MetadataValue::String(value))
                if value.contains("MemoryBlockID")
                    && value.contains("file order")
        ));
        assert_eq!(reader.open_bytes(0).unwrap(), [1, 2, 3, 4]);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn returns_blank_plane_for_truncated_pixel_block_like_java() {
        let mut bytes = synthetic_lif_bytes();
        bytes.truncate(bytes.len() - 192);
        let path = temp_lif_path("missing_block");
        std::fs::write(&path, &bytes).unwrap();

        let mut reader = LifReader::new();
        reader.set_id(&path).unwrap();
        assert_eq!(reader.open_bytes(0).unwrap(), vec![0; 24]);
        assert_eq!(reader.open_bytes_region(5, 1, 1, 2, 2).unwrap(), vec![0; 8]);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn returns_blank_plane_for_missing_pixel_block_like_java() {
        let mut bytes = synthetic_lif_bytes();
        let xml_chars = i32::from_le_bytes(bytes[9..13].try_into().unwrap()) as usize;
        bytes.truncate(13 + xml_chars * 2);
        let path = temp_lif_path("no_memory_block");
        std::fs::write(&path, &bytes).unwrap();

        let mut reader = LifReader::new();
        reader.set_id(&path).unwrap();

        assert!(matches!(
            reader.metadata().series_metadata.get("lif.memory_block.status"),
            Some(MetadataValue::String(value)) if value == "missing"
        ));
        assert_eq!(reader.open_bytes(0).unwrap(), vec![0; 24]);

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
        assert!(matches!(
            meta.series_metadata.get("lif.compression.status"),
            Some(MetadataValue::String(value)) if value == "supported_zlib"
        ));
        assert!(matches!(
            meta.series_metadata
                .get("lif.compression.payload_signature"),
            Some(MetadataValue::String(value)) if value == "zlib stream"
        ));
        assert!(matches!(
            meta.series_metadata
                .get("lif.compression.payload_first_bytes"),
            Some(MetadataValue::String(value)) if value == "78 01 01 04 00 fb ff 01"
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

        assert!(matches!(
            reader
                .metadata()
                .series_metadata
                .get("lif.compression.status"),
            Some(MetadataValue::String(value)) if value == "supported_raw_deflate"
        ));
        assert_eq!(reader.open_bytes(0).unwrap(), [5, 6, 7, 8]);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn reads_gzip_compressed_lif_payload() {
        let bytes = synthetic_compressed_lif_bytes("GZip", &gzip_compressed(&[9, 10, 11, 12]));
        let path = temp_lif_path("gzip");
        std::fs::write(&path, &bytes).unwrap();

        let mut reader = LifReader::new();
        reader.set_id(&path).unwrap();

        let meta = reader.metadata();
        assert!(matches!(
            meta.series_metadata.get("lif.compression"),
            Some(MetadataValue::String(value)) if value == "Compression=GZip"
        ));
        assert!(matches!(
            meta.series_metadata.get("lif.compression.status"),
            Some(MetadataValue::String(value)) if value == "supported_gzip"
        ));
        assert_eq!(reader.open_bytes(0).unwrap(), [9, 10, 11, 12]);
        assert_eq!(reader.open_bytes_region(0, 0, 1, 2, 1).unwrap(), [11, 12]);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn reads_generic_compressed_lif_payload_by_zlib_signature() {
        let bytes = synthetic_compressed_lif_bytes("true", &zlib_stored(&[21, 22, 23, 24]));
        let path = temp_lif_path("generic_zlib");
        std::fs::write(&path, &bytes).unwrap();

        let mut reader = LifReader::new();
        reader.set_id(&path).unwrap();

        let meta = reader.metadata();
        assert!(matches!(
            meta.series_metadata.get("lif.compression"),
            Some(MetadataValue::String(value)) if value == "Compression=true"
        ));
        assert!(matches!(
            meta.series_metadata.get("lif.compression.status"),
            Some(MetadataValue::String(value)) if value == "supported_zlib_payload_signature"
        ));
        assert!(matches!(
            meta.series_metadata.get("lif.compression.diagnostic"),
            Some(MetadataValue::String(value))
                if value.contains("Compression=true")
                    && value.contains("payload signature zlib stream")
        ));
        assert_eq!(reader.open_bytes(0).unwrap(), [21, 22, 23, 24]);
        assert_eq!(reader.open_bytes_region(0, 1, 0, 1, 2).unwrap(), [22, 24]);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn reads_generic_compressed_lif_payload_by_gzip_signature() {
        let bytes = synthetic_compressed_lif_bytes("yes", &gzip_compressed(&[31, 32, 33, 34]));
        let path = temp_lif_path("generic_gzip");
        std::fs::write(&path, &bytes).unwrap();

        let mut reader = LifReader::new();
        reader.set_id(&path).unwrap();

        let meta = reader.metadata();
        assert!(matches!(
            meta.series_metadata.get("lif.compression"),
            Some(MetadataValue::String(value)) if value == "Compression=yes"
        ));
        assert!(matches!(
            meta.series_metadata.get("lif.compression.status"),
            Some(MetadataValue::String(value)) if value == "supported_gzip_payload_signature"
        ));
        assert!(matches!(
            meta.series_metadata
                .get("lif.compression.payload_signature"),
            Some(MetadataValue::String(value)) if value == "gzip stream"
        ));
        assert_eq!(reader.open_bytes(0).unwrap(), [31, 32, 33, 34]);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn rejects_unknown_compressed_lif_payload_hint() {
        let bytes = synthetic_compressed_lif_bytes("LeicaMagic", &[0x1f, 0x8b, 0x08, 0x00, 0xff]);
        let path = temp_lif_path("unknown_compressed");
        std::fs::write(&path, &bytes).unwrap();

        let mut reader = LifReader::new();
        reader.set_id(&path).unwrap();

        let meta = reader.metadata();
        assert!(matches!(
            meta.series_metadata.get("lif.compression.status"),
            Some(MetadataValue::String(value)) if value == "unsupported_hint"
        ));
        assert!(matches!(
            meta.series_metadata.get("lif.compression.diagnostic"),
            Some(MetadataValue::String(value))
                if value.contains("Compression=LeicaMagic")
                    && value.contains("zlib/deflate")
        ));
        assert!(matches!(
            meta.series_metadata
                .get("lif.compression.payload_signature"),
            Some(MetadataValue::String(value)) if value == "gzip stream"
        ));
        assert!(matches!(
            meta.series_metadata
                .get("lif.compression.payload_first_bytes"),
            Some(MetadataValue::String(value)) if value == "1f 8b 08 00 ff"
        ));

        let err = reader.open_bytes(0).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::UnsupportedFormat(ref message)
                if message.contains("unsupported compression hint")
                    && message.contains("Compression=LeicaMagic")
                    && message.contains("payload signature gzip stream")
                    && message.contains("first bytes [1f 8b 08 00 ff]")
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

        // color planes are stored in BGR order; the reader swaps B<->R per
        // pixel (Java ImageTools.bgrToRgb, interleaved, 3 samples).
        assert_eq!(
            reader.open_bytes(0).unwrap(),
            [3, 2, 1, 6, 5, 4, 9, 8, 7, 12, 11, 10]
        );
        assert_eq!(
            reader.open_bytes_region(0, 1, 0, 1, 2).unwrap(),
            [6, 5, 4, 12, 11, 10]
        );
        assert_eq!(
            reader.open_bytes(1).unwrap(),
            [103, 102, 101, 106, 105, 104, 109, 108, 107, 112, 111, 110]
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn reads_uncompressed_interleaved_four_sample_color_pixels() {
        let bytes = synthetic_rgba_lif_bytes();
        let path = temp_lif_path("rgba");
        std::fs::write(&path, &bytes).unwrap();

        let mut reader = LifReader::new();
        reader.set_id(&path).unwrap();

        let meta = reader.metadata();
        assert_eq!(meta.size_x, 2);
        assert_eq!(meta.size_y, 2);
        assert_eq!(meta.size_z, 1);
        assert_eq!(meta.size_c, 4);
        assert_eq!(meta.image_count, 1);
        assert!(meta.is_rgb);
        assert!(meta.is_interleaved);
        assert!(!meta.is_indexed);
        assert!(matches!(
            meta.series_metadata.get("lif.rgb_samples_per_pixel"),
            Some(MetadataValue::Int(4))
        ));
        assert!(matches!(
            meta.series_metadata.get("lif.effective_size_c"),
            Some(MetadataValue::Int(1))
        ));

        let ome = reader.ome_metadata().expect("OME metadata");
        assert_eq!(ome.images[0].channels.len(), 1);
        assert_eq!(ome.images[0].channels[0].samples_per_pixel, 4);

        assert_eq!(
            reader.open_bytes(0).unwrap(),
            [1, 2, 3, 4, 5, 6, 7, 8, 11, 12, 13, 14, 15, 16, 17, 18]
        );
        assert_eq!(
            reader.open_bytes_region(0, 1, 0, 1, 2).unwrap(),
            [5, 6, 7, 8, 15, 16, 17, 18]
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

        // planar color planes are stored in BGR order; the reader swaps the
        // first and third planes (Java ImageTools.bgrToRgb, planar, 3 samples).
        assert_eq!(
            reader.open_bytes(0).unwrap(),
            [9, 10, 11, 12, 5, 6, 7, 8, 1, 2, 3, 4]
        );
        assert_eq!(
            reader.open_bytes_region(0, 1, 0, 1, 2).unwrap(),
            [10, 12, 6, 8, 2, 4]
        );
        assert_eq!(
            reader.open_bytes(1).unwrap(),
            [109, 110, 111, 112, 105, 106, 107, 108, 101, 102, 103, 104]
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
        // planar color planes are stored in BGR order; the reader swaps the
        // first and third planes (Java ImageTools.bgrToRgb, planar, 3 samples).
        assert_eq!(
            reader.open_bytes(0).unwrap(),
            [9, 10, 11, 12, 5, 6, 7, 8, 1, 2, 3, 4]
        );
        assert_eq!(
            reader.open_bytes_region(0, 1, 0, 1, 2).unwrap(),
            [10, 12, 6, 8, 2, 4]
        );
        assert_eq!(
            reader.open_bytes(1).unwrap(),
            [109, 110, 111, 112, 105, 106, 107, 108, 101, 102, 103, 104]
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

        // each three-sample group is stored in BGR order; the reader swaps
        // B<->R per pixel (Java ImageTools.bgrToRgb, interleaved, 3 samples).
        assert_eq!(
            reader.open_bytes(0).unwrap(),
            [3, 2, 1, 6, 5, 4, 9, 8, 7, 12, 11, 10]
        );
        assert_eq!(
            reader.open_bytes(1).unwrap(),
            [103, 102, 101, 106, 105, 104, 109, 108, 107, 112, 111, 110]
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

        // each planar three-sample group is stored in BGR order; the reader
        // swaps the first and third planes (Java ImageTools.bgrToRgb, planar).
        assert_eq!(
            reader.open_bytes(0).unwrap(),
            [9, 10, 11, 12, 5, 6, 7, 8, 1, 2, 3, 4]
        );
        assert_eq!(
            reader.open_bytes_region(1, 1, 0, 1, 2).unwrap(),
            [110, 112, 106, 108, 102, 104]
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn reads_padded_planar_rgb_from_non_contiguous_channel_offsets() {
        let bytes = synthetic_padded_planar_rgb_lif_bytes([0, 5, 10]);
        let path = temp_lif_path("padded_planar_rgb");
        std::fs::write(&path, &bytes).unwrap();

        let mut reader = LifReader::new();
        reader.set_id(&path).unwrap();

        let meta = reader.metadata();
        assert_eq!(meta.size_c, 3);
        assert_eq!(meta.image_count, 1);
        assert!(meta.is_rgb);
        assert!(!meta.is_interleaved);
        assert!(matches!(
            meta.series_metadata.get("lif.color_layout.status"),
            Some(MetadataValue::String(value)) if value == "planar_padded_non_contiguous"
        ));

        // planar color planes are stored in BGR order; the reader swaps the
        // first and third planes (Java ImageTools.bgrToRgb, planar, 3 samples).
        assert_eq!(
            reader.open_bytes(0).unwrap(),
            [9, 10, 11, 12, 5, 6, 7, 8, 1, 2, 3, 4]
        );
        assert_eq!(
            reader.open_bytes_region(0, 1, 0, 1, 2).unwrap(),
            [10, 12, 6, 8, 2, 4]
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn rejects_overlapping_rgb_channel_stride_layout() {
        let bytes = synthetic_planar_rgb_lif_bytes([0, 3, 10]);
        let path = temp_lif_path("overlapping_rgb_stride");
        std::fs::write(&path, &bytes).unwrap();

        let mut reader = LifReader::new();
        reader.set_id(&path).unwrap();

        let meta = reader.metadata();
        assert!(matches!(
            meta.series_metadata.get("lif.color_layout.status"),
            Some(MetadataValue::String(value)) if value == "unsupported_irregular_non_contiguous"
        ));
        assert!(matches!(
            meta.series_metadata.get("lif.color_layout.diagnostic"),
            Some(MetadataValue::String(value))
                if value.contains("irregular/non-contiguous")
                    && value.contains("channel offsets=[0, 3, 10]")
        ));

        let err = reader.open_bytes(0).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::UnsupportedFormat(ref message)
                if message.contains("unsupported irregular/non-contiguous color layout")
                    && message.contains("irregular/non-contiguous")
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
