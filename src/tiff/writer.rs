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
    /// (strip_offset, strip_byte_count) recorded per plane as they are written.
    plane_strips: Vec<(u64, u64)>,
    planes_written: u32,
    /// Optional OME-XML to embed in the first IFD's ImageDescription.
    ome_xml: Option<String>,
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

fn validate_tiff_writer_metadata(meta: &ImageMetadata) -> Result<()> {
    if meta.pixel_type == PixelType::Bit {
        return Err(BioFormatsError::Format(
            "TIFF writer does not support PixelType::Bit until 1-bit output is packed".into(),
        ));
    }
    if meta.is_rgb && meta.size_c > 1 && !meta.is_interleaved {
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
                            ifd_bytes[off + 8..off + 12]
                                .copy_from_slice(&abs_off.to_le_bytes());
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
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("tif") | Some("tiff"))
    }

    fn set_metadata(&mut self, meta: &ImageMetadata) -> Result<()> {
        validate_tiff_writer_metadata(meta)?;
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
        if data.len() != expected_len {
            return Err(BioFormatsError::Format(format!(
                "Pyramid TIFF writer: level 0 plane {plane_index} has {} bytes, expected {expected_len}",
                data.len()
            )));
        }
        self.levels[0].push(data.to_vec());
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
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("tif") | Some("tiff"))
    }

    fn set_metadata(&mut self, meta: &ImageMetadata) -> Result<()> {
        validate_tiff_writer_metadata(meta)?;
        self.meta = Some(meta.clone());
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
        if data.len() != expected_len {
            return Err(BioFormatsError::Format(format!(
                "TIFF writer: plane {} has {} bytes, expected {}",
                plane_index,
                data.len(),
                expected_len
            )));
        }

        let compressed = compress(data, self.compression)?;
        let w = self.file.as_mut().ok_or(BioFormatsError::NotInitialized)?;

        let offset = w.seek(SeekFrom::Current(0)).map_err(BioFormatsError::Io)?;
        w.write_all(&compressed).map_err(BioFormatsError::Io)?;

        self.plane_strips.push((offset, compressed.len() as u64));
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
            let (strip_offset, strip_byte_count) = self.plane_strips[plane_idx];

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
                Entry {
                    tag: 273,
                    typ: long_type().0,
                    count: 1,
                    value_or_offset: classic_tiff_u32(strip_offset, "strip offset")?,
                },
                short_entry(277, spp as u16),
                long_entry(278, meta.size_y), // RowsPerStrip = full image height
                Entry {
                    tag: 279,
                    typ: long_type().0,
                    count: 1,
                    value_or_offset: classic_tiff_u32(strip_byte_count, "strip byte count")?,
                },
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
                short_entry(284, 1), // PlanarConfiguration = chunky
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
                    // StripOffsets / StripByteCounts are already absolute (set from plane_strips)
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
