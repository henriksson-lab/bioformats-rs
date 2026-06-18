use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::ImageMetadata;
use crate::common::pixel_type::PixelType;
use crate::common::writer::FormatWriter;

/// Compression scheme for the TIFF writer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WriteCompression {
    /// No compression (fastest).
    #[default]
    None,
    /// Deflate/Zlib (good ratio, moderate speed).
    Deflate,
    /// LZW (classic TIFF compression).
    Lzw,
}

/// TIFF writer — supports 8/16/32-bit integer and 32/64-bit float images,
/// single-plane and multi-plane (Z/C/T stacks), grayscale and RGB.
pub struct TiffWriter {
    compression: WriteCompression,
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    file: Option<BufWriter<File>>,
    /// (strip_offset, strip_byte_count) recorded per strip as planes are written.
    plane_strips: Vec<Vec<(u64, u64)>>,
    planes_written: u32,
    /// Optional OME-XML to embed in the first IFD's ImageDescription.
    ome_xml: Option<String>,
    auto_ome_xml: bool,
}

impl TiffWriter {
    pub fn new() -> Self {
        TiffWriter {
            compression: WriteCompression::default(),
            path: None,
            meta: None,
            file: None,
            plane_strips: Vec::new(),
            planes_written: 0,
            ome_xml: None,
            auto_ome_xml: false,
        }
    }

    pub fn with_compression(mut self, c: WriteCompression) -> Self {
        self.compression = c;
        self
    }

    /// Set OME-XML to embed in the TIFF ImageDescription tag, producing an OME-TIFF.
    pub fn with_ome_xml(mut self, xml: String) -> Self {
        self.ome_xml = Some(xml);
        self
    }

    /// Generate minimal OME-XML from `ImageMetadata` during `set_metadata`.
    pub fn with_auto_ome_xml(mut self) -> Self {
        self.auto_ome_xml = true;
        self
    }

    /// Convenience: set OME metadata from an `OmeMetadata` struct.
    /// Must be called after `set_metadata` so the pixel metadata is available.
    pub fn set_ome_metadata(
        &mut self,
        ome: &crate::common::ome_metadata::OmeMetadata,
    ) -> Result<()> {
        if let Some(meta) = &self.meta {
            let mut ome = ome.clone();
            ome.populate_pixels(meta, 0)?;
            ome.verify_minimum_populated(meta, 0)?;
            self.ome_xml = Some(ome.to_ome_xml(meta));
        }
        Ok(())
    }
}

impl Default for TiffWriter {
    fn default() -> Self {
        Self::new()
    }
}

// ---- helpers ----------------------------------------------------------------

fn write_le_u16(w: &mut impl Write, v: u16) -> std::io::Result<()> {
    w.write_all(&v.to_le_bytes())
}
fn write_le_u32(w: &mut impl Write, v: u32) -> std::io::Result<()> {
    w.write_all(&v.to_le_bytes())
}

/// Returns (TIFF type code, bytes per element)
fn short_type() -> (u16, u32) {
    (3, 2)
}
fn long_type() -> (u16, u32) {
    (4, 4)
}
fn rational_type() -> (u16, u32) {
    (5, 8)
}

/// One IFD entry — tag, type, count, value_or_offset.
struct Entry {
    tag: u16,
    typ: u16,
    count: u32,
    /// Either the value inline (≤ 4 bytes) as a u32, or an offset into the file.
    value_or_offset: u32,
}

fn long_array_entry(tag: u16, values: &[u64], extra: &mut Vec<u8>) -> Result<Entry> {
    if values.len() == 1 {
        return Ok(long_entry(
            tag,
            classic_tiff_u32(values[0], "TIFF LONG entry")?,
        ));
    }
    let offset = extra.len() as u32;
    for &value in values {
        extra.extend_from_slice(&classic_tiff_u32(value, "TIFF LONG array value")?.to_le_bytes());
    }
    Ok(Entry {
        tag,
        typ: long_type().0,
        count: values.len() as u32,
        value_or_offset: offset,
    })
}

fn patch_extra_offset(
    ifd_bytes: &mut [u8],
    entry_offset: usize,
    extra_file_off: u64,
) -> Result<()> {
    let rel = u32::from_le_bytes([
        ifd_bytes[entry_offset + 8],
        ifd_bytes[entry_offset + 9],
        ifd_bytes[entry_offset + 10],
        ifd_bytes[entry_offset + 11],
    ]);
    let abs = classic_tiff_u32(extra_file_off + rel as u64, "IFD extra data offset")?;
    ifd_bytes[entry_offset + 8..entry_offset + 12].copy_from_slice(&abs.to_le_bytes());
    Ok(())
}

/// Write a SHORT entry with a single value stored inline.
fn short_entry(tag: u16, value: u16) -> Entry {
    Entry {
        tag,
        typ: short_type().0,
        count: 1,
        value_or_offset: value as u32,
    }
}

/// Write a LONG entry with a single value stored inline.
fn long_entry(tag: u16, value: u32) -> Entry {
    Entry {
        tag,
        typ: long_type().0,
        count: 1,
        value_or_offset: value,
    }
}

fn classic_tiff_u32(value: u64, field: &str) -> Result<u32> {
    u32::try_from(value).map_err(|_| {
        BioFormatsError::Format(format!(
            "classic TIFF {field} {value} exceeds 32-bit offset/count limit"
        ))
    })
}

fn sample_format(pt: PixelType) -> u16 {
    match pt {
        PixelType::Int8 | PixelType::Int16 | PixelType::Int32 => 2,
        PixelType::Float32 | PixelType::Float64 => 3,
        _ => 1, // unsigned integer (default)
    }
}

fn bits_per_sample_value(pt: PixelType) -> u16 {
    match pt {
        PixelType::Bit => 1,
        PixelType::Int8 | PixelType::Uint8 => 8,
        PixelType::Int16 | PixelType::Uint16 => 16,
        PixelType::Int32 | PixelType::Uint32 | PixelType::Float32 => 32,
        PixelType::Float64 => 64,
    }
}

/// Compress one strip's worth of data.
fn compress(data: &[u8], scheme: WriteCompression) -> Result<Vec<u8>> {
    match scheme {
        WriteCompression::None => Ok(data.to_vec()),
        WriteCompression::Deflate => {
            use flate2::write::ZlibEncoder;
            use flate2::Compression;
            use std::io::Write;
            let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
            enc.write_all(data).map_err(BioFormatsError::Io)?;
            enc.finish().map_err(BioFormatsError::Io)
        }
        WriteCompression::Lzw => {
            use weezl::{encode::Encoder, BitOrder};
            let mut enc = Encoder::with_tiff_size_switch(BitOrder::Msb, 8);
            enc.encode(data)
                .map_err(|e| BioFormatsError::Codec(e.to_string()))
        }
    }
}

fn compression_tag(scheme: WriteCompression) -> u16 {
    match scheme {
        WriteCompression::None => 1,
        WriteCompression::Lzw => 5,
        WriteCompression::Deflate => 8,
    }
}

fn expected_plane_len(meta: &ImageMetadata) -> Result<usize> {
    let samples_per_pixel = if meta.is_rgb { meta.size_c.max(1) } else { 1 };
    let bytes_per_sample = meta.pixel_type.bytes_per_sample() as u64;
    let len = meta.size_x as u64 * meta.size_y as u64 * samples_per_pixel as u64 * bytes_per_sample;
    usize::try_from(len).map_err(|_| {
        BioFormatsError::Format("TIFF writer: expected plane byte count overflows usize".into())
    })
}

fn expected_plane_count(meta: &ImageMetadata) -> Result<u32> {
    let effective_c = if meta.is_rgb { 1 } else { meta.size_c.max(1) };
    let dimension_planes = meta
        .size_z
        .max(1)
        .checked_mul(effective_c)
        .and_then(|v| v.checked_mul(meta.size_t.max(1)))
        .ok_or_else(|| BioFormatsError::Format("TIFF writer: plane count overflows u32".into()))?;
    let image_count = meta.image_count.max(1);
    if image_count > dimension_planes {
        return Err(BioFormatsError::Format(format!(
            "TIFF writer: metadata image_count {image_count} exceeds dimensional plane count {dimension_planes}"
        )));
    }
    Ok(dimension_planes)
}

fn validate_tiff_writer_metadata(meta: &ImageMetadata, allow_planar_rgb: bool) -> Result<()> {
    if meta.pixel_type == PixelType::Bit {
        return Err(BioFormatsError::Format(
            "TIFF writer does not support PixelType::Bit until 1-bit output is packed".into(),
        ));
    }
    if !allow_planar_rgb && meta.is_rgb && meta.size_c > 1 && !meta.is_interleaved {
        return Err(BioFormatsError::Format(
            "TIFF writer writes chunky RGB and does not support planar RGB metadata".into(),
        ));
    }
    Ok(())
}

fn expected_plane_len_for_dims(meta: &ImageMetadata, width: u32, height: u32) -> Result<usize> {
    let samples_per_pixel = if meta.is_rgb { meta.size_c.max(1) } else { 1 };
    let bytes_per_sample = meta.pixel_type.bytes_per_sample() as u64;
    let len = width as u64 * height as u64 * samples_per_pixel as u64 * bytes_per_sample;
    usize::try_from(len).map_err(|_| {
        BioFormatsError::Format("TIFF writer: expected plane byte count overflows usize".into())
    })
}

fn has_tiff_suffix(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.to_ascii_lowercase())
        .unwrap_or_default();
    matches!(
        name.as_str(),
        n if n.ends_with(".tif")
            || n.ends_with(".tiff")
            || n.ends_with(".tf2")
            || n.ends_with(".tf8")
            || n.ends_with(".btf")
    )
}

fn has_ome_tiff_suffix(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.to_ascii_lowercase())
        .unwrap_or_default();
    matches!(
        name.as_str(),
        n if n.ends_with(".ome.tif")
            || n.ends_with(".ome.tiff")
            || n.ends_with(".ome.tf2")
            || n.ends_with(".ome.tf8")
            || n.ends_with(".ome.btf")
    )
}

fn write_plane_strips(
    w: &mut BufWriter<File>,
    meta: &ImageMetadata,
    width: u32,
    height: u32,
    data: &[u8],
    compression: WriteCompression,
) -> Result<Vec<(u64, u64)>> {
    let spp = if meta.is_rgb { meta.size_c.max(1) } else { 1 } as usize;
    if meta.is_rgb && !meta.is_interleaved && spp > 1 {
        let channel_len = expected_plane_len_for_dims(meta, width, height)? / spp;
        let mut strips = Vec::with_capacity(spp);
        for channel in 0..spp {
            let start = channel * channel_len;
            let end = start + channel_len;
            let compressed = compress(&data[start..end], compression)?;
            let offset = w.seek(SeekFrom::Current(0)).map_err(BioFormatsError::Io)?;
            w.write_all(&compressed).map_err(BioFormatsError::Io)?;
            strips.push((offset, compressed.len() as u64));
        }
        Ok(strips)
    } else {
        let compressed = compress(data, compression)?;
        let offset = w.seek(SeekFrom::Current(0)).map_err(BioFormatsError::Io)?;
        w.write_all(&compressed).map_err(BioFormatsError::Io)?;
        Ok(vec![(offset, compressed.len() as u64)])
    }
}

fn pyramid_level_dimensions(meta: &ImageMetadata, level_idx: usize) -> Result<(u32, u32)> {
    let scale = 1u32.checked_shl(level_idx as u32).ok_or_else(|| {
        BioFormatsError::Format(format!(
            "Pyramid TIFF writer: resolution level {level_idx} scale overflows"
        ))
    })?;
    Ok((
        meta.size_x.div_ceil(scale).max(1),
        meta.size_y.div_ceil(scale).max(1),
    ))
}

fn validate_pyramid_levels(meta: &ImageMetadata, levels: &[Vec<Vec<u8>>]) -> Result<()> {
    let expected_planes = expected_plane_count(meta)? as usize;
    for (level_idx, level) in levels.iter().enumerate() {
        if level.len() != expected_planes {
            return Err(BioFormatsError::Format(format!(
                "Pyramid TIFF writer: resolution level {level_idx} has {} planes, expected {expected_planes}",
                level.len()
            )));
        }

        let (width, height) = pyramid_level_dimensions(meta, level_idx)?;
        let expected_len = expected_plane_len_for_dims(meta, width, height)?;
        for (plane_idx, plane) in level.iter().enumerate() {
            if plane.len() != expected_len {
                return Err(BioFormatsError::Format(format!(
                    "Pyramid TIFF writer: resolution level {level_idx} plane {plane_idx} has {} bytes, expected {expected_len} for {width}x{height}",
                    plane.len()
                )));
            }
        }
    }
    Ok(())
}

// ---- FormatWriter impl -------------------------------------------------------

/// Pyramid OME-TIFF writer — writes a main image plus sub-resolution levels
/// linked via SubIFD tags (tag 330), with OME-XML in ImageDescription.
pub struct PyramidOmeTiffWriter {
    compression: WriteCompression,
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    ome_xml: Option<String>,
    /// Pixel data per resolution level. Level 0 = full res.
    levels: Vec<Vec<Vec<u8>>>, // levels[resolution][plane] = bytes
}

impl PyramidOmeTiffWriter {
    pub fn new() -> Self {
        PyramidOmeTiffWriter {
            compression: WriteCompression::default(),
            path: None,
            meta: None,
            ome_xml: None,
            levels: Vec::new(),
        }
    }

    pub fn with_compression(mut self, c: WriteCompression) -> Self {
        self.compression = c;
        self
    }

    pub fn with_ome_xml(mut self, xml: String) -> Self {
        self.ome_xml = Some(xml);
        self
    }

    /// Add a resolution level. Level 0 (full res) should be added first via `save_bytes`;
    /// call this method for each subsequent lower resolution level.
    /// `planes` is a Vec of raw pixel data, one entry per plane.
    pub fn add_resolution_level(&mut self, planes: Vec<Vec<u8>>) {
        self.levels.push(planes);
    }

    /// Write the pyramid OME-TIFF file. The main image planes must have been
    /// supplied through `save_bytes` (stored as level 0), and additional
    /// resolution levels via `add_resolution_level`.
    pub fn write_pyramid(&mut self) -> Result<()> {
        let meta = self
            .meta
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?
            .clone();
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;

        if self.levels.is_empty() {
            return Err(BioFormatsError::Format(
                "No resolution levels provided".into(),
            ));
        }
        validate_pyramid_levels(&meta, &self.levels)?;

        let f = File::create(path).map_err(BioFormatsError::Io)?;
        let mut w = BufWriter::new(f);

        // Write TIFF header
        w.write_all(b"II").map_err(BioFormatsError::Io)?;
        write_le_u16(&mut w, 42).map_err(BioFormatsError::Io)?;
        write_le_u32(&mut w, 0).map_err(BioFormatsError::Io)?; // placeholder for IFD offset

        let comp_tag = compression_tag(self.compression);
        let spp = if meta.is_rgb { meta.size_c } else { 1 } as u16;
        let bps = bits_per_sample_value(meta.pixel_type);
        let sf = sample_format(meta.pixel_type);
        let photometric: u16 = if meta.is_rgb { 2 } else { 1 };

        // Write all strip data first, recording offsets
        // strip_info[level][plane] = (offset, byte_count)
        let mut strip_info: Vec<Vec<(u64, u64)>> = Vec::new();
        for level in &self.levels {
            let mut level_strips = Vec::new();
            for plane_data in level {
                let compressed = compress(plane_data, self.compression)?;
                let offset = w.seek(SeekFrom::Current(0)).map_err(BioFormatsError::Io)?;
                w.write_all(&compressed).map_err(BioFormatsError::Io)?;
                level_strips.push((offset, compressed.len() as u64));
            }
            strip_info.push(level_strips);
        }

        // Now write IFDs. Strategy: write level-0 IFD(s) as main IFDs,
        // then write sub-resolution IFDs and link them via SubIFD tag in level-0 IFD.
        // For simplicity, we write one main IFD per level-0 plane, and for each
        // main IFD, if there are sub-levels, we reference the corresponding plane's
        // sub-IFDs via tag 330.

        let num_sub_levels = self.levels.len() - 1;
        let level0_planes = strip_info[0].len();

        // First, write the sub-IFDs so we know their offsets
        // sub_ifd_offsets[main_plane][sub_level] = file offset of sub-IFD
        let mut sub_ifd_offsets: Vec<Vec<u64>> = Vec::new();

        // Write sub-level IFDs (levels 1..N)
        for _plane_idx in 0..level0_planes {
            sub_ifd_offsets.push(Vec::new());
        }

        // For each sub-level, write one IFD per plane
        // sub_ifd_file_offsets[sub_level_idx][plane_idx]
        let mut sub_ifd_file_offsets: Vec<Vec<u64>> = Vec::new();
        for level_idx in 1..self.levels.len() {
            let mut level_offsets = Vec::new();
            let level_planes = strip_info[level_idx].len();
            // Estimate sub-image dimensions (halved per level)
            let (sub_width, sub_height) = pyramid_level_dimensions(&meta, level_idx)?;

            for plane_idx in 0..level_planes {
                let (strip_offset, strip_byte_count) = strip_info[level_idx][plane_idx];
                let ifd_offset = w.seek(SeekFrom::Current(0)).map_err(BioFormatsError::Io)?;
                level_offsets.push(ifd_offset);

                // Build out-of-line (extra) data for this sub-IFD.
                // When spp > 1, BitsPerSample (258) must be an spp-element SHORT
                // array written out-of-line, mirroring the main IFD.
                let mut extra: Vec<u8> = Vec::new();
                if spp > 1 {
                    for _ in 0..spp {
                        extra.extend_from_slice(&bps.to_le_bytes());
                    }
                }

                // Write a minimal IFD for this sub-resolution plane
                let mut entries: Vec<Entry> = vec![
                    long_entry(256, sub_width),
                    long_entry(257, sub_height),
                    if spp == 1 {
                        short_entry(258, bps)
                    } else {
                        Entry {
                            tag: 258,
                            typ: short_type().0,
                            count: spp as u32,
                            value_or_offset: 0, // patched below
                        }
                    },
                    short_entry(259, comp_tag),
                    short_entry(262, photometric),
                    Entry {
                        tag: 273,
                        typ: long_type().0,
                        count: 1,
                        value_or_offset: classic_tiff_u32(strip_offset, "strip offset")?,
                    },
                    short_entry(277, spp),
                    long_entry(278, sub_height),
                    Entry {
                        tag: 279,
                        typ: long_type().0,
                        count: 1,
                        value_or_offset: classic_tiff_u32(strip_byte_count, "strip byte count")?,
                    },
                    short_entry(284, 1),
                ];
                // NewSubfileType = 1 (reduced resolution)
                entries.push(long_entry(254, 1));
                if sf != 1 {
                    entries.push(short_entry(339, sf));
                }
                entries.sort_by_key(|e| e.tag);

                let entry_count = entries.len() as u16;
                let ifd_data_len = 2 + entries.len() * 12 + 4; // count + entries + next-IFD

                // Serialize the IFD into a buffer so out-of-line offsets can be patched.
                let mut ifd_bytes: Vec<u8> = Vec::new();
                ifd_bytes.extend_from_slice(&entry_count.to_le_bytes());
                for e in &entries {
                    ifd_bytes.extend_from_slice(&e.tag.to_le_bytes());
                    ifd_bytes.extend_from_slice(&e.typ.to_le_bytes());
                    ifd_bytes.extend_from_slice(&e.count.to_le_bytes());
                    ifd_bytes.extend_from_slice(&e.value_or_offset.to_le_bytes());
                }
                // Next IFD = 0 (sub-IFDs are not chained)
                ifd_bytes.extend_from_slice(&0u32.to_le_bytes());

                // Extra (out-of-line) data is written immediately after the IFD.
                let extra_file_off = ifd_offset + ifd_data_len as u64;

                // Patch the BitsPerSample (258) entry to point at the out-of-line array.
                let ec = u16::from_le_bytes([ifd_bytes[0], ifd_bytes[1]]) as usize;
                for i in 0..ec {
                    let off = 2 + i * 12;
                    let tag = u16::from_le_bytes([ifd_bytes[off], ifd_bytes[off + 1]]);
                    if tag == 258 {
                        let count = u32::from_le_bytes([
                            ifd_bytes[off + 4],
                            ifd_bytes[off + 5],
                            ifd_bytes[off + 6],
                            ifd_bytes[off + 7],
                        ]);
                        if count > 1 {
                            let abs_off = classic_tiff_u32(extra_file_off, "sub-IFD BPS offset")?;
                            ifd_bytes[off + 8..off + 12].copy_from_slice(&abs_off.to_le_bytes());
                        }
                    }
                }

                w.write_all(&ifd_bytes).map_err(BioFormatsError::Io)?;
                w.write_all(&extra).map_err(BioFormatsError::Io)?;
            }
            sub_ifd_file_offsets.push(level_offsets);
        }

        // Record sub-IFD offsets per main plane
        for plane_idx in 0..level0_planes {
            for sub_level in &sub_ifd_file_offsets {
                if plane_idx < sub_level.len() {
                    sub_ifd_offsets[plane_idx].push(sub_level[plane_idx]);
                }
            }
        }

        // Now write the main (level 0) IFDs
        let first_ifd_offset = w.seek(SeekFrom::Current(0)).map_err(BioFormatsError::Io)?;
        let mut main_ifd_offsets: Vec<u64> = Vec::new();

        for plane_idx in 0..level0_planes {
            let ifd_start = w.seek(SeekFrom::Current(0)).map_err(BioFormatsError::Io)?;
            main_ifd_offsets.push(ifd_start);

            let (strip_offset, strip_byte_count) = strip_info[0][plane_idx];

            // Build extra data for this IFD
            let mut extra: Vec<u8> = Vec::new();

            // BitsPerSample array (if spp > 1)
            if spp > 1 {
                for _ in 0..spp {
                    extra.extend_from_slice(&bps.to_le_bytes());
                }
            }

            // ImageDescription (OME-XML) for first IFD only
            let desc_extra_offset = extra.len() as u32;
            let desc_bytes: Option<Vec<u8>> = if plane_idx == 0 {
                self.ome_xml.as_ref().map(|xml| {
                    let mut b = xml.as_bytes().to_vec();
                    b.push(0);
                    b
                })
            } else {
                None
            };
            if let Some(ref db) = desc_bytes {
                extra.extend_from_slice(db);
            }

            // XResolution and YResolution rationals
            let _xres_extra_offset = extra.len() as u32;
            extra.extend_from_slice(&72u32.to_le_bytes());
            extra.extend_from_slice(&1u32.to_le_bytes());
            let _yres_extra_offset = extra.len() as u32;
            extra.extend_from_slice(&72u32.to_le_bytes());
            extra.extend_from_slice(&1u32.to_le_bytes());

            // SubIFD offsets array (if we have sub-levels)
            let sub_ifd_extra_offset = extra.len() as u32;
            let sub_offsets = &sub_ifd_offsets[plane_idx];
            if num_sub_levels > 0 {
                for &off in sub_offsets {
                    extra.extend_from_slice(&classic_tiff_u32(off, "SubIFD offset")?.to_le_bytes());
                }
            }

            // Build entries
            let mut entries: Vec<Entry> =
                vec![long_entry(256, meta.size_x), long_entry(257, meta.size_y)];
            if spp == 1 {
                entries.push(short_entry(258, bps));
            } else {
                entries.push(Entry {
                    tag: 258,
                    typ: short_type().0,
                    count: spp as u32,
                    value_or_offset: 0,
                });
            }
            entries.push(short_entry(259, comp_tag));
            entries.push(short_entry(262, photometric));
            entries.push(Entry {
                tag: 273,
                typ: long_type().0,
                count: 1,
                value_or_offset: classic_tiff_u32(strip_offset, "strip offset")?,
            });
            entries.push(short_entry(277, spp));
            entries.push(long_entry(278, meta.size_y));
            entries.push(Entry {
                tag: 279,
                typ: long_type().0,
                count: 1,
                value_or_offset: classic_tiff_u32(strip_byte_count, "strip byte count")?,
            });
            entries.push(Entry {
                tag: 282,
                typ: rational_type().0,
                count: 1,
                value_or_offset: 0,
            });
            entries.push(Entry {
                tag: 283,
                typ: rational_type().0,
                count: 1,
                value_or_offset: 0,
            });
            entries.push(short_entry(284, 1));
            entries.push(short_entry(296, 2));

            if sf != 1 {
                entries.push(short_entry(339, sf));
            }

            if let Some(ref _db) = desc_bytes {
                entries.push(Entry {
                    tag: 270,
                    typ: 2,
                    count: desc_bytes.as_ref().unwrap().len() as u32,
                    value_or_offset: 0,
                });
            }

            // SubIFD tag (330) — IFD type
            if num_sub_levels > 0 && !sub_offsets.is_empty() {
                entries.push(Entry {
                    tag: 330,
                    typ: long_type().0,
                    count: sub_offsets.len() as u32,
                    value_or_offset: 0, // patched below
                });
            }

            entries.sort_by_key(|e| e.tag);

            let entry_count = entries.len() as u16;
            let ifd_data_len = 2 + entries.len() * 12 + 4; // count + entries + next-IFD

            // Write IFD entries (we'll need to patch offsets)
            let mut ifd_bytes: Vec<u8> = Vec::new();
            ifd_bytes.extend_from_slice(&entry_count.to_le_bytes());
            for e in &entries {
                ifd_bytes.extend_from_slice(&e.tag.to_le_bytes());
                ifd_bytes.extend_from_slice(&e.typ.to_le_bytes());
                ifd_bytes.extend_from_slice(&e.count.to_le_bytes());
                ifd_bytes.extend_from_slice(&e.value_or_offset.to_le_bytes());
            }
            // Next IFD placeholder
            ifd_bytes.extend_from_slice(&0u32.to_le_bytes());

            let extra_file_off = ifd_start + ifd_data_len as u64;

            // Patch offsets in IFD entries
            let ec = u16::from_le_bytes([ifd_bytes[0], ifd_bytes[1]]) as usize;
            for i in 0..ec {
                let off = 2 + i * 12;
                let tag = u16::from_le_bytes([ifd_bytes[off], ifd_bytes[off + 1]]);
                match tag {
                    258 => {
                        let count = u32::from_le_bytes([
                            ifd_bytes[off + 4],
                            ifd_bytes[off + 5],
                            ifd_bytes[off + 6],
                            ifd_bytes[off + 7],
                        ]);
                        if count > 1 {
                            let abs_off = classic_tiff_u32(extra_file_off, "extra-data offset")?;
                            ifd_bytes[off + 8..off + 12].copy_from_slice(&abs_off.to_le_bytes());
                        }
                    }
                    270 => {
                        let abs_off = classic_tiff_u32(
                            extra_file_off + desc_extra_offset as u64,
                            "ImageDescription offset",
                        )?;
                        ifd_bytes[off + 8..off + 12].copy_from_slice(&abs_off.to_le_bytes());
                    }
                    282 => {
                        let bps_extra = if spp > 1 { spp as u64 * 2 } else { 0 };
                        let desc_extra = desc_bytes.as_ref().map(|d| d.len() as u64).unwrap_or(0);
                        let abs_off = classic_tiff_u32(
                            extra_file_off + bps_extra + desc_extra,
                            "XResolution offset",
                        )?;
                        ifd_bytes[off + 8..off + 12].copy_from_slice(&abs_off.to_le_bytes());
                    }
                    283 => {
                        let bps_extra = if spp > 1 { spp as u64 * 2 } else { 0 };
                        let desc_extra = desc_bytes.as_ref().map(|d| d.len() as u64).unwrap_or(0);
                        let abs_off = classic_tiff_u32(
                            extra_file_off + bps_extra + desc_extra + 8,
                            "YResolution offset",
                        )?;
                        ifd_bytes[off + 8..off + 12].copy_from_slice(&abs_off.to_le_bytes());
                    }
                    330 => {
                        if sub_offsets.len() == 1 {
                            // Inline single offset
                            ifd_bytes[off + 8..off + 12].copy_from_slice(
                                &classic_tiff_u32(sub_offsets[0], "SubIFD offset")?.to_le_bytes(),
                            );
                        } else {
                            let abs_off = classic_tiff_u32(
                                extra_file_off + sub_ifd_extra_offset as u64,
                                "SubIFD offset-array offset",
                            )?;
                            ifd_bytes[off + 8..off + 12].copy_from_slice(&abs_off.to_le_bytes());
                        }
                    }
                    _ => {}
                }
            }

            // Patch next-IFD offset
            let next_ifd: u32 = if plane_idx + 1 < level0_planes {
                // We need to compute where the next IFD will be
                classic_tiff_u32(
                    ifd_start + ifd_data_len as u64 + extra.len() as u64,
                    "next IFD offset",
                )?
            } else {
                0
            };
            let last = ifd_bytes.len() - 4;
            ifd_bytes[last..].copy_from_slice(&next_ifd.to_le_bytes());

            w.write_all(&ifd_bytes).map_err(BioFormatsError::Io)?;
            w.write_all(&extra).map_err(BioFormatsError::Io)?;
        }

        // Patch header with first IFD offset
        w.seek(SeekFrom::Start(4)).map_err(BioFormatsError::Io)?;
        write_le_u32(
            &mut w,
            classic_tiff_u32(first_ifd_offset, "first IFD offset")?,
        )
        .map_err(BioFormatsError::Io)?;

        w.flush().map_err(BioFormatsError::Io)?;
        self.path = None;
        self.meta = None;
        Ok(())
    }
}

impl Default for PyramidOmeTiffWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatWriter for PyramidOmeTiffWriter {
    fn is_this_type(&self, path: &Path) -> bool {
        has_ome_tiff_suffix(path)
    }

    fn set_metadata(&mut self, meta: &ImageMetadata) -> Result<()> {
        validate_tiff_writer_metadata(meta, false)?;
        self.meta = Some(meta.clone());
        Ok(())
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.meta.as_ref().ok_or_else(|| {
            BioFormatsError::Format("set_metadata must be called before set_id".into())
        })?;
        self.path = Some(path.to_path_buf());
        self.levels.clear();
        Ok(())
    }

    fn save_bytes(&mut self, plane_index: u32, data: &[u8]) -> Result<()> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        // Accumulate into level 0
        if self.levels.is_empty() {
            self.levels.push(Vec::new());
        }
        let expected_plane = self.levels[0].len() as u32;
        if plane_index != expected_plane {
            return Err(BioFormatsError::Format(format!(
                "Pyramid TIFF writer: planes must be written in order; expected {expected_plane}, got {plane_index}"
            )));
        }
        let expected_count = expected_plane_count(meta)?;
        if plane_index >= expected_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let expected_len = expected_plane_len(meta)?;
        if data.len() < expected_len {
            return Err(BioFormatsError::Format(format!(
                "Pyramid TIFF writer: level 0 plane {plane_index} has {} bytes, expected {expected_len} bytes or more",
                data.len()
            )));
        }
        self.levels[0].push(data[..expected_len].to_vec());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.write_pyramid()
    }

    fn can_do_stacks(&self) -> bool {
        true
    }
}

impl FormatWriter for TiffWriter {
    fn is_this_type(&self, path: &Path) -> bool {
        has_tiff_suffix(path)
    }

    fn set_metadata(&mut self, meta: &ImageMetadata) -> Result<()> {
        validate_tiff_writer_metadata(meta, true)?;
        self.meta = Some(meta.clone());
        if self.auto_ome_xml && self.ome_xml.is_none() {
            let ome = crate::common::ome_metadata::OmeMetadata::from_image_metadata(meta);
            self.ome_xml = Some(ome.to_ome_xml(meta));
        }
        Ok(())
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.meta.as_ref().ok_or_else(|| {
            BioFormatsError::Format("set_metadata must be called before set_id".into())
        })?;
        let f = File::create(path).map_err(BioFormatsError::Io)?;
        let mut w = BufWriter::new(f);

        // Write TIFF header: II (LE), magic 42, placeholder IFD offset = 8
        w.write_all(b"II").map_err(BioFormatsError::Io)?;
        write_le_u16(&mut w, 42).map_err(BioFormatsError::Io)?;
        write_le_u32(&mut w, 8).map_err(BioFormatsError::Io)?; // IFD offset — will patch in close()

        self.path = Some(path.to_path_buf());
        self.file = Some(w);
        self.plane_strips.clear();
        self.planes_written = 0;
        Ok(())
    }

    fn save_bytes(&mut self, plane_index: u32, data: &[u8]) -> Result<()> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index != self.planes_written {
            return Err(BioFormatsError::Format(format!(
                "TIFF writer: planes must be written in order; expected {}, got {}",
                self.planes_written, plane_index
            )));
        }
        let expected_count = expected_plane_count(meta)?;
        if plane_index >= expected_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }

        let expected_len = expected_plane_len(meta)?;
        if data.len() < expected_len {
            return Err(BioFormatsError::Format(format!(
                "TIFF writer: plane {} has {} bytes, expected {} bytes or more",
                plane_index,
                data.len(),
                expected_len
            )));
        }

        let w = self.file.as_mut().ok_or(BioFormatsError::NotInitialized)?;

        let strips = write_plane_strips(
            w,
            meta,
            meta.size_x,
            meta.size_y,
            &data[..expected_len],
            self.compression,
        )?;
        self.plane_strips.push(strips);
        self.planes_written += 1;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let expected_count = expected_plane_count(&meta)?;
        if self.planes_written != expected_count {
            return Err(BioFormatsError::Format(format!(
                "TIFF writer: wrote {} planes, expected {}",
                self.planes_written, expected_count
            )));
        }
        let w = self.file.as_mut().ok_or(BioFormatsError::NotInitialized)?;

        let spp = if meta.is_rgb { meta.size_c } else { 1 } as u16;
        let bps = bits_per_sample_value(meta.pixel_type);
        let sf = sample_format(meta.pixel_type);
        let comp_tag = compression_tag(self.compression);
        let photometric: u16 = if meta.is_rgb { 2 } else { 1 }; // RGB=2, MinIsBlack=1

        // We write all IFDs after the image data.
        // Each IFD may need extra data (BitsPerSample array if spp>1, rational for resolution).

        let plane_count = self.plane_strips.len();

        // We collect the IFDs in reverse so we can chain them.
        // Gather byte blobs for each IFD.
        struct IfdBlob {
            ifd_bytes: Vec<u8>,
            extra_bytes: Vec<u8>,
            desc_offset: u32,
            bps_offset: u32,
            xres_offset: u32,
            yres_offset: u32,
        }

        let mut ifd_blobs: Vec<IfdBlob> = Vec::with_capacity(plane_count);

        for plane_idx in 0..plane_count {
            let strips = &self.plane_strips[plane_idx];
            let strip_offsets: Vec<u64> = strips.iter().map(|&(offset, _)| offset).collect();
            let strip_byte_counts: Vec<u64> = strips.iter().map(|&(_, count)| count).collect();
            let planar_configuration = if meta.is_rgb && !meta.is_interleaved && spp > 1 {
                2
            } else {
                1
            };

            // Build extra data (placed right after the IFD).
            // We'll store BitsPerSample array here if spp > 1, and resolution rationals.
            let mut extra: Vec<u8> = Vec::new();

            // IFD entry count (2 bytes) + entries (12 each) + next IFD offset (4 bytes)
            // We'll compute the IFD offset for this pass.
            // Pass 1: collect entries that need offsets.

            // BitsPerSample: if spp == 1, store inline; if > 1, needs offset
            let bps_offset_placeholder: u32; // offset into extra where BPS array lives
            let bps_entry;
            if spp == 1 {
                bps_entry = short_entry(258, bps);
                bps_offset_placeholder = 0;
            } else {
                bps_offset_placeholder = extra.len() as u32;
                for _ in 0..spp {
                    extra.extend_from_slice(&bps.to_le_bytes());
                }
                bps_entry = Entry {
                    tag: 258,
                    typ: short_type().0,
                    count: spp as u32,
                    value_or_offset: 0, /* filled later */
                };
            }

            // ImageDescription (OME-XML) for the first IFD only
            let desc_offset = extra.len() as u32;
            let desc_bytes: Option<Vec<u8>> = if plane_idx == 0 {
                self.ome_xml.as_ref().map(|xml| {
                    let mut b = xml.as_bytes().to_vec();
                    b.push(0); // NUL terminator for ASCII tag
                    b
                })
            } else {
                None
            };
            if let Some(ref db) = desc_bytes {
                extra.extend_from_slice(db);
            }

            // XResolution and YResolution rationals (72/1)
            let xres_offset = extra.len() as u32;
            extra.extend_from_slice(&72u32.to_le_bytes());
            extra.extend_from_slice(&1u32.to_le_bytes());
            let yres_offset = extra.len() as u32;
            extra.extend_from_slice(&72u32.to_le_bytes());
            extra.extend_from_slice(&1u32.to_le_bytes());

            // Build sorted entry list
            let mut entries: Vec<Entry> = vec![
                long_entry(256, meta.size_x),
                long_entry(257, meta.size_y),
                bps_entry,
                short_entry(259, comp_tag),
                short_entry(262, photometric),
                long_array_entry(273, &strip_offsets, &mut extra)?,
                short_entry(277, spp as u16),
                long_entry(278, meta.size_y), // RowsPerStrip = full image height
                long_array_entry(279, &strip_byte_counts, &mut extra)?,
                Entry {
                    tag: 282,
                    typ: rational_type().0,
                    count: 1,
                    value_or_offset: 0,
                }, // XResolution
                Entry {
                    tag: 283,
                    typ: rational_type().0,
                    count: 1,
                    value_or_offset: 0,
                }, // YResolution
                short_entry(284, planar_configuration),
                short_entry(296, 2), // ResolutionUnit = inch
            ];

            // Add SampleFormat if not default (unsigned int = 1)
            if sf != 1 {
                entries.push(short_entry(339, sf));
            }

            // ImageDescription (tag 270) for OME-TIFF
            if let Some(ref db) = desc_bytes {
                entries.push(Entry {
                    tag: 270,
                    typ: 2, // ASCII
                    count: db.len() as u32,
                    value_or_offset: 0, // patched later
                });
            }

            entries.sort_by_key(|e| e.tag);

            // We'll write the IFD blob (we don't know the file offset yet, so we record
            // where the extra data is *relative to the IFD start*, then patch at write time).
            // Build the raw IFD bytes with placeholder offsets for extra data.
            let mut ifd_bytes: Vec<u8> = Vec::new();
            let entry_count = entries.len() as u16;
            ifd_bytes.extend_from_slice(&entry_count.to_le_bytes());

            for e in &entries {
                ifd_bytes.extend_from_slice(&e.tag.to_le_bytes());
                ifd_bytes.extend_from_slice(&e.typ.to_le_bytes());
                ifd_bytes.extend_from_slice(&e.count.to_le_bytes());
                ifd_bytes.extend_from_slice(&e.value_or_offset.to_le_bytes());
            }

            // Append next IFD placeholder (4 bytes)
            ifd_bytes.extend_from_slice(&0u32.to_le_bytes());

            ifd_blobs.push(IfdBlob {
                ifd_bytes,
                extra_bytes: extra,
                desc_offset,
                bps_offset: bps_offset_placeholder,
                xres_offset,
                yres_offset,
            });

            // Remember what we need to patch later:
            // - BitsPerSample offset (if spp > 1)
            // - XResolution offset
            // - YResolution offset
            // We'll do a second pass once we know the IFD file offsets.
        }

        // Now write IFDs to the file and patch offsets.
        // Write IFD chain: IFD0 extra0 IFD1 extra1 ...
        let first_ifd_file_offset = w.seek(SeekFrom::Current(0)).map_err(BioFormatsError::Io)?;

        let mut ifd_file_offsets: Vec<u64> = Vec::with_capacity(plane_count);
        let mut cursor = first_ifd_file_offset;
        for blob in &ifd_blobs {
            ifd_file_offsets.push(cursor);
            cursor += blob.ifd_bytes.len() as u64 + blob.extra_bytes.len() as u64;
        }

        for (plane_idx, blob) in ifd_blobs.iter_mut().enumerate() {
            let ifd_file_off = ifd_file_offsets[plane_idx];
            let extra_file_off = ifd_file_off + blob.ifd_bytes.len() as u64;

            // Patch the entry values that point into extra data.
            // We need to walk through entries again.
            // IFD layout: 2-byte count, then 12-byte entries.
            let entry_count = u16::from_le_bytes([blob.ifd_bytes[0], blob.ifd_bytes[1]]) as usize;
            for i in 0..entry_count {
                let off = 2 + i * 12;
                let tag = u16::from_le_bytes([blob.ifd_bytes[off], blob.ifd_bytes[off + 1]]);
                match tag {
                    258 => {
                        // BitsPerSample — patch to file offset of BPS array in extra
                        // (only if count > 1)
                        let count = u32::from_le_bytes([
                            blob.ifd_bytes[off + 4],
                            blob.ifd_bytes[off + 5],
                            blob.ifd_bytes[off + 6],
                            blob.ifd_bytes[off + 7],
                        ]);
                        if count > 1 {
                            let abs_off = classic_tiff_u32(
                                extra_file_off + blob.bps_offset as u64,
                                "BitsPerSample offset",
                            )?;
                            blob.ifd_bytes[off + 8..off + 12]
                                .copy_from_slice(&abs_off.to_le_bytes());
                        }
                    }
                    282 => {
                        let abs_off = classic_tiff_u32(
                            extra_file_off + blob.xres_offset as u64,
                            "XResolution offset",
                        )?;
                        blob.ifd_bytes[off + 8..off + 12].copy_from_slice(&abs_off.to_le_bytes());
                    }
                    283 => {
                        let abs_off = classic_tiff_u32(
                            extra_file_off + blob.yres_offset as u64,
                            "YResolution offset",
                        )?;
                        blob.ifd_bytes[off + 8..off + 12].copy_from_slice(&abs_off.to_le_bytes());
                    }
                    270 => {
                        // ImageDescription — offset into extra data
                        let abs_off = classic_tiff_u32(
                            extra_file_off + blob.desc_offset as u64,
                            "ImageDescription offset",
                        )?;
                        blob.ifd_bytes[off + 8..off + 12].copy_from_slice(&abs_off.to_le_bytes());
                    }
                    273 | 279 => {
                        let count = u32::from_le_bytes([
                            blob.ifd_bytes[off + 4],
                            blob.ifd_bytes[off + 5],
                            blob.ifd_bytes[off + 6],
                            blob.ifd_bytes[off + 7],
                        ]);
                        if count > 1 {
                            patch_extra_offset(&mut blob.ifd_bytes, off, extra_file_off)?;
                        }
                    }
                    // Single StripOffsets / StripByteCounts are already absolute.
                    _ => {}
                }
            }

            // Patch next-IFD offset (last 4 bytes of ifd_bytes)
            let next_ifd: u32 = if plane_idx + 1 < plane_count {
                classic_tiff_u32(ifd_file_offsets[plane_idx + 1], "next IFD offset")?
            } else {
                0
            };
            let last = blob.ifd_bytes.len() - 4;
            blob.ifd_bytes[last..].copy_from_slice(&next_ifd.to_le_bytes());

            w.write_all(&blob.ifd_bytes).map_err(BioFormatsError::Io)?;
            w.write_all(&blob.extra_bytes)
                .map_err(BioFormatsError::Io)?;
        }

        // Patch header: write first_ifd_file_offset at byte 4
        w.seek(SeekFrom::Start(4)).map_err(BioFormatsError::Io)?;
        write_le_u32(
            w,
            classic_tiff_u32(first_ifd_file_offset, "first IFD offset")?,
        )
        .map_err(BioFormatsError::Io)?;

        w.flush().map_err(BioFormatsError::Io)?;
        self.file = None;
        self.meta = None;
        self.plane_strips.clear();
        self.planes_written = 0;
        Ok(())
    }

    fn can_do_stacks(&self) -> bool {
        true
    }
}

// =============================================================================
// In-place TIFF metadata overwrite — faithful port of Java
// loci.formats.tiff.TiffSaver: overwriteComment / overwriteIFDValue /
// overwriteLastIFDOffset and the makeValidIFD helper.
//
// These functions surgically edit an EXISTING TIFF file's IFD entry (e.g. the
// ImageDescription / OME-XML in IFD 0) in place: if the new value fits in the
// original allocation it is overwritten at its offset; otherwise the new value
// is appended at EOF and the entry's offset/count are rewritten to point there.
// This lets you update OME-XML in a TIFF without rewriting the pixel data.
//
// Strictly additive — none of the existing TiffWriter write paths are touched.
// =============================================================================

use std::io::Read;

/// A value to write into an IFD entry, mirroring the `Object` overloads handled
/// by Java `TiffSaver.writeIFDValue`. Only the variants needed for metadata
/// overwrite are modelled (this is the same set the Java method special-cases).
#[derive(Debug, Clone)]
pub enum TiffSaverValue {
    /// ASCII string (IFD type 2). A trailing NUL is appended, exactly like Java.
    Ascii(String),
    /// BYTE array (IFD type 1) — the Java `short[]` branch emits one byte per
    /// element via `writeByte`.
    ByteArray(Vec<u8>),
    /// SHORT array (IFD type 3).
    Short(Vec<u16>),
    /// LONG array (IFD type 4 in classic TIFF, LONG8/type 16 in BigTIFF).
    Long(Vec<u64>),
    /// FLOAT array (IFD type 11).
    Float(Vec<f32>),
    /// DOUBLE array (IFD type 12).
    Double(Vec<f64>),
}

/// Byte-order / BigTIFF context for the overwrite routines.
#[derive(Clone, Copy)]
struct TiffSaverCtx {
    little: bool,
    big_tiff: bool,
}

impl TiffSaverCtx {
    /// Mirrors Java `writeIntValue`: 8 bytes for BigTIFF, else 4.
    fn int_value_bytes(&self, v: u64) -> Vec<u8> {
        if self.big_tiff {
            if self.little {
                v.to_le_bytes().to_vec()
            } else {
                v.to_be_bytes().to_vec()
            }
        } else {
            let v = v as u32;
            if self.little {
                v.to_le_bytes().to_vec()
            } else {
                v.to_be_bytes().to_vec()
            }
        }
    }

    fn u16_bytes(&self, v: u16) -> [u8; 2] {
        if self.little {
            v.to_le_bytes()
        } else {
            v.to_be_bytes()
        }
    }

    fn read_u16(&self, b: &[u8]) -> u16 {
        if self.little {
            u16::from_le_bytes([b[0], b[1]])
        } else {
            u16::from_be_bytes([b[0], b[1]])
        }
    }

    fn read_u32(&self, b: &[u8]) -> u32 {
        if self.little {
            u32::from_le_bytes([b[0], b[1], b[2], b[3]])
        } else {
            u32::from_be_bytes([b[0], b[1], b[2], b[3]])
        }
    }

    fn read_u64(&self, b: &[u8]) -> u64 {
        let mut a = [0u8; 8];
        a.copy_from_slice(&b[..8]);
        if self.little {
            u64::from_le_bytes(a)
        } else {
            u64::from_be_bytes(a)
        }
    }
}

/// TIFF type element sizes in bytes — mirrors Java `IFDType.getBytesPerElement()`.
fn tiff_type_bytes(typ: u16) -> Option<u32> {
    Some(match typ {
        1 | 2 | 6 | 7 => 1,              // BYTE, ASCII, SBYTE, UNDEFINED
        3 | 8 => 2,                      // SHORT, SSHORT
        4 | 9 | 11 | 13 => 4,            // LONG, SLONG, FLOAT, IFD
        5 | 10 | 12 | 16 | 17 | 18 => 8, // RATIONAL, SRATIONAL, DOUBLE, LONG8, SLONG8, IFD8
        _ => return None,
    })
}

/// A parsed TIFF IFD directory entry (mirrors `TiffIFDEntry`): type, value
/// count, and the offset where the value lives. For in-line values
/// `value_offset` is the file position of the value field within the entry.
struct ParsedEntry {
    typ: u16,
    value_count: u64,
    value_offset: u64,
}

/// Mirror of Java `TiffParser.checkHeader`: returns (little_endian, big_tiff)
/// or an error if the header is not a valid TIFF.
fn check_tiff_header(buf: &[u8]) -> Result<(bool, bool)> {
    if buf.len() < 4 {
        return Err(BioFormatsError::Format("Invalid TIFF header".into()));
    }
    let little = match (buf[0], buf[1]) {
        (0x49, 0x49) => true,  // "II"
        (0x4D, 0x4D) => false, // "MM"
        _ => return Err(BioFormatsError::Format("Invalid TIFF header".into())),
    };
    let magic = if little {
        u16::from_le_bytes([buf[2], buf[3]])
    } else {
        u16::from_be_bytes([buf[2], buf[3]])
    };
    let big_tiff = match magic {
        42 => false,
        43 => true,
        _ => return Err(BioFormatsError::Format("Invalid TIFF header".into())),
    };
    Ok((little, big_tiff))
}

/// Read the first-IFD offset from a TIFF header.
fn first_ifd_offset(buf: &[u8], ctx: &TiffSaverCtx) -> Result<u64> {
    if ctx.big_tiff {
        if buf.len() < 16 {
            return Err(BioFormatsError::Format("Truncated BigTIFF header".into()));
        }
        Ok(ctx.read_u64(&buf[8..16]))
    } else {
        if buf.len() < 8 {
            return Err(BioFormatsError::Format("Truncated TIFF header".into()));
        }
        Ok(ctx.read_u32(&buf[4..8]) as u64)
    }
}

/// Walk the IFD chain and return every IFD's file offset, mirroring
/// `TiffParser.getIFDOffsets()`.
fn ifd_offsets(data: &[u8], ctx: &TiffSaverCtx) -> Result<Vec<u64>> {
    let bytes_per_entry: u64 = if ctx.big_tiff { 20 } else { 12 };
    let mut offsets = Vec::new();
    let mut offset = first_ifd_offset(data, ctx)?;
    let mut seen = std::collections::HashSet::new();
    while offset != 0 {
        if !seen.insert(offset) {
            // Defensive: avoid infinite loop on a cyclic/corrupt chain.
            break;
        }
        let o = offset as usize;
        if o + (if ctx.big_tiff { 8 } else { 2 }) > data.len() {
            return Err(BioFormatsError::Format("IFD offset past EOF".into()));
        }
        let num = if ctx.big_tiff {
            ctx.read_u64(&data[o..o + 8])
        } else {
            ctx.read_u16(&data[o..o + 2]) as u64
        };
        offsets.push(offset);
        // next-IFD pointer follows the directory entries
        let next_off =
            o + (if ctx.big_tiff { 8 } else { 2 }) as usize + (bytes_per_entry * num) as usize;
        if next_off + (if ctx.big_tiff { 8 } else { 4 }) > data.len() {
            break;
        }
        offset = if ctx.big_tiff {
            ctx.read_u64(&data[next_off..next_off + 8])
        } else {
            ctx.read_u32(&data[next_off..next_off + 4]) as u64
        };
    }
    Ok(offsets)
}

/// Read a single IFD directory entry from `data` at `pos`, mirroring
/// `TiffParser.readTiffIFDEntry`. The returned `value_offset` is, for in-line
/// values, the file offset of the value field; for out-of-line values it is the
/// offset the entry points to.
fn read_ifd_entry(data: &[u8], pos: u64, ctx: &TiffSaverCtx) -> Result<(u16, ParsedEntry)> {
    let p = pos as usize;
    let bytes_per_entry = if ctx.big_tiff { 20 } else { 12 };
    if p + bytes_per_entry > data.len() {
        return Err(BioFormatsError::Format("IFD entry past EOF".into()));
    }
    let tag = ctx.read_u16(&data[p..p + 2]);
    let typ = ctx.read_u16(&data[p + 2..p + 4]);
    let bytes_per_elem = tiff_type_bytes(typ)
        .ok_or_else(|| BioFormatsError::Format(format!("Unknown TIFF type {typ}")))?;
    let (value_count, count_field_bytes) = if ctx.big_tiff {
        (ctx.read_u64(&data[p + 4..p + 12]), 8usize)
    } else {
        (ctx.read_u32(&data[p + 4..p + 8]) as u64, 4usize)
    };
    let value_field_pos = pos + 4 + count_field_bytes as u64;
    let inline_capacity = if ctx.big_tiff { 8u64 } else { 4u64 };
    let total_bytes = value_count.saturating_mul(bytes_per_elem as u64);
    let value_offset = if total_bytes > inline_capacity {
        // out-of-line: the value field holds an offset
        if ctx.big_tiff {
            ctx.read_u64(&data[value_field_pos as usize..value_field_pos as usize + 8])
        } else {
            ctx.read_u32(&data[value_field_pos as usize..value_field_pos as usize + 4]) as u64
        }
    } else {
        // in-line: value lives in the entry itself
        value_field_pos
    };
    Ok((
        tag,
        ParsedEntry {
            typ,
            value_count,
            value_offset,
        },
    ))
}

/// Serialize the value bytes + canonical (type, count) for an IFD value,
/// mirroring Java `TiffSaver.writeIFDValue`. Returns `(new_type, new_count,
/// serialized_bytes)`. `serialized_bytes` is the raw value payload (without
/// padding); the caller decides in-line (fits inline capacity) vs out-of-line
/// using the same `extraBuf.length()==0` test Java uses.
fn serialize_ifd_value(ctx: &TiffSaverCtx, value: &TiffSaverValue) -> (u16, u64, Vec<u8>) {
    match value {
        TiffSaverValue::Ascii(s) => {
            // ASCII — Java writes the bytes plus a concluding NUL; count = len+1.
            let mut bytes = s.as_bytes().to_vec();
            bytes.push(0);
            let count = bytes.len() as u64;
            (2, count, bytes)
        }
        TiffSaverValue::ByteArray(q) => (1, q.len() as u64, q.clone()),
        TiffSaverValue::Short(q) => {
            let mut bytes = Vec::with_capacity(q.len() * 2);
            for &v in q {
                bytes.extend_from_slice(&ctx.u16_bytes(v));
            }
            (3, q.len() as u64, bytes)
        }
        TiffSaverValue::Long(q) => {
            let typ = if ctx.big_tiff { 16 } else { 4 };
            let mut bytes = Vec::new();
            for &v in q {
                bytes.extend_from_slice(&ctx.int_value_bytes(v));
            }
            (typ, q.len() as u64, bytes)
        }
        TiffSaverValue::Float(q) => {
            let mut bytes = Vec::with_capacity(q.len() * 4);
            for &v in q {
                if ctx.little {
                    bytes.extend_from_slice(&v.to_le_bytes());
                } else {
                    bytes.extend_from_slice(&v.to_be_bytes());
                }
            }
            (11, q.len() as u64, bytes)
        }
        TiffSaverValue::Double(q) => {
            let mut bytes = Vec::with_capacity(q.len() * 8);
            for &v in q {
                if ctx.little {
                    bytes.extend_from_slice(&v.to_le_bytes());
                } else {
                    bytes.extend_from_slice(&v.to_be_bytes());
                }
            }
            (12, q.len() as u64, bytes)
        }
    }
}

/// Core in-place overwrite of one IFD entry, a faithful port of Java
/// `TiffSaver.overwriteIFDValue(raf, ifdOffset, tag, value, skipHeaderCheck)`.
///
/// Locates the entry for `tag` within the IFD at `ifd_offset`, computes the new
/// entry fields, decides in-place vs append-to-EOF using Java's exact branch
/// logic, applies the edit to `data`, and writes the file back to `path`.
fn overwrite_ifd_value_at_offset(
    path: &Path,
    data: &mut Vec<u8>,
    ctx: &TiffSaverCtx,
    ifd_offset: u64,
    tag: u16,
    value: &TiffSaverValue,
) -> Result<()> {
    let bytes_per_entry: u64 = if ctx.big_tiff { 20 } else { 12 };
    let dir_count_bytes: u64 = if ctx.big_tiff { 8 } else { 2 };

    let o = ifd_offset as usize;
    if o + dir_count_bytes as usize > data.len() {
        return Err(BioFormatsError::Format("IFD offset past EOF".into()));
    }
    let num = if ctx.big_tiff {
        ctx.read_u64(&data[o..o + 8])
    } else {
        ctx.read_u16(&data[o..o + 2]) as u64
    };

    for i in 0..num {
        let entry_pos = ifd_offset + dir_count_bytes + bytes_per_entry * i;
        let (etag, entry) = read_ifd_entry(data, entry_pos, ctx)?;
        if etag != tag {
            continue;
        }

        // Build the new value's canonical fields + serialized bytes.
        let (new_type, new_count, serialized) = serialize_ifd_value(ctx, value);

        // In-line vs out-of-line, mirroring Java's `extraBuf.length() == 0`:
        // a value is in-line iff its serialized size fits the inline capacity.
        let inline_capacity: u64 = if ctx.big_tiff { 8 } else { 4 };
        let is_inline = serialized.len() as u64 <= inline_capacity;
        let extra: Vec<u8> = if is_inline {
            Vec::new()
        } else {
            serialized.clone()
        };

        // First overwrite the original value with 0s if it was out-of-line,
        // mirroring Java's `entry.getValueCount() > (offset / bytesPerElement)`
        // where `offset` is the IFD-offset constant (8 for BigTIFF else 4).
        let offset_const: u64 = if ctx.big_tiff { 8 } else { 4 };
        let old_elem_bytes = tiff_type_bytes(entry.typ).unwrap_or(1) as u64;
        if entry.value_count > (offset_const / old_elem_bytes.max(1)) {
            let start = entry.value_offset as usize;
            let zeros = (entry.value_count * old_elem_bytes) as usize;
            let end = (start + zeros).min(data.len());
            for b in data.iter_mut().take(end).skip(start) {
                *b = 0;
            }
        }

        // Determine the best way to overwrite the old entry (Java's branch).
        let raf_len = data.len() as u64;
        let mut new_offset = entry.value_offset;
        if extra.is_empty() {
            // new entry is inline; nothing to relocate.
        } else if entry.value_offset + entry.value_count * old_elem_bytes == raf_len {
            // old entry was already at EOF; overwrite it
            new_offset = entry.value_offset;
        } else if new_count <= entry.value_count {
            // new entry is as small or smaller than old entry; overwrite it
            new_offset = entry.value_offset;
        } else {
            // old entry was elsewhere; append to EOF, orphaning old entry
            new_offset = raf_len;
        }

        // Overwrite the old entry: Java seeks `entry_pos + 2` (past the tag) and
        // writes type(2), count(int), offset-or-inline-value(int).
        let mut field = Vec::new();
        field.extend_from_slice(&ctx.u16_bytes(new_type));
        field.extend_from_slice(&ctx.int_value_bytes(new_count));
        if extra.is_empty() {
            // in-line: write the value bytes padded to inline capacity
            let mut v = serialized;
            v.resize(inline_capacity as usize, 0);
            field.extend_from_slice(&v);
        } else {
            field.extend_from_slice(&ctx.int_value_bytes(new_offset));
        }
        let tail_len = bytes_per_entry as usize - 2;
        debug_assert_eq!(field.len(), tail_len);
        let fstart = (entry_pos + 2) as usize;
        data[fstart..fstart + tail_len].copy_from_slice(&field[..tail_len]);

        // Write out-of-line data at new_offset (may extend the file).
        if !extra.is_empty() {
            let start = new_offset as usize;
            let end = start + extra.len();
            if end > data.len() {
                data.resize(end, 0);
            }
            data[start..end].copy_from_slice(&extra);
        }

        std::fs::write(path, &*data).map_err(BioFormatsError::Io)?;
        return Ok(());
    }

    Err(BioFormatsError::Format(format!("Tag not found ({tag})")))
}

/// Surgically overwrite the value of `tag` in IFD number `ifd_index` of the TIFF
/// at `path`. Faithful port of Java
/// `TiffSaver.overwriteIFDValue(RandomAccessInputStream, int ifd, int tag, Object value)`.
///
/// If the new value fits in the original allocation it is written in place;
/// otherwise it is appended at EOF and the entry's offset/count are repointed.
pub fn overwrite_ifd_value(
    path: &Path,
    ifd_index: usize,
    tag: u16,
    value: TiffSaverValue,
) -> Result<()> {
    let mut data = std::fs::read(path).map_err(BioFormatsError::Io)?;
    let (little, big_tiff) = check_tiff_header(&data)?;
    let ctx = TiffSaverCtx { little, big_tiff };

    let offsets = ifd_offsets(&data, &ctx)?;
    if ifd_index >= offsets.len() {
        return Err(BioFormatsError::Format(format!(
            "No such IFD ({ifd_index} of {})",
            offsets.len()
        )));
    }
    let ifd_offset = offsets[ifd_index];
    overwrite_ifd_value_at_offset(path, &mut data, &ctx, ifd_offset, tag, &value)
}

/// Convenience method for overwriting a file's first ImageDescription
/// (tag 270, `IFD.IMAGE_DESCRIPTION`). Faithful port of Java
/// `TiffSaver.overwriteComment(in, value)`.
pub fn overwrite_comment(path: &Path, comment: &str) -> Result<()> {
    overwrite_ifd_value(path, 0, 270, TiffSaverValue::Ascii(comment.to_string()))
}

/// Overwrite the last IFD's next-IFD pointer with 0, faithful port of Java
/// `TiffSaver.overwriteLastIFDOffset`. After this call the IFD chain is
/// terminated at whatever was previously the last IFD in the file.
pub fn overwrite_last_ifd_offset(path: &Path) -> Result<()> {
    let mut data = std::fs::read(path).map_err(BioFormatsError::Io)?;
    let (little, big_tiff) = check_tiff_header(&data)?;
    let ctx = TiffSaverCtx { little, big_tiff };
    let bytes_per_entry: u64 = if ctx.big_tiff { 20 } else { 12 };
    let dir_count_bytes: u64 = if ctx.big_tiff { 8 } else { 2 };

    let offsets = ifd_offsets(&data, &ctx)?;
    let last = *offsets
        .last()
        .ok_or_else(|| BioFormatsError::Format("No IFDs in file".into()))?;
    let o = last as usize;
    let num = if ctx.big_tiff {
        ctx.read_u64(&data[o..o + 8])
    } else {
        ctx.read_u16(&data[o..o + 2]) as u64
    };
    // next-IFD pointer immediately follows the directory entries
    let next_ptr_pos = (last + dir_count_bytes + bytes_per_entry * num) as usize;
    let zero = ctx.int_value_bytes(0);
    if next_ptr_pos + zero.len() > data.len() {
        data.resize(next_ptr_pos + zero.len(), 0);
    }
    data[next_ptr_pos..next_ptr_pos + zero.len()].copy_from_slice(&zero);
    std::fs::write(path, &data).map_err(BioFormatsError::Io)?;
    Ok(())
}

/// Faithful port of Java `TiffSaver.makeValidIFD(ifd, pixelType, nChannels)`.
/// Fills in the mandatory pixel-related IFD fields for a freshly-built IFD map.
/// This operates on a tag→`IfdValue` map (the Rust analogue of Java's `IFD`),
/// and is provided for parity; the existing TiffWriter does not use an IFD map
/// for its own write path.
pub fn make_valid_ifd(
    ifd: &mut std::collections::HashMap<u16, crate::tiff::ifd::IfdValue>,
    pixel_type: PixelType,
    n_channels: u16,
) {
    use crate::tiff::ifd::IfdValue;
    // Tag constants (mirroring loci.formats.tiff.IFD).
    const BITS_PER_SAMPLE: u16 = 258;
    const COMPRESSION: u16 = 259;
    const PHOTOMETRIC_INTERPRETATION: u16 = 262;
    const SAMPLES_PER_PIXEL: u16 = 277;
    const X_RESOLUTION: u16 = 282;
    const Y_RESOLUTION: u16 = 283;
    const ROWS_PER_STRIP: u16 = 278;
    const SAMPLE_FORMAT: u16 = 339;
    const COLOR_MAP: u16 = 320;
    const SOFTWARE: u16 = 305;
    const IMAGE_DESCRIPTION: u16 = 270;
    const EXTRA_SAMPLES: u16 = 338;
    const TILE_WIDTH: u16 = 322;
    const TILE_LENGTH: u16 = 323;

    let bps = 8 * pixel_type.bytes_per_sample() as u16;
    ifd.insert(
        BITS_PER_SAMPLE,
        IfdValue::Short(vec![bps; n_channels as usize]),
    );

    if matches!(pixel_type, PixelType::Float32 | PixelType::Float64) {
        ifd.insert(SAMPLE_FORMAT, IfdValue::Short(vec![3]));
    }
    if !ifd.contains_key(&COMPRESSION) {
        // TiffCompression.UNCOMPRESSED == 1
        ifd.insert(COMPRESSION, IfdValue::Short(vec![1]));
    }

    // PhotoInterp: BLACK_IS_ZERO=1, RGB_PALETTE=3, RGB=2, Y_CB_CR=6
    let mut pi: u16 = 1;
    let compression_code = ifd.get(&COMPRESSION).and_then(|v| v.as_u16()).unwrap_or(1);
    if n_channels == 1 && ifd.contains_key(&COLOR_MAP) {
        pi = 3;
    } else if n_channels == 3 || n_channels == 4 {
        if compression_code == 7 {
            // TiffCompression.JPEG == 7
            pi = 6;
        } else {
            pi = 2;
        }
        if n_channels == 4 {
            ifd.insert(EXTRA_SAMPLES, IfdValue::Short(vec![0]));
        }
    }
    ifd.insert(PHOTOMETRIC_INTERPRETATION, IfdValue::Short(vec![pi]));
    ifd.insert(SAMPLES_PER_PIXEL, IfdValue::Short(vec![n_channels]));

    if !ifd.contains_key(&X_RESOLUTION) {
        ifd.insert(X_RESOLUTION, IfdValue::Rational(vec![(1, 1)]));
    }
    if !ifd.contains_key(&Y_RESOLUTION) {
        ifd.insert(Y_RESOLUTION, IfdValue::Rational(vec![(1, 1)]));
    }
    if !ifd.contains_key(&SOFTWARE) {
        ifd.insert(SOFTWARE, IfdValue::Ascii("bioformats-rs".to_string()));
    }
    if !ifd.contains_key(&ROWS_PER_STRIP)
        && !ifd.contains_key(&TILE_WIDTH)
        && !ifd.contains_key(&TILE_LENGTH)
    {
        ifd.insert(ROWS_PER_STRIP, IfdValue::Long(vec![1]));
    }
    if !ifd.contains_key(&IMAGE_DESCRIPTION) {
        ifd.insert(IMAGE_DESCRIPTION, IfdValue::Ascii(String::new()));
    }
}

/// Read the ImageDescription (tag 270) string from IFD 0 of a TIFF file.
/// Helper used by tests and callers verifying an overwrite round-trip.
pub fn read_first_comment(path: &Path) -> Result<Option<String>> {
    let mut data = Vec::new();
    File::open(path)
        .map_err(BioFormatsError::Io)?
        .read_to_end(&mut data)
        .map_err(BioFormatsError::Io)?;
    let (little, big_tiff) = check_tiff_header(&data)?;
    let ctx = TiffSaverCtx { little, big_tiff };
    let offsets = ifd_offsets(&data, &ctx)?;
    let Some(&ifd0) = offsets.first() else {
        return Ok(None);
    };
    let dir_count_bytes: u64 = if ctx.big_tiff { 8 } else { 2 };
    let bytes_per_entry: u64 = if ctx.big_tiff { 20 } else { 12 };
    let o = ifd0 as usize;
    let num = if ctx.big_tiff {
        ctx.read_u64(&data[o..o + 8])
    } else {
        ctx.read_u16(&data[o..o + 2]) as u64
    };
    for i in 0..num {
        let entry_pos = ifd0 + dir_count_bytes + bytes_per_entry * i;
        let (tag, entry) = read_ifd_entry(&data, entry_pos, &ctx)?;
        if tag == 270 {
            let start = entry.value_offset as usize;
            let mut len = entry.value_count as usize;
            let raw = &data[start..(start + len).min(data.len())];
            // strip trailing NUL(s)
            while len > 0 && raw.get(len - 1) == Some(&0) {
                len -= 1;
            }
            let s = String::from_utf8_lossy(&raw[..len.min(raw.len())]).into_owned();
            return Ok(Some(s));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod overwrite_tests {
    use super::*;
    use crate::common::metadata::ImageMetadata;
    use crate::common::pixel_type::PixelType;

    fn write_basic_tiff(path: &Path, comment: &str) {
        let mut meta = ImageMetadata::default();
        meta.size_x = 4;
        meta.size_y = 4;
        meta.size_c = 1;
        meta.size_z = 1;
        meta.size_t = 1;
        meta.image_count = 1;
        meta.pixel_type = PixelType::Uint8;
        let mut w = TiffWriter::new().with_ome_xml(comment.to_string());
        w.set_metadata(&meta).unwrap();
        w.set_id(path).unwrap();
        w.save_bytes(0, &[0u8; 16]).unwrap();
        w.close().unwrap();
    }

    #[test]
    fn overwrite_comment_shorter_in_place() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("bfrs_overwrite_short_{}.tif", std::process::id()));
        write_basic_tiff(&path, "ORIGINAL-LONG-COMMENT-STRING-HERE");
        let before = std::fs::metadata(&path).unwrap().len();

        overwrite_comment(&path, "short").unwrap();
        let got = read_first_comment(&path).unwrap();
        assert_eq!(got.as_deref(), Some("short"));

        // shorter string must NOT grow the file (overwritten in place)
        let after = std::fs::metadata(&path).unwrap().len();
        assert_eq!(before, after, "shorter comment should overwrite in place");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn overwrite_comment_longer_appends() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("bfrs_overwrite_long_{}.tif", std::process::id()));
        write_basic_tiff(&path, "short");
        let before = std::fs::metadata(&path).unwrap().len();

        let long = "A-MUCH-LONGER-REPLACEMENT-COMMENT-THAT-WILL-NOT-FIT-IN-PLACE-".repeat(4);
        overwrite_comment(&path, &long).unwrap();
        let got = read_first_comment(&path).unwrap();
        assert_eq!(got.as_deref(), Some(long.as_str()));

        // longer string must grow the file (appended at EOF, entry repointed)
        let after = std::fs::metadata(&path).unwrap().len();
        assert!(
            after > before,
            "longer comment should append at EOF (before={before}, after={after})"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn overwrite_value_general_roundtrip() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("bfrs_overwrite_general_{}.tif", std::process::id()));
        write_basic_tiff(&path, "x");
        overwrite_ifd_value(&path, 0, 270, TiffSaverValue::Ascii("hello world".into())).unwrap();
        assert_eq!(
            read_first_comment(&path).unwrap().as_deref(),
            Some("hello world")
        );
        let _ = std::fs::remove_file(&path);
    }
}
