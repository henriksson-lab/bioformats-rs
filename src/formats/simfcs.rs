//! SimFCS FLIM binary format reader.
//!
//! SimFCS stores raw binary FLIM data with no file header.
//! The file extension indicates the data type.
//!
//! Also includes LambertFlimReader for Lambert Instruments FLIM .asc files.

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;

// ── SimFCS Reader ─────────────────────────────────────────────────────────────

pub struct SimfcsReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl SimfcsReader {
    pub fn new() -> Self {
        SimfcsReader { path: None, meta: None }
    }
}

impl Default for SimfcsReader {
    fn default() -> Self { Self::new() }
}

fn simfcs_pixel_type(ext: &str) -> Option<PixelType> {
    match ext {
        "b64" => Some(PixelType::Uint8),
        "r64" => Some(PixelType::Float32),
        "i64" => Some(PixelType::Int32),
        _ => None,
    }
}

impl FormatReader for SimfcsReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension().and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("b64") | Some("r64") | Some("i64"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let ext = path.extension().and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .unwrap_or_default();
        let pixel_type = simfcs_pixel_type(&ext)
            .ok_or_else(|| BioFormatsError::Format(format!("Unknown SimFCS extension: {}", ext)))?;

        let bps = pixel_type.bytes_per_sample();
        let file_size = fs::metadata(path).map_err(BioFormatsError::Io)?.len() as usize;
        let frame_bytes = 256 * 256 * bps;
        let n_frames = if frame_bytes > 0 { file_size / frame_bytes } else { 1 };
        let image_count = n_frames.max(1) as u32;

        let meta = ImageMetadata {
            size_x: 256,
            size_y: 256,
            size_z: image_count,
            size_c: 1,
            size_t: 1,
            pixel_type,
            bits_per_pixel: (bps * 8) as u8,
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
        };

        self.path = Some(path.to_path_buf());
        self.meta = Some(meta);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        Ok(())
    }

    fn series_count(&self) -> usize { 1 }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if s != 0 { Err(BioFormatsError::SeriesOutOfRange(s)) } else { Ok(()) }
    }
    fn series(&self) -> usize { 0 }

    fn metadata(&self) -> &ImageMetadata {
        self.meta.as_ref().expect("set_id not called")
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let bps = meta.pixel_type.bytes_per_sample();
        let plane_bytes = 256 * 256 * bps;
        let offset = plane_index as u64 * plane_bytes as u64;

        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = fs::File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(offset)).map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; plane_bytes];
        f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
        Ok(buf)
    }

    fn open_bytes_region(&mut self, plane_index: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let full = self.open_bytes(plane_index)?;
        let meta = self.meta.as_ref().unwrap();
        let bps = meta.pixel_type.bytes_per_sample();
        let row_bytes = 256 * bps;
        let out_row = w as usize * bps;
        let mut out = Vec::with_capacity(h as usize * out_row);
        for row in 0..h as usize {
            let src = &full[(y as usize + row) * row_bytes..];
            let s = x as usize * bps;
            out.extend_from_slice(&src[s..s + out_row]);
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
}

// ── Lambert FLIM Reader ───────────────────────────────────────────────────────

pub struct LambertFlimReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl LambertFlimReader {
    pub fn new() -> Self {
        LambertFlimReader { path: None, meta: None }
    }
}

impl Default for LambertFlimReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for LambertFlimReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension().and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("asc"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // Check for Lambert Instruments ASCII heuristic
        if header.len() < 8 { return false; }
        let s = std::str::from_utf8(&header[..header.len().min(256)]).unwrap_or("");
        s.contains("Lambert") || s.contains("GlobalImages") || s.starts_with('#')
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        // Read header for dimensions
        let content = fs::read_to_string(path).map_err(BioFormatsError::Io)?;
        let mut width = 256u32;
        let mut height = 256u32;

        for line in content.lines() {
            let line = line.trim();
            if let Some(val) = parse_key_value(line, "X-Resolution") {
                if let Ok(v) = val.parse::<u32>() { width = v; }
            } else if let Some(val) = parse_key_value(line, "Width") {
                if let Ok(v) = val.parse::<u32>() { width = v; }
            } else if let Some(val) = parse_key_value(line, "Y-Resolution") {
                if let Ok(v) = val.parse::<u32>() { height = v; }
            } else if let Some(val) = parse_key_value(line, "Height") {
                if let Ok(v) = val.parse::<u32>() { height = v; }
            }
        }

        let meta = ImageMetadata {
            size_x: width,
            size_y: height,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type: PixelType::Float32,
            bits_per_pixel: 32,
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

        self.path = Some(path.to_path_buf());
        self.meta = Some(meta);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        Ok(())
    }

    fn series_count(&self) -> usize { 1 }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if s != 0 { Err(BioFormatsError::SeriesOutOfRange(s)) } else { Ok(()) }
    }
    fn series(&self) -> usize { 0 }

    fn metadata(&self) -> &ImageMetadata {
        self.meta.as_ref().expect("set_id not called")
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        // Return zero-filled placeholder
        Ok(vec![0u8; meta.size_x as usize * meta.size_y as usize * 4])
    }

    fn open_bytes_region(&mut self, plane_index: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let full = self.open_bytes(plane_index)?;
        let meta = self.meta.as_ref().unwrap();
        let bps = meta.pixel_type.bytes_per_sample();
        let row_bytes = meta.size_x as usize * bps;
        let out_row = w as usize * bps;
        let mut out = Vec::with_capacity(h as usize * out_row);
        for row in 0..h as usize {
            let src = &full[(y as usize + row) * row_bytes..];
            let s = x as usize * bps;
            out.extend_from_slice(&src[s..s + out_row]);
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
}

fn parse_key_value<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    // Check for "key=value" or "key: value" patterns
    if let Some(rest) = line.strip_prefix(key) {
        let rest = rest.trim_start();
        if let Some(val) = rest.strip_prefix('=').or_else(|| rest.strip_prefix(':')) {
            return Some(val.trim());
        }
    }
    None
}
