//! Scanco AIM micro-CT format reader.
//!
//! Supports ISQ (.isq) and AIM (.aim) files from Scanco Medical micro-CT scanners.
//! ISQ files have magic "CTDATA-HEADER_V1" and a 512-byte header.
//! AIM files use extension-only detection.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;

pub struct AimReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    data_offset: u64,
}

impl AimReader {
    pub fn new() -> Self {
        AimReader {
            path: None,
            meta: None,
            data_offset: 512,
        }
    }
}

impl Default for AimReader {
    fn default() -> Self {
        Self::new()
    }
}

/// Read a NUL-terminated string starting at the current file position,
/// returning the string and the position immediately after the NUL.
fn read_cstring(f: &mut std::fs::File) -> Result<(String, u64)> {
    let mut bytes = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = f.read(&mut byte).map_err(BioFormatsError::Io)?;
        if n == 0 {
            break; // EOF before NUL
        }
        if byte[0] == 0 {
            break;
        }
        bytes.push(byte[0]);
    }
    let pos = f.stream_position().map_err(BioFormatsError::Io)?;
    Ok((String::from_utf8_lossy(&bytes).into_owned(), pos))
}

fn read_i32_le(f: &mut std::fs::File) -> Result<i32> {
    let mut b = [0u8; 4];
    f.read_exact(&mut b).map_err(BioFormatsError::Io)?;
    Ok(i32::from_le_bytes(b))
}

fn read_i64_le(f: &mut std::fs::File) -> Result<i64> {
    let mut b = [0u8; 8];
    f.read_exact(&mut b).map_err(BioFormatsError::Io)?;
    Ok(i64::from_le_bytes(b))
}

fn load_aim_header(path: &Path) -> Result<(ImageMetadata, u64)> {
    let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
    let file_len = f.metadata().map_err(BioFormatsError::Io)?.len();

    // Peek the first 16 bytes to determine the format flavour.
    let mut version = [0u8; 16];
    let n = f.read(&mut version).map_err(BioFormatsError::Io)?;
    let version_str = String::from_utf8_lossy(&version[..n]).into_owned();

    // Scanco ISQ files (a distinct format) carry the CTDATA magic. Keep that
    // path; everything else is treated as a genuine AIM file.
    let is_isq = n >= 16 && &version[..16] == b"CTDATA-HEADER_V1";

    if is_isq {
        // ISQ: 512-byte header, dimensions as i32 LE at offsets 28/32/36.
        f.seek(SeekFrom::Start(28)).map_err(BioFormatsError::Io)?;
        let w = positive_dim(read_i32_le(&mut f)?, "ISQ width")?;
        let h = positive_dim(read_i32_le(&mut f)?, "ISQ height")?;
        let d = positive_dim(read_i32_le(&mut f)?, "ISQ depth")?;
        let meta = aim_metadata(w, h, d);
        validate_payload_len(file_len, 512, &meta)?;
        return Ok((meta, 512));
    }
    if !version_str.starts_with("AIMDATA") {
        return Err(BioFormatsError::UnsupportedFormat(
            "AIM file is missing AIMDATA header".into(),
        ));
    }

    // AIM path (port of AIMReader.java). littleEndian = true.
    // "AIMDATA_V030..." uses wider (64-bit) dimension fields.
    let wider_offsets = version_str.starts_with("AIMDATA_V030");

    let (w, h, d) = if wider_offsets {
        f.seek(SeekFrom::Start(96)).map_err(BioFormatsError::Io)?;
        let w = positive_dim_i64(read_i64_le(&mut f)?, "AIM width")?;
        let h = positive_dim_i64(read_i64_le(&mut f)?, "AIM height")?;
        let d = positive_dim_i64(read_i64_le(&mut f)?, "AIM depth")?;
        f.seek(SeekFrom::Start(280)).map_err(BioFormatsError::Io)?;
        (w, h, d)
    } else {
        f.seek(SeekFrom::Start(56)).map_err(BioFormatsError::Io)?;
        let w = positive_dim(read_i32_le(&mut f)?, "AIM width")?;
        let h = positive_dim(read_i32_le(&mut f)?, "AIM height")?;
        let d = positive_dim(read_i32_le(&mut f)?, "AIM depth")?;
        f.seek(SeekFrom::Start(160)).map_err(BioFormatsError::Io)?;
        (w, h, d)
    };

    // A variable-length NUL-terminated processing-log string precedes the
    // pixel data; the pixel offset is the position just after it.
    let (processing_log, pixel_offset) = read_cstring(&mut f)?;

    let mut meta = aim_metadata(w, h, d);
    validate_payload_len(file_len, pixel_offset, &meta)?;
    // Store the processing log lines as global metadata (key  value pairs).
    for line in processing_log.split('\n') {
        let line = line.trim();
        if let Some(split) = line.find("  ") {
            let key = line[..split].trim();
            let value = line[split..].trim();
            if !key.is_empty() {
                meta.series_metadata.insert(
                    key.to_string(),
                    crate::common::metadata::MetadataValue::String(value.to_string()),
                );
            }
        }
    }

    Ok((meta, pixel_offset))
}

fn positive_dim(value: i32, label: &str) -> Result<u32> {
    if value <= 0 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "AIM header has non-positive {label}"
        )));
    }
    Ok(value as u32)
}

fn positive_dim_i64(value: i64, label: &str) -> Result<u32> {
    if value <= 0 || value > u32::MAX as i64 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "AIM header has invalid {label}"
        )));
    }
    Ok(value as u32)
}

fn validate_payload_len(file_len: u64, data_offset: u64, meta: &ImageMetadata) -> Result<()> {
    let plane_bytes = (meta.size_x as u64)
        .checked_mul(meta.size_y as u64)
        .and_then(|px| px.checked_mul(meta.pixel_type.bytes_per_sample() as u64))
        .ok_or_else(|| BioFormatsError::Format("AIM plane size overflows".into()))?;
    let required_len = data_offset
        .checked_add(
            plane_bytes
                .checked_mul(meta.image_count as u64)
                .ok_or_else(|| BioFormatsError::Format("AIM payload size overflows".into()))?,
        )
        .ok_or_else(|| BioFormatsError::Format("AIM payload size overflows".into()))?;
    if file_len < required_len {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "AIM pixel payload is shorter than declared ({file_len} < {required_len})"
        )));
    }
    Ok(())
}

fn aim_metadata(width: u32, height: u32, depth: u32) -> ImageMetadata {
    let image_count = depth.max(1);
    ImageMetadata {
        size_x: width,
        size_y: height,
        size_z: image_count,
        size_c: 1,
        size_t: 1,
        pixel_type: PixelType::Int16,
        bits_per_pixel: 16,
        image_count,
        dimension_order: DimensionOrder::XYZCT,
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
    }
}

impl FormatReader for AimReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("aim") | Some("isq"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // ISQ magic, or AIM version marker.
        (header.len() >= 16 && &header[..16] == b"CTDATA-HEADER_V1")
            || (header.len() >= 7 && &header[..7] == b"AIMDATA")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let (meta, data_offset) = load_aim_header(path)?;
        self.path = Some(path.to_path_buf());
        self.meta = Some(meta);
        self.data_offset = data_offset;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.data_offset = 512;
        Ok(())
    }

    fn series_count(&self) -> usize {
        1
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
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
        let bps = meta.pixel_type.bytes_per_sample();
        let plane_bytes = meta.size_x as usize * meta.size_y as usize * bps;
        let file_offset = self.data_offset + plane_index as u64 * plane_bytes as u64;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(file_offset))
            .map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; plane_bytes];
        f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
        Ok(buf)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let full = self.open_bytes(plane_index)?;
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        crop_full_plane("AIM", &full, meta, 1, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}
