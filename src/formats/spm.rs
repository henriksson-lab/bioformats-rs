//! Scanning Probe Microscopy (SPM) and related format readers.
//!
//! Includes binary readers for PicoQuant TCSPC and several SPM/AFM platform
//! layouts. Formats without a decoded native layout require explicit strict raw
//! fixtures instead of heuristic dimensions.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::{crop_full_plane, validate_region};

// ===========================================================================
// Binary reader — PicoQuant TCSPC / FLIM
// ===========================================================================

/// PicoQuant PTU/PQRES time-correlated single-photon counting format.
///
/// Magic: first 6 bytes == `PQTTTR`. Image dimensions parsed from text header.
pub struct PicoQuantReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

#[derive(Debug, Clone)]
struct PicoQuantTag {
    ident: String,
    index: i32,
    tag_type: u32,
    value: i64,
}

const PTU_HEADER_LEN: usize = 16;
const PTU_TAG_LEN: usize = 48;
const PTU_TAG_INT8: u32 = 0x1000_0008;
const PTU_TAG_BOOL8: u32 = 0x0000_0008;
const PTU_TAG_FLOAT8: u32 = 0x2000_0008;
const PTU_TAG_EMPTY8: u32 = 0xffff_0008;
const PTU_TAG_ANSI_STRING: u32 = 0x4001_ffff;
const PTU_TAG_WIDE_STRING: u32 = 0x4002_ffff;
const PTU_TAG_BINARY_BLOB: u32 = 0xffff_ffff;

fn picoquant_event_stream_unsupported() -> BioFormatsError {
    BioFormatsError::UnsupportedFormat(
        "PicoQuant TCSPC event-stream image reconstruction is unsupported; native PTU/PQRES event streams require explicit image dimensions for metadata and are not decoded to image planes without a strict image-plane payload".into(),
    )
}

impl PicoQuantReader {
    pub fn new() -> Self {
        PicoQuantReader {
            path: None,
            meta: None,
        }
    }

    fn parse_unified_tags(data: &[u8]) -> Result<(Vec<PicoQuantTag>, usize)> {
        if data.len() < PTU_HEADER_LEN || &data[0..6] != b"PQTTTR" {
            return Err(BioFormatsError::UnsupportedFormat(
                "PicoQuant PTU missing PQTTTR magic".into(),
            ));
        }

        let mut tags = Vec::new();
        let mut offset = PTU_HEADER_LEN;
        loop {
            let record_end = offset.checked_add(PTU_TAG_LEN).ok_or_else(|| {
                BioFormatsError::UnsupportedFormat("PicoQuant PTU tag offset overflows".into())
            })?;
            if record_end > data.len() {
                return Err(BioFormatsError::UnsupportedFormat(
                    "PicoQuant PTU tag table is truncated".into(),
                ));
            }

            let ident_bytes = &data[offset..offset + 32];
            let ident_len = ident_bytes
                .iter()
                .position(|b| *b == 0)
                .unwrap_or(ident_bytes.len());
            let ident = String::from_utf8_lossy(&ident_bytes[..ident_len]).into_owned();
            let index = i32::from_le_bytes(data[offset + 32..offset + 36].try_into().unwrap());
            let tag_type = u32::from_le_bytes(data[offset + 36..offset + 40].try_into().unwrap());
            let value = i64::from_le_bytes(data[offset + 40..offset + 48].try_into().unwrap());
            offset = record_end;

            if matches!(
                tag_type,
                PTU_TAG_ANSI_STRING | PTU_TAG_WIDE_STRING | PTU_TAG_BINARY_BLOB
            ) {
                let len = usize::try_from(value).map_err(|_| {
                    BioFormatsError::UnsupportedFormat(format!(
                        "PicoQuant PTU tag {ident} has negative payload length"
                    ))
                })?;
                let payload_end = offset.checked_add(len).ok_or_else(|| {
                    BioFormatsError::UnsupportedFormat(format!(
                        "PicoQuant PTU tag {ident} payload offset overflows"
                    ))
                })?;
                if payload_end > data.len() {
                    return Err(BioFormatsError::UnsupportedFormat(format!(
                        "PicoQuant PTU tag {ident} payload is truncated"
                    )));
                }
                offset = payload_end;
            }

            let is_end = ident == "Header_End";
            tags.push(PicoQuantTag {
                ident,
                index,
                tag_type,
                value,
            });
            if is_end {
                return Ok((tags, offset));
            }
        }
    }

    fn int_tag(tags: &[PicoQuantTag], names: &[&str]) -> Option<i64> {
        tags.iter()
            .find(|tag| {
                tag.tag_type == PTU_TAG_INT8 && tag.index < 0 && names.contains(&tag.ident.as_str())
            })
            .map(|tag| tag.value)
    }
}

impl Default for PicoQuantReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for PicoQuantReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("ptu") | Some("pqres"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.len() >= 6 && &header[0..6] == b"PQTTTR"
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.path = None;
        self.meta = None;
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        let (tags, data_offset) = Self::parse_unified_tags(&data)?;
        let width = Self::int_tag(&tags, &["ImgHdr_PixX", "ImgHdr_Pixels"]).ok_or_else(|| {
            BioFormatsError::UnsupportedFormat("PicoQuant PTU missing explicit image width".into())
        })?;
        let height = Self::int_tag(&tags, &["ImgHdr_PixY", "ImgHdr_Lines"]).ok_or_else(|| {
            BioFormatsError::UnsupportedFormat("PicoQuant PTU missing explicit image height".into())
        })?;
        let frames = Self::int_tag(&tags, &["ImgHdr_Frames", "ImgHdr_Frame"]).unwrap_or(1);
        if width <= 0 || height <= 0 || frames <= 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "PicoQuant PTU image dimensions must be positive".into(),
            ));
        }
        let width = u32::try_from(width).map_err(|_| {
            BioFormatsError::UnsupportedFormat("PicoQuant PTU image width is too large".into())
        })?;
        let height = u32::try_from(height).map_err(|_| {
            BioFormatsError::UnsupportedFormat("PicoQuant PTU image height is too large".into())
        })?;
        let frames = u32::try_from(frames).map_err(|_| {
            BioFormatsError::UnsupportedFormat("PicoQuant PTU frame count is too large".into())
        })?;

        let mut series_metadata = HashMap::new();
        series_metadata.insert(
            "ptu.data_offset".into(),
            MetadataValue::Int(data_offset as i64),
        );
        for tag in &tags {
            if matches!(
                tag.tag_type,
                PTU_TAG_INT8 | PTU_TAG_BOOL8 | PTU_TAG_FLOAT8 | PTU_TAG_EMPTY8
            ) {
                let key = if tag.index >= 0 {
                    format!("ptu.{}[{}]", tag.ident, tag.index)
                } else {
                    format!("ptu.{}", tag.ident)
                };
                let value = match tag.tag_type {
                    PTU_TAG_BOOL8 => MetadataValue::Bool(tag.value != 0),
                    PTU_TAG_FLOAT8 => MetadataValue::Float(f64::from_bits(tag.value as u64)),
                    _ => MetadataValue::Int(tag.value),
                };
                series_metadata.insert(key, value);
            }
        }

        self.path = Some(path.to_path_buf());
        self.meta = Some(ImageMetadata {
            size_x: width,
            size_y: height,
            size_z: 1,
            size_c: 1,
            size_t: frames,
            pixel_type: PixelType::Uint32,
            bits_per_pixel: 32,
            image_count: frames,
            dimension_order: DimensionOrder::XYCZT,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: true,
            resolution_count: 1,
            series_metadata,
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        });
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        Ok(())
    }

    fn series_count(&self) -> usize {
        usize::from(self.meta.is_some())
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.meta.is_none() {
            Err(BioFormatsError::NotInitialized)
        } else if s == 0 {
            Ok(())
        } else {
            Err(BioFormatsError::SeriesOutOfRange(s))
        }
    }

    fn series(&self) -> usize {
        0
    }

    fn metadata(&self) -> &ImageMetadata {
        self.meta
            .as_ref()
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        Err(picoquant_event_stream_unsupported())
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        validate_region("PicoQuant", meta.size_x, meta.size_y, x, y, w, h)?;
        Err(picoquant_event_stream_unsupported())
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn resolution_count(&self) -> usize {
        1
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if level != 0 {
            Err(BioFormatsError::Format(format!(
                "resolution {} out of range",
                level
            )))
        } else {
            Ok(())
        }
    }
}

// ===========================================================================
// Helpers for strict raw SPM subsets.
// ===========================================================================

fn unsupported_raw_spm(format_name: &str) -> BioFormatsError {
    BioFormatsError::UnsupportedFormat(format!(
        "{format_name} native binary layout is unsupported unless explicit strict raw data is present; refusing heuristic dimensions"
    ))
}

#[derive(Debug, Clone, Copy)]
struct SpmStrictRawLayout {
    data_offset: u64,
    plane_bytes: u64,
}

fn read_le_u32_spm(data: &[u8], offset: usize, label: &str, format_name: &str) -> Result<u32> {
    let bytes = data.get(offset..offset + 4).ok_or_else(|| {
        BioFormatsError::UnsupportedFormat(format!("{format_name} strict header missing {label}"))
    })?;
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn read_le_u16_spm(data: &[u8], offset: usize, label: &str, format_name: &str) -> Result<u16> {
    let bytes = data.get(offset..offset + 2).ok_or_else(|| {
        BioFormatsError::UnsupportedFormat(format!("{format_name} strict header missing {label}"))
    })?;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn read_le_u64_spm(data: &[u8], offset: usize, label: &str, format_name: &str) -> Result<u64> {
    let bytes = data.get(offset..offset + 8).ok_or_else(|| {
        BioFormatsError::UnsupportedFormat(format!("{format_name} strict header missing {label}"))
    })?;
    Ok(u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ]))
}

fn parse_strict_spm_raw(
    path: &Path,
    magic: &[u8],
    format_name: &str,
) -> Result<(ImageMetadata, SpmStrictRawLayout)> {
    let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
    if !data.starts_with(magic) {
        return Err(unsupported_raw_spm(format_name));
    }

    let width_offset = magic.len();
    let height_offset = width_offset + 4;
    let planes_offset = height_offset + 4;
    let pixel_type_offset = planes_offset + 4;
    let reserved_offset = pixel_type_offset + 2;
    let data_offset_offset = reserved_offset + 2;
    let fixed_header_len = data_offset_offset + 8;

    let width = read_le_u32_spm(&data, width_offset, "width", format_name)?;
    let height = read_le_u32_spm(&data, height_offset, "height", format_name)?;
    let planes = read_le_u32_spm(&data, planes_offset, "plane count", format_name)?;
    if width == 0 || height == 0 || planes == 0 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "{format_name} strict header dimensions must be non-zero"
        )));
    }

    let pixel_type_code = read_le_u16_spm(&data, pixel_type_offset, "pixel type", format_name)?;
    let (pixel_type, bits_per_pixel) = match pixel_type_code {
        1 => (PixelType::Uint8, 8),
        2 => (PixelType::Uint16, 16),
        _ => {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "{format_name} strict header has unsupported pixel type code {pixel_type_code}"
            )));
        }
    };
    let reserved = read_le_u16_spm(&data, reserved_offset, "reserved field", format_name)?;
    if reserved != 0 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "{format_name} strict header reserved field must be zero"
        )));
    }

    let data_offset = read_le_u64_spm(&data, data_offset_offset, "data offset", format_name)?;
    if data_offset < fixed_header_len as u64 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "{format_name} strict data offset points into header"
        )));
    }

    let plane_bytes = (width as u64)
        .checked_mul(height as u64)
        .and_then(|n| n.checked_mul(pixel_type.bytes_per_sample() as u64))
        .ok_or_else(|| BioFormatsError::Format(format!("{format_name} plane size overflows")))?;
    let payload_len = plane_bytes
        .checked_mul(planes as u64)
        .ok_or_else(|| BioFormatsError::Format(format!("{format_name} payload size overflows")))?;
    let expected_len = data_offset
        .checked_add(payload_len)
        .ok_or_else(|| BioFormatsError::Format(format!("{format_name} file size overflows")))?;
    if data.len() as u64 != expected_len {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "{format_name} strict payload length mismatch: got {}, expected {expected_len}",
            data.len()
        )));
    }

    Ok((
        ImageMetadata {
            size_x: width,
            size_y: height,
            size_z: 1,
            size_c: 1,
            size_t: planes,
            pixel_type,
            bits_per_pixel,
            image_count: planes,
            dimension_order: DimensionOrder::XYCZT,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: true,
            resolution_count: 1,
            series_metadata: HashMap::new(),
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        },
        SpmStrictRawLayout {
            data_offset,
            plane_bytes,
        },
    ))
}

fn read_strict_spm_raw_plane(
    path: &Path,
    layout: SpmStrictRawLayout,
    plane_index: u32,
) -> Result<Vec<u8>> {
    let offset = layout
        .data_offset
        .checked_add(
            layout
                .plane_bytes
                .checked_mul(plane_index as u64)
                .ok_or_else(|| {
                    BioFormatsError::Format("SPM strict plane offset overflows".into())
                })?,
        )
        .ok_or_else(|| BioFormatsError::Format("SPM strict plane offset overflows".into()))?;
    let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
    f.seek(SeekFrom::Start(offset))
        .map_err(BioFormatsError::Io)?;
    let mut buf = vec![0u8; layout.plane_bytes as usize];
    f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
    Ok(buf)
}

// ===========================================================================
// Real binary reader — RHK Technology SPM
// ===========================================================================

/// RHK Technology SPM reader (`.sm2`, `.sm3`, `.sm4`).
///
/// Port of Bio-Formats `RHKReader.java`. The file begins with a 512-byte
/// page header. There are two layouts:
///
///   * **XPM** (binary): the first little-endian `short` equals `0xaa`.
///     Integer fields live at fixed offsets (image/page/data/line type at 40,
///     `sizeX`/`sizeY` after them, then the pixel offset; float X/Y scales
///     follow).
///   * **text**: a space-separated ASCII record at offset 32 carries the same
///     type codes and dimensions; pixels start at the fixed 512-byte boundary
///     and the X/Y scales come from two further 32-byte axis records.
///
/// `dataType` selects the pixel type (0=float32, 1=int16, 2=int32, 3=uint8).
/// In the text layout the X/Y scale signs drive `invertX`/`invertY`, which
/// mirror the stored plane horizontally/vertically when reading pixels.
pub struct RhkReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    pixel_offset: u64,
    invert_x: bool,
    invert_y: bool,
}

impl RhkReader {
    const HEADER_SIZE: u64 = 512;

    pub fn new() -> Self {
        RhkReader {
            path: None,
            meta: None,
            pixel_offset: 0,
            invert_x: false,
            invert_y: false,
        }
    }

    fn read_i32_le(data: &[u8], offset: usize, label: &str) -> Result<i32> {
        let bytes = data.get(offset..offset + 4).ok_or_else(|| {
            BioFormatsError::UnsupportedFormat(format!("RHK SPM header missing {label}"))
        })?;
        Ok(i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn read_f32_le(data: &[u8], offset: usize, label: &str) -> Result<f32> {
        let bytes = data.get(offset..offset + 4).ok_or_else(|| {
            BioFormatsError::UnsupportedFormat(format!("RHK SPM header missing {label}"))
        })?;
        Ok(f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    /// Read a fixed-width ASCII record (Java `readString(len).trim()`).
    fn read_string(data: &[u8], offset: usize, len: usize) -> String {
        let end = (offset + len).min(data.len());
        let slice = data.get(offset..end).unwrap_or(&[]);
        // Stop at the first NUL like Java's String construction over the bytes,
        // then trim surrounding whitespace.
        let nul = slice.iter().position(|&b| b == 0).unwrap_or(slice.len());
        String::from_utf8_lossy(&slice[..nul]).trim().to_string()
    }

    /// Map RHK dataType code → (PixelType, bits-per-pixel).
    fn pixel_type_from_data_type(data_type: i32) -> Result<(PixelType, u8)> {
        match data_type {
            0 => Ok((PixelType::Float32, 32)),
            1 => Ok((PixelType::Int16, 16)),
            2 => Ok((PixelType::Int32, 32)),
            3 => Ok((PixelType::Uint8, 8)),
            other => Err(BioFormatsError::UnsupportedFormat(format!(
                "RHK SPM unsupported data type: {other}"
            ))),
        }
    }
}

impl Default for RhkReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for RhkReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("sm2") | Some("sm3") | Some("sm4"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        if data.len() < Self::HEADER_SIZE as usize {
            return Err(BioFormatsError::UnsupportedFormat(
                "RHK SPM file is shorter than the 512-byte page header".into(),
            ));
        }

        // Java: little-endian; xpm = (readShort() == 0xaa).
        let first_short = i16::from_le_bytes([data[0], data[1]]);
        let xpm = first_short == 0xaa;

        let mut width: u32;
        let mut height: u32;
        let pixel_offset: u64;
        let data_type: i32;
        let mut invert_x = false;
        let mut invert_y = false;
        let x_scale: f64;
        let y_scale: f64;

        if xpm {
            // seek(40): imageType, pageType, dataType, lineType ints.
            let _image_type = Self::read_i32_le(&data, 40, "image type")?;
            let _page_type = Self::read_i32_le(&data, 44, "page type")?;
            data_type = Self::read_i32_le(&data, 48, "data type")?;
            let _line_type = Self::read_i32_le(&data, 52, "line type")?;
            // skipBytes(8) → offset 56..64.
            width = Self::read_i32_le(&data, 64, "width")? as u32;
            height = Self::read_i32_le(&data, 68, "height")? as u32;
            // skipBytes(16) → offset 72..88.
            pixel_offset = Self::read_i32_le(&data, 88, "pixel offset")? as u32 as u64;
            // After the int read, the stream is at offset 92; skipBytes(8) → 100.
            x_scale = Self::read_f32_le(&data, 100, "x scale")? as f64 * 1_000_000.0;
            y_scale = Self::read_f32_le(&data, 104, "y scale")? as f64 * 1_000_000.0;
        } else {
            // seek(32): 32-byte space-separated ASCII type/dimension record.
            let type_record = Self::read_string(&data, 32, 32);
            let type_data: Vec<&str> = type_record.split_whitespace().collect();
            let parse = |idx: usize, label: &str| -> Result<i32> {
                type_data
                    .get(idx)
                    .and_then(|v| v.parse::<i32>().ok())
                    .ok_or_else(|| {
                        BioFormatsError::UnsupportedFormat(format!(
                            "RHK SPM text header missing {label}"
                        ))
                    })
            };
            let _image_type = parse(0, "image type")?;
            data_type = parse(1, "data type")?;
            let _line_type = parse(2, "line type")?;
            width = parse(3, "width")? as u32;
            height = parse(4, "height")? as u32;
            let _page_type = parse(6, "page type")?;
            pixel_offset = Self::HEADER_SIZE;

            // Two further 32-byte axis records (X then Y); field [1] is the scale.
            let x_axis = Self::read_string(&data, 64, 32);
            let y_axis = Self::read_string(&data, 96, 32);
            let x_axis_fields: Vec<&str> = x_axis.split_whitespace().collect();
            let y_axis_fields: Vec<&str> = y_axis.split_whitespace().collect();
            let x_raw = x_axis_fields
                .get(1)
                .and_then(|v| v.parse::<f64>().ok())
                .ok_or_else(|| {
                    BioFormatsError::UnsupportedFormat("RHK SPM text header missing X scale".into())
                })?;
            let y_raw = y_axis_fields
                .get(1)
                .and_then(|v| v.parse::<f64>().ok())
                .ok_or_else(|| {
                    BioFormatsError::UnsupportedFormat("RHK SPM text header missing Y scale".into())
                })?;
            x_scale = x_raw * 1_000_000.0;
            y_scale = y_raw * 1_000_000.0;
            invert_x = x_scale < 0.0;
            invert_y = y_scale > 0.0;
        }

        if width == 0 || height == 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "RHK SPM header contains invalid image dimensions".into(),
            ));
        }
        let _ = (&mut width, &mut height);

        let (pixel_type, bits_per_pixel) = Self::pixel_type_from_data_type(data_type)?;
        let bps = pixel_type.bytes_per_sample() as u64;
        let expected = pixel_offset
            .checked_add(
                (width as u64)
                    .checked_mul(height as u64)
                    .and_then(|p| p.checked_mul(bps))
                    .ok_or_else(|| {
                        BioFormatsError::Format("RHK SPM plane size overflows".into())
                    })?,
            )
            .ok_or_else(|| BioFormatsError::Format("RHK SPM plane size overflows".into()))?;
        if expected > data.len() as u64 {
            return Err(BioFormatsError::UnsupportedFormat(
                "RHK SPM pixel payload is shorter than declared dimensions".into(),
            ));
        }

        // seek(352): 32-byte description string.
        let description = Self::read_string(&data, 352, 32);
        let mut series_metadata = HashMap::new();
        if !description.is_empty() {
            series_metadata.insert(
                "Description".into(),
                crate::common::metadata::MetadataValue::String(description),
            );
        }
        series_metadata.insert(
            "X scale (um)".into(),
            crate::common::metadata::MetadataValue::Float(x_scale),
        );
        series_metadata.insert(
            "Y scale (um)".into(),
            crate::common::metadata::MetadataValue::Float(y_scale),
        );

        self.pixel_offset = pixel_offset;
        self.invert_x = invert_x;
        self.invert_y = invert_y;
        self.path = Some(path.to_path_buf());
        self.meta = Some(ImageMetadata {
            size_x: width,
            size_y: height,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type,
            bits_per_pixel,
            image_count: 1,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: true,
            resolution_count: 1,
            series_metadata,
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        });
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.pixel_offset = 0;
        self.invert_x = false;
        self.invert_y = false;
        Ok(())
    }

    fn series_count(&self) -> usize {
        usize::from(self.meta.is_some())
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.meta.is_none() {
            return Err(BioFormatsError::NotInitialized);
        }
        if s != 0 {
            Err(BioFormatsError::SeriesOutOfRange(s))
        } else {
            Ok(())
        }
    }

    fn series(&self) -> usize {
        0
    }

    fn metadata(&self) -> &ImageMetadata {
        self.meta
            .as_ref()
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let (sx, sy) = (meta.size_x, meta.size_y);
        self.open_bytes_region(plane_index, 0, 0, sx, sy)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let meta = self
            .meta
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?
            .clone();
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let bps = meta.pixel_type.bytes_per_sample();
        let sx = meta.size_x as usize;
        let sy = meta.size_y as usize;
        let n_bytes = sx
            .checked_mul(sy)
            .and_then(|p| p.checked_mul(bps))
            .ok_or_else(|| BioFormatsError::Format("RHK SPM plane size overflows".into()))?;

        let path = self.path.clone().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = std::fs::File::open(&path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(self.pixel_offset))
            .map_err(BioFormatsError::Io)?;
        let mut plane = vec![0u8; n_bytes];
        f.read_exact(&mut plane).map_err(BioFormatsError::Io)?;

        // RHKReader.java reads pixels from the mirrored corner and then flips
        // the returned tile. Mirroring the whole stored plane (per axis) before
        // cropping at (x,y,w,h) is equivalent and reuses the crop helper.
        let row_len = sx * bps;
        if self.invert_y {
            for row in 0..sy / 2 {
                let top = row * row_len;
                let bottom = (sy - row - 1) * row_len;
                for i in 0..row_len {
                    plane.swap(top + i, bottom + i);
                }
            }
        }
        if self.invert_x {
            for row in 0..sy {
                let base = row * row_len;
                for col in 0..sx / 2 {
                    let left = base + col * bps;
                    let right = base + (sx - col - 1) * bps;
                    for i in 0..bps {
                        plane.swap(left + i, right + i);
                    }
                }
            }
        }

        crop_full_plane("RHK SPM", &plane, &meta, 1, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn resolution_count(&self) -> usize {
        1
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if level != 0 {
            Err(BioFormatsError::Format(format!(
                "resolution {} out of range",
                level
            )))
        } else {
            Ok(())
        }
    }
}

// ===========================================================================
// Raw binary reader — Quesant AFM
// ===========================================================================

/// Quesant AFM reader (`.afm`).
///
/// Strict raw subset only; native Quesant AFM layout is not decoded.
pub struct QuesantReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    layout: Option<SpmStrictRawLayout>,
}

impl QuesantReader {
    const STRICT_RAW_MAGIC: &'static [u8] = b"BFQUESANTAFMRAW!";

    pub fn new() -> Self {
        QuesantReader {
            path: None,
            meta: None,
            layout: None,
        }
    }
}

impl Default for QuesantReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for QuesantReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        // Note: .afm is also used by VeecoReader (Nanoscope). Quesant AFM
        // files lack the NANOSCOPE header, so this reader is a fallback.
        matches!(ext.as_deref(), Some("afm"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        _header.starts_with(Self::STRICT_RAW_MAGIC)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.layout = None;
        let (meta, layout) = parse_strict_spm_raw(path, Self::STRICT_RAW_MAGIC, "Quesant AFM")?;
        self.path = Some(path.to_path_buf());
        self.meta = Some(meta);
        self.layout = Some(layout);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.layout = None;
        Ok(())
    }

    fn series_count(&self) -> usize {
        usize::from(self.meta.is_some())
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.meta.is_none() {
            Err(BioFormatsError::NotInitialized)
        } else if s == 0 {
            Ok(())
        } else {
            Err(BioFormatsError::SeriesOutOfRange(s))
        }
    }

    fn series(&self) -> usize {
        0
    }

    fn metadata(&self) -> &ImageMetadata {
        self.meta
            .as_ref()
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        read_strict_spm_raw_plane(
            self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?,
            self.layout.ok_or(BioFormatsError::NotInitialized)?,
            plane_index,
        )
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let meta = self
            .meta
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?
            .clone();
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let full = self.open_bytes(plane_index)?;
        crop_full_plane("Quesant AFM", &full, &meta, 1, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn resolution_count(&self) -> usize {
        1
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if level != 0 {
            Err(BioFormatsError::Format(format!(
                "resolution {} out of range",
                level
            )))
        } else {
            Ok(())
        }
    }
}

// ===========================================================================
// TIFF reader — JPK Instruments AFM
// ===========================================================================

/// JPK Instruments AFM reader (`.jpk`).
///
/// Port of JPKReader.java: a `.jpk` file IS a TIFF (JPKReader extends
/// BaseTiffReader). Exposes two series: series 0 = IFD 0 (a single-plane
/// thumbnail), series 1 = IFDs 1..n grouped as a T-stack.
pub struct JpkReader {
    extracted_path: Option<PathBuf>,
    inner: crate::tiff::TiffReader,
    meta: Option<ImageMetadata>,
    is_tiff: bool,
}

impl JpkReader {
    pub fn new() -> Self {
        JpkReader {
            extracted_path: None,
            inner: crate::tiff::TiffReader::new(),
            meta: None,
            is_tiff: false,
        }
    }
}

impl Default for JpkReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for JpkReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("jpk"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let _ = self.inner.close();
        // A .jpk file is itself a TIFF; parse it directly.
        self.inner.set_id(path)?;

        let ifd_count = self.inner.ifd_count();
        if ifd_count == 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "JPK: TIFF contains no IFDs".to_string(),
            ));
        }

        // Build a per-IFD metadata lookup from the default series grouping so we
        // can reconstruct accurate dimensions/pixel-type for the JPK layout.
        // We clone existing TiffSeries values (the type is not re-exported) and
        // mutate their public fields rather than constructing literals.
        let default_series = self.inner.series_list();
        let mut meta_for_ifd: Vec<Option<ImageMetadata>> = vec![None; ifd_count];
        for series in default_series {
            for &idx in &series.ifd_indices {
                if idx < ifd_count {
                    meta_for_ifd[idx] = Some(series.metadata.clone());
                }
            }
        }
        // A template TiffSeries to clone (carries the unexported type).
        let template = default_series[0].clone();
        let ifd_meta = |idx: usize| -> ImageMetadata {
            meta_for_ifd
                .get(idx)
                .and_then(|m| m.clone())
                .unwrap_or_else(|| template.metadata.clone())
        };

        let mut new_series = Vec::new();

        // Series 0: IFD 0 only, a single-plane thumbnail.
        {
            let mut s = template.clone();
            let mut m = ifd_meta(0);
            m.size_z = 1;
            m.size_t = 1;
            m.image_count = 1;
            m.dimension_order = crate::common::metadata::DimensionOrder::XYCZT;
            s.ifd_indices = vec![0];
            s.plane_ifd_indices = Vec::new();
            s.sub_resolutions = Vec::new();
            s.metadata = m;
            new_series.push(s);
        }

        // Series 1 (only if there is more than one IFD): IFDs 1..n as a T-stack.
        if ifd_count > 1 {
            let t = (ifd_count - 1) as u32;
            let mut s = template.clone();
            let mut m = ifd_meta(1);
            m.size_z = 1;
            m.size_t = t;
            m.size_c = if m.is_rgb { m.size_c } else { 1 };
            m.image_count = t;
            m.dimension_order = crate::common::metadata::DimensionOrder::XYCZT;
            s.ifd_indices = (1..ifd_count).collect();
            s.plane_ifd_indices = Vec::new();
            s.sub_resolutions = Vec::new();
            s.metadata = m;
            new_series.push(s);
        }

        self.inner.replace_series(new_series);
        self.inner.set_series(0)?;
        self.meta = Some(self.inner.metadata().clone());
        self.is_tiff = true;

        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        if self.is_tiff {
            let _ = self.inner.close();
        }
        if let Some(p) = self.extracted_path.take() {
            let _ = std::fs::remove_file(p);
        }
        self.meta = None;
        self.is_tiff = false;
        Ok(())
    }

    fn series_count(&self) -> usize {
        if self.is_tiff {
            self.inner.series_count()
        } else {
            0
        }
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.is_tiff {
            self.inner.set_series(s)
        } else if self.meta.is_none() {
            Err(BioFormatsError::NotInitialized)
        } else if s != 0 {
            Err(BioFormatsError::SeriesOutOfRange(s))
        } else {
            Ok(())
        }
    }

    fn series(&self) -> usize {
        if self.is_tiff {
            self.inner.series()
        } else {
            0
        }
    }

    fn metadata(&self) -> &ImageMetadata {
        if self.is_tiff {
            self.inner.metadata()
        } else {
            self.meta
                .as_ref()
                .unwrap_or(crate::common::reader::uninitialized_metadata())
        }
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        if self.is_tiff {
            return self.inner.open_bytes(plane_index);
        }
        let _ = plane_index;
        Err(BioFormatsError::UnsupportedFormat(
            "JPK ZIP archive does not contain delegated TIFF data".to_string(),
        ))
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        if self.is_tiff {
            return self.inner.open_bytes_region(plane_index, x, y, w, h);
        }
        let _ = (plane_index, x, y, w, h);
        Err(BioFormatsError::UnsupportedFormat(
            "JPK ZIP archive does not contain delegated TIFF data".to_string(),
        ))
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        if self.is_tiff {
            return self.inner.open_thumb_bytes(plane_index);
        }
        let _ = plane_index;
        Err(BioFormatsError::UnsupportedFormat(
            "JPK ZIP archive does not contain delegated TIFF data".to_string(),
        ))
    }

    fn resolution_count(&self) -> usize {
        if self.is_tiff {
            self.inner.resolution_count()
        } else {
            1
        }
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if self.is_tiff {
            self.inner.set_resolution(level)
        } else if level != 0 {
            Err(BioFormatsError::Format(format!(
                "resolution {} out of range",
                level
            )))
        } else {
            Ok(())
        }
    }
}

// ===========================================================================
// Raw binary reader — WaTom SPM
// ===========================================================================

/// WA Technology TOP reader (`.wat`, plus legacy aliases).
///
/// Java Bio-Formats uses a 4864-byte little-endian header followed by raw
/// signed 16-bit pixels.
pub struct WatopReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl WatopReader {
    const HEADER_SIZE: usize = 4864;
    const MAGIC: &'static [u8] = b"0TOPSystem W.A.Technology";

    pub fn new() -> Self {
        WatopReader {
            path: None,
            meta: None,
        }
    }

    fn read_i32_le(data: &[u8], offset: usize, label: &str) -> Result<i32> {
        let bytes = data.get(offset..offset + 4).ok_or_else(|| {
            BioFormatsError::UnsupportedFormat(format!("WA Technology TOP header missing {label}"))
        })?;
        Ok(i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }
}

impl Default for WatopReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for WatopReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(
            ext.as_deref(),
            Some("wat") | Some("wap") | Some("opo") | Some("opz") | Some("opt")
        )
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.starts_with(Self::MAGIC)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        if data.len() < Self::HEADER_SIZE {
            return Err(BioFormatsError::UnsupportedFormat(
                "WA Technology TOP file is shorter than the 4864-byte header".into(),
            ));
        }
        if !data.starts_with(Self::MAGIC) {
            return Err(BioFormatsError::UnsupportedFormat(
                "WA Technology TOP file is missing 0TOPSystem W.A.Technology magic".into(),
            ));
        }

        let width = Self::read_i32_le(&data, 259, "width")?;
        let height = Self::read_i32_le(&data, 263, "height")?;
        if width <= 0 || height <= 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "WA Technology TOP header contains invalid image dimensions".into(),
            ));
        }
        let width = width as u32;
        let height = height as u32;
        let expected = (Self::HEADER_SIZE as u64)
            .checked_add(width as u64 * height as u64 * 2)
            .ok_or_else(|| BioFormatsError::Format("WA Technology TOP size overflows".into()))?;
        let file_len = data.len() as u64;
        if file_len < expected {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "WA Technology TOP pixel payload is shorter than declared dimensions {width}x{height}"
            )));
        }

        let comment_bytes = data.get(49..82).unwrap_or(&[]);
        let comment = String::from_utf8_lossy(comment_bytes)
            .trim_end_matches('\0')
            .trim()
            .to_string();
        let mut series_metadata = HashMap::new();
        if !comment.is_empty() {
            series_metadata.insert(
                "Comment".to_string(),
                crate::common::metadata::MetadataValue::String(comment),
            );
        }
        if let Ok(x_size) = Self::read_i32_le(&data, 247, "x size") {
            series_metadata.insert(
                "X size (in um)".to_string(),
                crate::common::metadata::MetadataValue::Float(x_size as f64 / 100.0),
            );
        }
        if let Ok(y_size) = Self::read_i32_le(&data, 251, "y size") {
            series_metadata.insert(
                "Y size (in um)".to_string(),
                crate::common::metadata::MetadataValue::Float(y_size as f64 / 100.0),
            );
        }
        if let Ok(z_size) = Self::read_i32_le(&data, 255, "z size") {
            series_metadata.insert(
                "Z size (in um)".to_string(),
                crate::common::metadata::MetadataValue::Float(z_size as f64 / 100.0),
            );
        }

        self.path = Some(path.to_path_buf());
        self.meta = Some(ImageMetadata {
            size_x: width,
            size_y: height,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type: PixelType::Int16,
            bits_per_pixel: 16,
            image_count: 1,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: true,
            resolution_count: 1,
            series_metadata,
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        });
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        Ok(())
    }

    fn series_count(&self) -> usize {
        usize::from(self.meta.is_some())
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.meta.is_none() {
            return Err(BioFormatsError::NotInitialized);
        }
        if s != 0 {
            Err(BioFormatsError::SeriesOutOfRange(s))
        } else {
            Ok(())
        }
    }

    fn series(&self) -> usize {
        0
    }

    fn metadata(&self) -> &ImageMetadata {
        self.meta
            .as_ref()
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(Self::HEADER_SIZE as u64))
            .map_err(BioFormatsError::Io)?;
        let n_bytes = meta.size_x as usize * meta.size_y as usize * 2;
        let mut buf = vec![0; n_bytes];
        f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
        Ok(buf)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        _x: u32,
        _y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let meta = self
            .meta
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?
            .clone();
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let full = self.open_bytes(plane_index)?;
        crop_full_plane("WA Technology TOP", &full, &meta, 1, _x, _y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn resolution_count(&self) -> usize {
        1
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if level != 0 {
            Err(BioFormatsError::Format(format!(
                "resolution {} out of range",
                level
            )))
        } else {
            Ok(())
        }
    }
}

// ===========================================================================
// Raw binary reader — VG SAM
// ===========================================================================

/// VG SAM reader (`.dti`, plus legacy `.vgsam` alias).
///
/// Java Bio-Formats uses `VGS` magic, big-endian dimensions at offsets
/// 348/352, bytes-per-pixel at 360, and pixels at offset 368.
pub struct VgSamReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl VgSamReader {
    const PIXEL_OFFSET: usize = 368;
    const MAGIC: &'static [u8] = b"VGS";

    pub fn new() -> Self {
        VgSamReader {
            path: None,
            meta: None,
        }
    }

    fn read_i32_be(data: &[u8], offset: usize, label: &str) -> Result<i32> {
        let bytes = data.get(offset..offset + 4).ok_or_else(|| {
            BioFormatsError::UnsupportedFormat(format!("VG SAM header missing {label}"))
        })?;
        Ok(i32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn pixel_type_from_bpp(bytes_per_pixel: i32) -> Result<(PixelType, u8)> {
        match bytes_per_pixel {
            1 => Ok((PixelType::Uint8, 8)),
            2 => Ok((PixelType::Uint16, 16)),
            4 => Ok((PixelType::Float32, 32)),
            _ => Err(BioFormatsError::UnsupportedFormat(format!(
                "VG SAM unsupported bytes per pixel: {bytes_per_pixel}"
            ))),
        }
    }
}

impl Default for VgSamReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for VgSamReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("dti") | Some("vgsam"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.starts_with(Self::MAGIC)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        if data.len() < Self::PIXEL_OFFSET {
            return Err(BioFormatsError::UnsupportedFormat(
                "VG SAM file is shorter than the 368-byte header".into(),
            ));
        }
        if !data.starts_with(Self::MAGIC) {
            return Err(BioFormatsError::UnsupportedFormat(
                "VG SAM file is missing VGS magic".into(),
            ));
        }
        let width = Self::read_i32_be(&data, 348, "width")?;
        let height = Self::read_i32_be(&data, 352, "height")?;
        let bytes_per_pixel = Self::read_i32_be(&data, 360, "bytes per pixel")?;
        if width <= 0 || height <= 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "VG SAM header contains invalid image dimensions".into(),
            ));
        }
        let (pixel_type, bits_per_pixel) = Self::pixel_type_from_bpp(bytes_per_pixel)?;
        let width = width as u32;
        let height = height as u32;
        let expected = (Self::PIXEL_OFFSET as u64)
            .checked_add(width as u64 * height as u64 * bytes_per_pixel as u64)
            .ok_or_else(|| BioFormatsError::Format("VG SAM size overflows".into()))?;
        if (data.len() as u64) < expected {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "VG SAM pixel payload is shorter than declared dimensions {width}x{height}"
            )));
        }

        let mut series_metadata = HashMap::new();
        series_metadata.insert(
            "Bytes per pixel".into(),
            crate::common::metadata::MetadataValue::Int(bytes_per_pixel as i64),
        );
        self.path = Some(path.to_path_buf());
        self.meta = Some(ImageMetadata {
            size_x: width,
            size_y: height,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type,
            bits_per_pixel,
            image_count: 1,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: false,
            resolution_count: 1,
            series_metadata,
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        });
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        Ok(())
    }

    fn series_count(&self) -> usize {
        usize::from(self.meta.is_some())
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.meta.is_none() {
            return Err(BioFormatsError::NotInitialized);
        }
        if s != 0 {
            Err(BioFormatsError::SeriesOutOfRange(s))
        } else {
            Ok(())
        }
    }

    fn series(&self) -> usize {
        0
    }

    fn metadata(&self) -> &ImageMetadata {
        self.meta
            .as_ref()
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(Self::PIXEL_OFFSET as u64))
            .map_err(BioFormatsError::Io)?;
        let mut buf =
            vec![
                0u8;
                meta.size_x as usize * meta.size_y as usize * meta.pixel_type.bytes_per_sample()
            ];
        f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
        Ok(buf)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        _x: u32,
        _y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let meta = self
            .meta
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?
            .clone();
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let full = self.open_bytes(plane_index)?;
        crop_full_plane("VG SAM", &full, &meta, 1, _x, _y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn resolution_count(&self) -> usize {
        1
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if level != 0 {
            Err(BioFormatsError::Format(format!(
                "resolution {} out of range",
                level
            )))
        } else {
            Ok(())
        }
    }
}

// ===========================================================================
// Raw binary reader — UBM Messtechnik
// ===========================================================================

/// UBM reader (`.pr3`, plus legacy `.ubm` alias).
///
/// Java Bio-Formats stores dimensions at offsets 44/48 in a 128-byte
/// little-endian header, followed by uint32 pixels with optional row padding.
pub struct UbmReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    padding_pixels: usize,
}

impl UbmReader {
    const HEADER_SIZE: usize = 128;

    pub fn new() -> Self {
        UbmReader {
            path: None,
            meta: None,
            padding_pixels: 0,
        }
    }

    fn read_i32_le(data: &[u8], offset: usize, label: &str) -> Result<i32> {
        let bytes = data.get(offset..offset + 4).ok_or_else(|| {
            BioFormatsError::UnsupportedFormat(format!("UBM header missing {label}"))
        })?;
        Ok(i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }
}

impl Default for UbmReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for UbmReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("pr3") | Some("ubm"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        if data.len() < Self::HEADER_SIZE {
            return Err(BioFormatsError::UnsupportedFormat(
                "UBM file is shorter than the 128-byte header".into(),
            ));
        }
        let width = Self::read_i32_le(&data, 44, "width")?;
        let height = Self::read_i32_le(&data, 48, "height")?;
        if width <= 0 || height <= 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "UBM header contains invalid image dimensions".into(),
            ));
        }
        let width = width as u32;
        let height = height as u32;
        let plane_bytes = width as u64 * height as u64 * 4;
        let min_len = Self::HEADER_SIZE as u64 + plane_bytes;
        if (data.len() as u64) < min_len {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "UBM pixel payload is shorter than declared dimensions {width}x{height}"
            )));
        }
        let extra = data.len() as u64 - min_len;
        let row_padding_bytes = extra
            .checked_div(height as u64)
            .ok_or_else(|| BioFormatsError::Format("UBM row padding overflows".into()))?;
        if row_padding_bytes % 4 != 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "UBM row padding is not aligned to uint32 pixels".into(),
            ));
        }
        let padding_pixels = (row_padding_bytes / 4) as usize;

        self.path = Some(path.to_path_buf());
        self.padding_pixels = padding_pixels;
        let mut series_metadata = HashMap::new();
        series_metadata.insert(
            "Padding pixels".to_string(),
            crate::common::metadata::MetadataValue::Int(padding_pixels as i64),
        );
        self.meta = Some(ImageMetadata {
            size_x: width,
            size_y: height,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type: PixelType::Uint32,
            bits_per_pixel: 32,
            image_count: 1,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: true,
            resolution_count: 1,
            series_metadata,
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        });
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.padding_pixels = 0;
        Ok(())
    }

    fn series_count(&self) -> usize {
        usize::from(self.meta.is_some())
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.meta.is_none() {
            return Err(BioFormatsError::NotInitialized);
        }
        if s != 0 {
            Err(BioFormatsError::SeriesOutOfRange(s))
        } else {
            Ok(())
        }
    }

    fn series(&self) -> usize {
        0
    }

    fn metadata(&self) -> &ImageMetadata {
        self.meta
            .as_ref()
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        self.open_bytes_region(plane_index, 0, 0, meta.size_x, meta.size_y)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        _x: u32,
        _y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        crate::common::region::validate_region("UBM", meta.size_x, meta.size_y, _x, _y, w, h)?;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
        let row_stride = (meta.size_x as usize + self.padding_pixels)
            .checked_mul(4)
            .ok_or_else(|| BioFormatsError::Format("UBM row stride overflows".into()))?;
        let out_row = (w as usize)
            .checked_mul(4)
            .ok_or_else(|| BioFormatsError::Format("UBM output row size overflows".into()))?;
        let mut out = Vec::with_capacity(out_row * h as usize);
        for row in 0..h as usize {
            let source_row = _y as usize + row;
            let offset =
                Self::HEADER_SIZE as u64 + source_row as u64 * row_stride as u64 + _x as u64 * 4;
            f.seek(SeekFrom::Start(offset))
                .map_err(BioFormatsError::Io)?;
            let start = out.len();
            out.resize(start + out_row, 0);
            f.read_exact(&mut out[start..start + out_row])
                .map_err(BioFormatsError::Io)?;
        }
        Ok(out)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn resolution_count(&self) -> usize {
        1
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if level != 0 {
            Err(BioFormatsError::Format(format!(
                "resolution {} out of range",
                level
            )))
        } else {
            Ok(())
        }
    }
}

// ===========================================================================
// Raw binary reader — Seiko SPM
// ===========================================================================

/// Seiko SPM reader (`.xqd`, `.xqf`).
///
/// Java Bio-Formats stores dimensions at offset 1402 in a 2944-byte
/// little-endian header, followed by raw uint16 pixels.
pub struct SeikoReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl SeikoReader {
    const HEADER_SIZE: usize = 2944;

    pub fn new() -> Self {
        SeikoReader {
            path: None,
            meta: None,
        }
    }

    fn read_u16_le(data: &[u8], offset: usize, label: &str) -> Result<u16> {
        let bytes = data.get(offset..offset + 2).ok_or_else(|| {
            BioFormatsError::UnsupportedFormat(format!("Seiko SPM header missing {label}"))
        })?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn read_f32_le(data: &[u8], offset: usize) -> Option<f32> {
        let bytes = data.get(offset..offset + 4)?;
        Some(f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }
}

impl Default for SeikoReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for SeikoReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("xqd") | Some("xqf"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        if data.len() < Self::HEADER_SIZE {
            return Err(BioFormatsError::UnsupportedFormat(
                "Seiko SPM file is shorter than the 2944-byte header".into(),
            ));
        }
        let width = Self::read_u16_le(&data, 1402, "width")? as u32;
        let height = Self::read_u16_le(&data, 1404, "height")? as u32;
        if width == 0 || height == 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "Seiko SPM header contains invalid image dimensions".into(),
            ));
        }
        let expected = (Self::HEADER_SIZE as u64)
            .checked_add(width as u64 * height as u64 * 2)
            .ok_or_else(|| BioFormatsError::Format("Seiko SPM size overflows".into()))?;
        if (data.len() as u64) < expected {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "Seiko SPM pixel payload is shorter than declared dimensions {width}x{height}"
            )));
        }

        let mut series_metadata = HashMap::new();
        let comment_bytes = &data[40..data.len().min(156)];
        let nul = comment_bytes
            .iter()
            .position(|b| *b == 0)
            .unwrap_or(comment_bytes.len());
        let comment = String::from_utf8_lossy(&comment_bytes[..nul])
            .trim()
            .to_string();
        if !comment.is_empty() {
            series_metadata.insert(
                "Comment".into(),
                crate::common::metadata::MetadataValue::String(comment),
            );
        }
        if let Some(x_size) = Self::read_f32_le(&data, 156) {
            series_metadata.insert(
                "X size".into(),
                crate::common::metadata::MetadataValue::Float(x_size as f64),
            );
        }
        if let Some(y_size) = Self::read_f32_le(&data, 164) {
            series_metadata.insert(
                "Y size".into(),
                crate::common::metadata::MetadataValue::Float(y_size as f64),
            );
        }

        self.path = Some(path.to_path_buf());
        self.meta = Some(ImageMetadata {
            size_x: width,
            size_y: height,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type: PixelType::Uint16,
            bits_per_pixel: 16,
            image_count: 1,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: true,
            resolution_count: 1,
            series_metadata,
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        });
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        Ok(())
    }

    fn series_count(&self) -> usize {
        usize::from(self.meta.is_some())
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.meta.is_none() {
            return Err(BioFormatsError::NotInitialized);
        }
        if s != 0 {
            Err(BioFormatsError::SeriesOutOfRange(s))
        } else {
            Ok(())
        }
    }

    fn series(&self) -> usize {
        0
    }

    fn metadata(&self) -> &ImageMetadata {
        self.meta
            .as_ref()
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(Self::HEADER_SIZE as u64))
            .map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; meta.size_x as usize * meta.size_y as usize * 2];
        f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
        Ok(buf)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        _x: u32,
        _y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let meta = self
            .meta
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?
            .clone();
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let full = self.open_bytes(plane_index)?;
        crop_full_plane("Seiko SPM", &full, &meta, 1, _x, _y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn resolution_count(&self) -> usize {
        1
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if level != 0 {
            Err(BioFormatsError::Format(format!(
                "resolution {} out of range",
                level
            )))
        } else {
            Ok(())
        }
    }
}
