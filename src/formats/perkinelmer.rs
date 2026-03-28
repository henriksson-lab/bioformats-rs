//! PerkinElmer format readers.
//!
//! - PerkinElmerReader: UltraVIEW spinning disk (.cfg + .rec)
//! - OpenlabRawReader: Openlab Raw (.raw) with "LBLB" magic
//! - PhotonDynamicsReader: Photon Dynamics (.pds) extension-only

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;

// ── Helpers ──────────────────────────────────────────────────────────────────

fn default_meta(w: u32, h: u32, pt: PixelType) -> ImageMetadata {
    let bps = pt.bytes_per_sample();
    ImageMetadata {
        size_x: w,
        size_y: h,
        size_z: 1,
        size_c: 1,
        size_t: 1,
        pixel_type: pt,
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
    }
}

fn open_bytes_impl(path: &Path, offset: u64, meta: &ImageMetadata, plane_index: u32) -> Result<Vec<u8>> {
    if plane_index != 0 {
        return Err(BioFormatsError::PlaneOutOfRange(plane_index));
    }
    let bps = meta.pixel_type.bytes_per_sample();
    let plane_bytes = meta.size_x as usize * meta.size_y as usize * bps;
    let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
    f.seek(SeekFrom::Start(offset)).map_err(BioFormatsError::Io)?;
    let mut buf = vec![0u8; plane_bytes];
    let _ = f.read(&mut buf).map_err(BioFormatsError::Io)?;
    Ok(buf)
}

fn region_from_full(full: &[u8], meta: &ImageMetadata, x: u32, y: u32, w: u32, h: u32) -> Vec<u8> {
    let bps = meta.pixel_type.bytes_per_sample();
    let row = meta.size_x as usize * bps;
    let out_row = w as usize * bps;
    let mut out = Vec::with_capacity(h as usize * out_row);
    for r in 0..h as usize {
        let src = &full[(y as usize + r) * row..];
        out.extend_from_slice(&src[x as usize * bps..x as usize * bps + out_row]);
    }
    out
}

// ── PerkinElmerReader ─────────────────────────────────────────────────────────

pub struct PerkinElmerReader {
    path: Option<PathBuf>,
    rec_path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl PerkinElmerReader {
    pub fn new() -> Self {
        PerkinElmerReader { path: None, rec_path: None, meta: None }
    }
}

impl Default for PerkinElmerReader {
    fn default() -> Self { Self::new() }
}

fn parse_pe_cfg(path: &Path) -> Result<(ImageMetadata, PathBuf)> {
    let content = std::fs::read_to_string(path).map_err(BioFormatsError::Io)?;
    let mut width = 512u32;
    let mut height = 512u32;
    let mut bytes_per_pixel = 2u32;

    for line in content.lines() {
        let line = line.trim();
        if let Some(v) = kv(line, "Image Width") {
            if let Ok(n) = v.parse() { width = n; }
        } else if let Some(v) = kv(line, "Image Height") {
            if let Ok(n) = v.parse() { height = n; }
        } else if let Some(v) = kv(line, "Bytes Per Pixel") {
            if let Ok(n) = v.parse() { bytes_per_pixel = n; }
        }
    }

    let pixel_type = match bytes_per_pixel {
        1 => PixelType::Uint8,
        4 => PixelType::Uint32,
        _ => PixelType::Uint16,
    };
    let rec_path = path.with_extension("rec");
    Ok((default_meta(width, height, pixel_type), rec_path))
}

fn kv<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let stripped = line.strip_prefix(key)?.trim_start();
    Some(stripped.strip_prefix('=')?.trim_start())
}

impl FormatReader for PerkinElmerReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase());
        if matches!(ext.as_deref(), Some("cfg")) {
            return path.with_extension("rec").exists();
        }
        false
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool { false }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let (meta, rec_path) = parse_pe_cfg(path)?;
        self.path = Some(path.to_path_buf());
        self.rec_path = Some(rec_path);
        self.meta = Some(meta);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None; self.rec_path = None; self.meta = None; Ok(())
    }

    fn series_count(&self) -> usize { 1 }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if s != 0 { Err(BioFormatsError::SeriesOutOfRange(s)) } else { Ok(()) }
    }
    fn series(&self) -> usize { 0 }

    fn metadata(&self) -> &ImageMetadata { self.meta.as_ref().expect("set_id not called") }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let rec = self.rec_path.as_ref().ok_or(BioFormatsError::NotInitialized)?.clone();
        open_bytes_impl(&rec, 0, meta, plane_index)
    }

    fn open_bytes_region(&mut self, plane_index: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let full = self.open_bytes(plane_index)?;
        let meta = self.meta.as_ref().unwrap();
        Ok(region_from_full(&full, meta, x, y, w, h))
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256); let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2; let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}

// ── OpenlabRawReader ──────────────────────────────────────────────────────────

const OPENLAB_MAGIC: &[u8] = b"LBLB";
const OPENLAB_HEADER_SIZE: u64 = 288;

pub struct OpenlabRawReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl OpenlabRawReader {
    pub fn new() -> Self {
        OpenlabRawReader { path: None, meta: None }
    }
}

impl Default for OpenlabRawReader {
    fn default() -> Self { Self::new() }
}

fn parse_openlab(path: &Path) -> Result<ImageMetadata> {
    let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
    if data.len() < OPENLAB_HEADER_SIZE as usize {
        return Err(BioFormatsError::Format("Openlab header too short".into()));
    }

    // Width at offset 8, Height at offset 12, bit_depth at offset 16 (i32 BE)
    let width = i32::from_be_bytes([data[8], data[9], data[10], data[11]]).max(1) as u32;
    let height = i32::from_be_bytes([data[12], data[13], data[14], data[15]]).max(1) as u32;
    let bit_depth = i32::from_be_bytes([data[16], data[17], data[18], data[19]]);

    let pixel_type = match bit_depth {
        8 => PixelType::Uint8,
        32 => PixelType::Float32,
        _ => PixelType::Uint16,
    };

    Ok(default_meta(width, height, pixel_type))
}

impl FormatReader for OpenlabRawReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("raw"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.len() >= 4 && header[0..4] == *OPENLAB_MAGIC
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let meta = parse_openlab(path)?;
        self.path = Some(path.to_path_buf());
        self.meta = Some(meta);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None; self.meta = None; Ok(())
    }

    fn series_count(&self) -> usize { 1 }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if s != 0 { Err(BioFormatsError::SeriesOutOfRange(s)) } else { Ok(()) }
    }
    fn series(&self) -> usize { 0 }

    fn metadata(&self) -> &ImageMetadata { self.meta.as_ref().expect("set_id not called") }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?.clone();
        open_bytes_impl(&path, OPENLAB_HEADER_SIZE, meta, plane_index)
    }

    fn open_bytes_region(&mut self, plane_index: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let full = self.open_bytes(plane_index)?;
        let meta = self.meta.as_ref().unwrap();
        Ok(region_from_full(&full, meta, x, y, w, h))
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256); let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2; let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}

// ── PhotonDynamicsReader ──────────────────────────────────────────────────────

pub struct PhotonDynamicsReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl PhotonDynamicsReader {
    pub fn new() -> Self {
        PhotonDynamicsReader { path: None, meta: None }
    }
}

impl Default for PhotonDynamicsReader {
    fn default() -> Self { Self::new() }
}

fn parse_pds(path: &Path) -> Result<ImageMetadata> {
    let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
    // Try to read width/height as u32 LE at reasonable offsets
    let width = if data.len() >= 8 {
        u32::from_le_bytes([data[4], data[5], data[6], data[7]]).max(1).min(65536)
    } else {
        512
    };
    let height = if data.len() >= 12 {
        u32::from_le_bytes([data[8], data[9], data[10], data[11]]).max(1).min(65536)
    } else {
        512
    };
    // Sanity check: fall back to defaults if unreasonable
    let (width, height) = if width == 0 || height == 0 || width > 65536 || height > 65536 {
        (512, 512)
    } else {
        (width, height)
    };

    Ok(default_meta(width, height, PixelType::Uint16))
}

impl FormatReader for PhotonDynamicsReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("pds"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool { false }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let meta = parse_pds(path)?;
        self.path = Some(path.to_path_buf());
        self.meta = Some(meta);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None; self.meta = None; Ok(())
    }

    fn series_count(&self) -> usize { 1 }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if s != 0 { Err(BioFormatsError::SeriesOutOfRange(s)) } else { Ok(()) }
    }
    fn series(&self) -> usize { 0 }

    fn metadata(&self) -> &ImageMetadata { self.meta.as_ref().expect("set_id not called") }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index != 0 { return Err(BioFormatsError::PlaneOutOfRange(plane_index)); }
        let bps = meta.pixel_type.bytes_per_sample();
        Ok(vec![0u8; meta.size_x as usize * meta.size_y as usize * bps])
    }

    fn open_bytes_region(&mut self, plane_index: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let full = self.open_bytes(plane_index)?;
        let meta = self.meta.as_ref().unwrap();
        Ok(region_from_full(&full, meta, x, y, w, h))
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256); let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2; let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}
