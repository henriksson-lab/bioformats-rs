//! AFM/STM format readers.
//!
//! - TopoMetrix AFM (.tfr, .ffr, .zfr): text header + binary data
//! - Unisoku STM/AFM (.hdr + .dat): text header with companion binary

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;

// ── TopoMetrix Reader ─────────────────────────────────────────────────────────

pub struct TopoMetrixReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    data_offset: u64,
}

impl TopoMetrixReader {
    pub fn new() -> Self {
        TopoMetrixReader {
            path: None,
            meta: None,
            data_offset: 0,
        }
    }
}

impl Default for TopoMetrixReader {
    fn default() -> Self {
        Self::new()
    }
}

fn parse_topometrix(path: &Path) -> Result<(ImageMetadata, u64)> {
    let content = std::fs::read(path).map_err(BioFormatsError::Io)?;

    let mut width: Option<u32> = None;
    let mut height: Option<u32> = None;
    let mut pixel_type: Option<PixelType> = None;
    let mut data_offset: Option<u64> = None;

    // Scan for header lines
    let mut pos = 0usize;
    let text_end = content.len().min(8192);
    let text_region = std::str::from_utf8(&content[..text_end]).unwrap_or("");

    for line in text_region.lines() {
        let trimmed = line.trim();

        // Track position in file
        pos += line.len() + 1; // +1 for newline

        if trimmed.is_empty() || trimmed == "[Data]" {
            // End of header
            data_offset = Some(pos as u64);
            break;
        }

        if let Some(val) = kv_value(trimmed, "XPoints") {
            if let Ok(v) = val.parse::<u32>() {
                width = Some(v);
            }
        } else if let Some(val) = kv_value(trimmed, "YPoints") {
            if let Ok(v) = val.parse::<u32>() {
                height = Some(v);
            }
        } else if let Some(val) = kv_value(trimmed, "DataType") {
            pixel_type = Some(match val.to_ascii_lowercase().as_str() {
                "int16" | "short" => PixelType::Int16,
                "uint16" | "ushort" => PixelType::Uint16,
                "float32" | "float" => PixelType::Float32,
                "int32" | "long" => PixelType::Int32,
                other => {
                    return Err(BioFormatsError::UnsupportedFormat(format!(
                        "TopoMetrix unsupported DataType {other}"
                    )));
                }
            });
        }
    }

    let width = width.filter(|&v| v > 0).ok_or_else(|| {
        BioFormatsError::UnsupportedFormat("TopoMetrix header missing XPoints".into())
    })?;
    let height = height.filter(|&v| v > 0).ok_or_else(|| {
        BioFormatsError::UnsupportedFormat("TopoMetrix header missing YPoints".into())
    })?;
    let data_offset = data_offset.ok_or_else(|| {
        BioFormatsError::UnsupportedFormat("TopoMetrix header missing data marker".into())
    })?;
    let pixel_type = pixel_type.unwrap_or(PixelType::Int16);
    let bps = pixel_type.bytes_per_sample();
    let plane_bytes = (width as u64)
        .checked_mul(height as u64)
        .and_then(|v| v.checked_mul(bps as u64))
        .ok_or_else(|| BioFormatsError::Format("TopoMetrix plane size overflows".into()))?;
    if data_offset + plane_bytes > content.len() as u64 {
        return Err(BioFormatsError::UnsupportedFormat(
            "TopoMetrix pixel payload is shorter than declared dimensions".into(),
        ));
    }
    let meta = ImageMetadata {
        size_x: width,
        size_y: height,
        size_z: 1,
        size_c: 1,
        size_t: 1,
        pixel_type,
        bits_per_pixel: (bps * 8) as u8,
        image_count: 1,
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
    };

    Ok((meta, data_offset))
}

fn kv_value<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    // Accept "key=value" or "key = value"
    let stripped = line.strip_prefix(key)?;
    let stripped = stripped.trim_start();
    let val = stripped.strip_prefix('=')?.trim_start();
    Some(val)
}

impl FormatReader for TopoMetrixReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("tfr") | Some("ffr") | Some("zfr"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let (meta, data_offset) = parse_topometrix(path)?;
        self.path = Some(path.to_path_buf());
        self.meta = Some(meta);
        self.data_offset = data_offset;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.data_offset = 0;
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
        if plane_index != 0 {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let bps = meta.pixel_type.bytes_per_sample();
        let plane_bytes = meta.size_x as usize * meta.size_y as usize * bps;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(self.data_offset))
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
        let meta = self.meta.as_ref().unwrap();
        crop_full_plane("TopoMetrix", &full, meta, 1, x, y, w, h)
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

// ── Unisoku Reader ─────────────────────────────────────────────────────────────

pub struct UnisokuReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    dat_path: Option<PathBuf>,
}

impl UnisokuReader {
    pub fn new() -> Self {
        UnisokuReader {
            path: None,
            meta: None,
            dat_path: None,
        }
    }
}

impl Default for UnisokuReader {
    fn default() -> Self {
        Self::new()
    }
}

fn resolve_unisoku_header_path(path: &Path) -> PathBuf {
    let is_dat = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("dat"))
        .unwrap_or(false);
    if !is_dat {
        return path.to_path_buf();
    }

    let upper = path.with_extension("HDR");
    if upper.exists() {
        return upper;
    }
    let lower = path.with_extension("hdr");
    if lower.exists() {
        return lower;
    }
    upper
}

fn resolve_unisoku_dat_path(header: &Path) -> PathBuf {
    let upper = header.with_extension("DAT");
    if upper.exists() {
        return upper;
    }
    let lower = header.with_extension("dat");
    if lower.exists() {
        return lower;
    }
    upper
}

fn unisoku_pixel_type_from_ascii_data_type(data_type: i32) -> Option<PixelType> {
    let signed = data_type % 2 == 1;
    let bytes = data_type / 2;
    match (bytes, signed) {
        (1, false) => Some(PixelType::Uint8),
        (1, true) => Some(PixelType::Int8),
        (2, false) => Some(PixelType::Uint16),
        (2, true) => Some(PixelType::Int16),
        (4, _) => Some(PixelType::Float32),
        _ => None,
    }
}

fn parse_unisoku_hdr(path: &Path) -> Result<(ImageMetadata, PathBuf)> {
    let header_path = resolve_unisoku_header_path(path);
    let content = std::fs::read_to_string(&header_path).map_err(BioFormatsError::Io)?;

    let mut width: Option<u32> = None;
    let mut height: Option<u32> = None;
    let mut bits: Option<u32> = None;
    let mut pixel_type: Option<PixelType> = None;
    let mut series_metadata = HashMap::new();

    if content.contains(":STM data") {
        let lines: Vec<&str> = content.split('\r').collect();
        let mut i = 0usize;
        while i < lines.len() {
            let key = lines[i].trim();
            i += 1;
            if !key.starts_with(':') {
                continue;
            }

            let mut values = Vec::new();
            while i < lines.len() {
                let value = lines[i].trim();
                if value.starts_with(':') {
                    break;
                }
                if !value.is_empty() {
                    values.push(value);
                }
                i += 1;
            }

            let value = values.join(" ");
            series_metadata.insert(key.to_string(), MetadataValue::String(value.clone()));
            let tokens: Vec<&str> = value.split_whitespace().collect();

            if key == ":data volume(x*y)" && tokens.len() >= 2 {
                width = tokens[0].parse::<u32>().ok();
                height = tokens[1].parse::<u32>().ok();
            } else if key.starts_with(":ascii flag; data type") {
                let type_token = tokens
                    .last()
                    .ok_or_else(|| {
                        BioFormatsError::UnsupportedFormat(
                            "Unisoku header missing ASCII data type".into(),
                        )
                    })?
                    .parse::<i32>()
                    .map_err(|_| {
                        BioFormatsError::UnsupportedFormat(
                            "Unisoku header has invalid ASCII data type".into(),
                        )
                    })?;
                pixel_type = unisoku_pixel_type_from_ascii_data_type(type_token);
                if pixel_type.is_none() {
                    return Err(BioFormatsError::UnsupportedFormat(format!(
                        "Unisoku unsupported ASCII data type {type_token}"
                    )));
                }
            }
        }
    } else {
        for line in content.lines() {
            let line = line.trim();
            if let Some(val) = kv_value(line, "XSIZE") {
                if let Ok(v) = val.parse::<u32>() {
                    width = Some(v);
                }
            } else if let Some(val) = kv_value(line, "YSIZE") {
                if let Ok(v) = val.parse::<u32>() {
                    height = Some(v);
                }
            } else if let Some(val) = kv_value(line, "BIT") {
                if let Ok(v) = val.parse::<u32>() {
                    bits = Some(v);
                }
            }
        }
    }

    let width = width
        .filter(|&v| v > 0)
        .ok_or_else(|| BioFormatsError::UnsupportedFormat("Unisoku header missing XSIZE".into()))?;
    let height = height
        .filter(|&v| v > 0)
        .ok_or_else(|| BioFormatsError::UnsupportedFormat("Unisoku header missing YSIZE".into()))?;
    let pixel_type = match pixel_type {
        Some(pixel_type) => pixel_type,
        None => {
            let bits = bits.ok_or_else(|| {
                BioFormatsError::UnsupportedFormat("Unisoku header missing BIT depth".into())
            })?;
            if bits == 0 {
                return Err(BioFormatsError::UnsupportedFormat(
                    "Unisoku header has invalid BIT depth".into(),
                ));
            } else if bits <= 16 {
                PixelType::Int16
            } else {
                PixelType::Int32
            }
        }
    };
    let bps = pixel_type.bytes_per_sample();

    let dat_path = resolve_unisoku_dat_path(&header_path);
    let plane_bytes = (width as u64)
        .checked_mul(height as u64)
        .and_then(|v| v.checked_mul(bps as u64))
        .ok_or_else(|| BioFormatsError::Format("Unisoku plane size overflows".into()))?;
    let dat_len = std::fs::metadata(&dat_path)
        .map_err(BioFormatsError::Io)?
        .len();
    if dat_len < plane_bytes {
        return Err(BioFormatsError::UnsupportedFormat(
            "Unisoku .dat payload is shorter than declared dimensions".into(),
        ));
    }

    let meta = ImageMetadata {
        size_x: width,
        size_y: height,
        size_z: 1,
        size_c: 1,
        size_t: 1,
        pixel_type,
        bits_per_pixel: (bps * 8) as u8,
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
    };

    Ok((meta, dat_path))
}

impl FormatReader for UnisokuReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            return false;
        };
        if ext.eq_ignore_ascii_case("hdr") {
            return resolve_unisoku_dat_path(path).exists();
        }
        if ext.eq_ignore_ascii_case("dat") {
            return resolve_unisoku_header_path(path).exists();
        }
        false
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header
            .windows(b":STM data".len())
            .any(|window| window == b":STM data")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let header_path = resolve_unisoku_header_path(path);
        let (meta, dat_path) = parse_unisoku_hdr(path)?;
        self.path = Some(header_path);
        self.meta = Some(meta);
        self.dat_path = Some(dat_path);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.dat_path = None;
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
        if plane_index != 0 {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let bps = meta.pixel_type.bytes_per_sample();
        let plane_bytes = meta.size_x as usize * meta.size_y as usize * bps;
        let dat = self
            .dat_path
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?;
        let mut f = std::fs::File::open(dat).map_err(BioFormatsError::Io)?;
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
        let meta = self.meta.as_ref().unwrap();
        crop_full_plane("Unisoku", &full, meta, 1, x, y, w, h)
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
