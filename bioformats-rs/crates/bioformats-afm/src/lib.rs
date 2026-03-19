//! AFM/STM format readers.
//!
//! - TopoMetrix AFM (.tfr, .ffr, .zfr): text header + binary data
//! - Unisoku STM/AFM (.hdr + .dat): text header with companion binary

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use bioformats_common::error::{BioFormatsError, Result};
use bioformats_common::metadata::{DimensionOrder, ImageMetadata};
use bioformats_common::pixel_type::PixelType;
use bioformats_common::reader::FormatReader;

// ── TopoMetrix Reader ─────────────────────────────────────────────────────────

pub struct TopoMetrixReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    data_offset: u64,
}

impl TopoMetrixReader {
    pub fn new() -> Self {
        TopoMetrixReader { path: None, meta: None, data_offset: 0 }
    }
}

impl Default for TopoMetrixReader {
    fn default() -> Self { Self::new() }
}

fn parse_topometrix(path: &Path) -> Result<(ImageMetadata, u64)> {
    let content = std::fs::read(path).map_err(BioFormatsError::Io)?;

    let mut width = 256u32;
    let mut height = 256u32;
    let mut pixel_type = PixelType::Int16;
    let mut data_offset = 0u64;

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
            data_offset = pos as u64;
            break;
        }

        if let Some(val) = kv_value(trimmed, "XPoints") {
            if let Ok(v) = val.parse::<u32>() { width = v; }
        } else if let Some(val) = kv_value(trimmed, "YPoints") {
            if let Ok(v) = val.parse::<u32>() { height = v; }
        } else if let Some(val) = kv_value(trimmed, "DataType") {
            pixel_type = match val.to_ascii_lowercase().as_str() {
                "int16" | "short" => PixelType::Int16,
                "uint16" | "ushort" => PixelType::Uint16,
                "float32" | "float" => PixelType::Float32,
                "int32" | "long" => PixelType::Int32,
                _ => PixelType::Int16,
            };
        }
    }

    let bps = pixel_type.bytes_per_sample();
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
        let ext = path.extension().and_then(|e| e.to_str())
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
        if plane_index != 0 { return Err(BioFormatsError::PlaneOutOfRange(plane_index)); }
        let bps = meta.pixel_type.bytes_per_sample();
        let plane_bytes = meta.size_x as usize * meta.size_y as usize * bps;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(self.data_offset)).map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; plane_bytes];
        f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
        Ok(buf)
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

// ── Unisoku Reader ─────────────────────────────────────────────────────────────

pub struct UnisokuReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    dat_path: Option<PathBuf>,
}

impl UnisokuReader {
    pub fn new() -> Self {
        UnisokuReader { path: None, meta: None, dat_path: None }
    }
}

impl Default for UnisokuReader {
    fn default() -> Self { Self::new() }
}

fn parse_unisoku_hdr(path: &Path) -> Result<(ImageMetadata, PathBuf)> {
    let content = std::fs::read_to_string(path).map_err(BioFormatsError::Io)?;

    let mut width = 256u32;
    let mut height = 256u32;
    let mut bits = 16u32;

    for line in content.lines() {
        let line = line.trim();
        if let Some(val) = kv_value(line, "XSIZE") {
            if let Ok(v) = val.parse::<u32>() { width = v; }
        } else if let Some(val) = kv_value(line, "YSIZE") {
            if let Ok(v) = val.parse::<u32>() { height = v; }
        } else if let Some(val) = kv_value(line, "BIT") {
            if let Ok(v) = val.parse::<u32>() { bits = v; }
        }
    }

    let pixel_type = if bits <= 16 { PixelType::Int16 } else { PixelType::Int32 };
    let bps = pixel_type.bytes_per_sample();

    // Companion .dat file in same directory
    let dat_path = path.with_extension("dat");

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
    };

    Ok((meta, dat_path))
}

impl FormatReader for UnisokuReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        // Detect .hdr files when companion .dat exists
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            if ext.eq_ignore_ascii_case("hdr") {
                let dat = path.with_extension("dat");
                return dat.exists();
            }
        }
        false
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let (meta, dat_path) = parse_unisoku_hdr(path)?;
        self.path = Some(path.to_path_buf());
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
        if plane_index != 0 { return Err(BioFormatsError::PlaneOutOfRange(plane_index)); }
        let bps = meta.pixel_type.bytes_per_sample();
        let plane_bytes = meta.size_x as usize * meta.size_y as usize * bps;
        let dat = self.dat_path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = std::fs::File::open(dat).map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; plane_bytes];
        f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
        Ok(buf)
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
