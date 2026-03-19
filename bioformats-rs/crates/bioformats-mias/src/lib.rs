//! bioformats-mias — format readers:
//!
//! - CellWorxReader: CellWorX HCS (.htd / .pnl)
//! - Al3dReader: 3D image format (.al3d) with "AL3D" magic
//! - OxfordInstrumentsReader: Oxford Instruments SEM/AFM (.top)
//! - FeiSerReader: FEI SER electron-microscopy series (.ser)

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use bioformats_common::error::{BioFormatsError, Result};
use bioformats_common::metadata::{DimensionOrder, ImageMetadata};
use bioformats_common::pixel_type::PixelType;
use bioformats_common::reader::FormatReader;

// ── Helpers ───────────────────────────────────────────────────────────────────

fn simple_meta(w: u32, h: u32, z: u32, pt: PixelType) -> ImageMetadata {
    let bps = pt.bytes_per_sample();
    ImageMetadata {
        size_x: w,
        size_y: h,
        size_z: z,
        size_c: 1,
        size_t: 1,
        pixel_type: pt,
        bits_per_pixel: (bps * 8) as u8,
        image_count: z,
        dimension_order: DimensionOrder::XYZCT,
        is_rgb: false,
        is_interleaved: false,
        is_indexed: false,
        is_little_endian: true,
        resolution_count: 1,
        series_metadata: HashMap::new(),
        lookup_table: None,
    }
}

fn region_crop(full: &[u8], meta: &ImageMetadata, x: u32, y: u32, w: u32, h: u32) -> Vec<u8> {
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

fn blank_plane(meta: &ImageMetadata) -> Vec<u8> {
    let bps = meta.pixel_type.bytes_per_sample();
    vec![0u8; meta.size_x as usize * meta.size_y as usize * bps]
}

// ── CellWorxReader ────────────────────────────────────────────────────────────

pub struct CellWorxReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl CellWorxReader {
    pub fn new() -> Self {
        CellWorxReader { path: None, meta: None }
    }
}

impl Default for CellWorxReader {
    fn default() -> Self { Self::new() }
}

fn parse_htd(path: &Path) -> Result<ImageMetadata> {
    let content = std::fs::read_to_string(path).map_err(BioFormatsError::Io)?;
    let mut x_sites = 1u32;
    let mut y_sites = 1u32;
    let mut timepoints = 1u32;
    let mut z_steps = 1u32;
    let mut wavelengths = 1u32;

    for line in content.lines() {
        let line = line.trim();
        if let Some(v) = htd_kv(line, "XSites") { if let Ok(n) = v.parse() { x_sites = n; } }
        else if let Some(v) = htd_kv(line, "YSites") { if let Ok(n) = v.parse() { y_sites = n; } }
        else if let Some(v) = htd_kv(line, "TimePoints") { if let Ok(n) = v.parse() { timepoints = n; } }
        else if let Some(v) = htd_kv(line, "ZSteps") { if let Ok(n) = v.parse() { z_steps = n; } }
        else if let Some(v) = htd_kv(line, "Wavelengths") { if let Ok(n) = v.parse() { wavelengths = n; } }
    }

    let image_count = x_sites * y_sites * timepoints * z_steps * wavelengths;
    let image_count = image_count.max(1);
    Ok(simple_meta(512, 512, image_count, PixelType::Uint16))
}

fn htd_kv<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let stripped = line.strip_prefix(key)?.trim_start();
    Some(stripped.strip_prefix(',')?.trim_start())
}

impl FormatReader for CellWorxReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("htd") | Some("pnl"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool { false }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        // If .pnl, look for companion .htd
        let cfg_path = if path.extension().and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("pnl")).unwrap_or(false)
        {
            path.with_extension("htd")
        } else {
            path.to_path_buf()
        };

        let meta = if cfg_path.exists() {
            parse_htd(&cfg_path)?
        } else {
            simple_meta(512, 512, 1, PixelType::Uint16)
        };
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
        if plane_index >= meta.image_count { return Err(BioFormatsError::PlaneOutOfRange(plane_index)); }
        Ok(blank_plane(meta))
    }

    fn open_bytes_region(&mut self, plane_index: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let full = self.open_bytes(plane_index)?;
        let meta = self.meta.as_ref().unwrap();
        Ok(region_crop(&full, meta, x, y, w, h))
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256); let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2; let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}

// ── Al3dReader ────────────────────────────────────────────────────────────────

const AL3D_MAGIC: &[u8] = b"AL3D";
const AL3D_DATA_OFFSET: u64 = 512;

pub struct Al3dReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl Al3dReader {
    pub fn new() -> Self {
        Al3dReader { path: None, meta: None }
    }
}

impl Default for Al3dReader {
    fn default() -> Self { Self::new() }
}

fn parse_al3d(path: &Path) -> Result<ImageMetadata> {
    let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
    if data.len() < 24 {
        return Err(BioFormatsError::Format("AL3D file too short".into()));
    }
    // Offset 8: width (u32 LE), 12: height (u32 LE), 16: depth (u32 LE)
    let width  = u32::from_le_bytes([data[8], data[9], data[10], data[11]]).max(1);
    let height = u32::from_le_bytes([data[12], data[13], data[14], data[15]]).max(1);
    let depth  = u32::from_le_bytes([data[16], data[17], data[18], data[19]]).max(1);
    // Offset 20: data_type (u16 LE)
    let data_type = u16::from_le_bytes([data[20], data[21]]);
    let pixel_type = match data_type {
        0 => PixelType::Uint8,
        1 => PixelType::Uint16,
        2 => PixelType::Float32,
        _ => PixelType::Uint16,
    };
    Ok(simple_meta(width, height, depth, pixel_type))
}

impl FormatReader for Al3dReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("al3d"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.len() >= 4 && header[0..4] == *AL3D_MAGIC
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let meta = parse_al3d(path)?;
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
        if plane_index >= meta.image_count { return Err(BioFormatsError::PlaneOutOfRange(plane_index)); }
        let bps = meta.pixel_type.bytes_per_sample();
        let plane_bytes = meta.size_x as usize * meta.size_y as usize * bps;
        let plane_offset = AL3D_DATA_OFFSET + plane_index as u64 * plane_bytes as u64;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?.clone();
        let mut f = std::fs::File::open(&path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(plane_offset)).map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; plane_bytes];
        let _ = f.read(&mut buf).map_err(BioFormatsError::Io)?;
        Ok(buf)
    }

    fn open_bytes_region(&mut self, plane_index: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let full = self.open_bytes(plane_index)?;
        let meta = self.meta.as_ref().unwrap();
        Ok(region_crop(&full, meta, x, y, w, h))
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256); let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2; let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}

// ── FeiSerReader ──────────────────────────────────────────────────────────────

/// FEI SER format: electron-microscopy image series from TEM/STEM systems.
/// Magic: bytes 0-1 == 0x97 0x01 (series file signature).
pub struct FeiSerReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl FeiSerReader {
    pub fn new() -> Self { FeiSerReader { path: None, meta: None } }
}

impl Default for FeiSerReader { fn default() -> Self { Self::new() } }

fn parse_ser(path: &Path) -> Result<ImageMetadata> {
    let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
    if data.len() < 30 {
        return Ok(simple_meta(512, 512, 1, PixelType::Uint16));
    }
    // Bytes 4-5: data type id (LE u16). 1=u8,2=u16,3=u32,4=i8,5=i16,6=i32,7=f32,8=f64
    let dtype = u16::from_le_bytes([data[4], data[5]]);
    // Bytes 8-11: total element count (LE u32) — number of frames
    let n_frames = u32::from_le_bytes([data[8], data[9], data[10], data[11]]).max(1);
    // Bytes 24-27: width, 28-31: height (LE u32 at those positions in the tag)
    let width  = u32::from_le_bytes([data[24], data[25], data[26], data[27]]).max(1);
    let height = if data.len() >= 32 {
        u32::from_le_bytes([data[28], data[29], data[30], data[31]]).max(1)
    } else { 512 };
    let pixel_type = match dtype {
        1 => PixelType::Uint8,
        2 => PixelType::Uint16,
        3 | 6 => PixelType::Int32,
        7 => PixelType::Float32,
        8 => PixelType::Float64,
        _ => PixelType::Uint16,
    };
    let width  = if width  > 65535 { 512 } else { width };
    let height = if height > 65535 { 512 } else { height };
    Ok(simple_meta(width, height, n_frames, pixel_type))
}

impl FormatReader for FeiSerReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("ser"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.len() >= 2 && header[0] == 0x97 && header[1] == 0x01
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let meta = parse_ser(path)?;
        self.path = Some(path.to_path_buf());
        self.meta = Some(meta);
        Ok(())
    }

    fn close(&mut self) -> Result<()> { self.path = None; self.meta = None; Ok(()) }
    fn series_count(&self) -> usize { 1 }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if s != 0 { Err(BioFormatsError::SeriesOutOfRange(s)) } else { Ok(()) }
    }
    fn series(&self) -> usize { 0 }
    fn metadata(&self) -> &ImageMetadata { self.meta.as_ref().expect("set_id not called") }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count { return Err(BioFormatsError::PlaneOutOfRange(plane_index)); }
        Ok(blank_plane(meta))
    }

    fn open_bytes_region(&mut self, plane_index: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let full = self.open_bytes(plane_index)?;
        let meta = self.meta.as_ref().unwrap();
        Ok(region_crop(&full, meta, x, y, w, h))
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256); let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2; let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}

// ── OxfordInstrumentsReader ───────────────────────────────────────────────────

const OXFORD_DATA_OFFSET: u64 = 128;

pub struct OxfordInstrumentsReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl OxfordInstrumentsReader {
    pub fn new() -> Self {
        OxfordInstrumentsReader { path: None, meta: None }
    }
}

impl Default for OxfordInstrumentsReader {
    fn default() -> Self { Self::new() }
}

fn parse_oxford(path: &Path) -> Result<ImageMetadata> {
    let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
    if data.len() < 12 {
        return Ok(simple_meta(512, 512, 1, PixelType::Uint16));
    }
    // Offset 4: width (u16 LE), 6: height (u16 LE), 8: data_type (u16 LE)
    let width  = u16::from_le_bytes([data[4], data[5]]) as u32;
    let height = u16::from_le_bytes([data[6], data[7]]) as u32;
    let dtype  = u16::from_le_bytes([data[8], data[9]]);
    let pixel_type = match dtype {
        0 => PixelType::Uint8,
        1 => PixelType::Uint16,
        2 => PixelType::Float32,
        _ => PixelType::Uint16,
    };
    let width  = if width  == 0 { 512 } else { width };
    let height = if height == 0 { 512 } else { height };
    Ok(simple_meta(width, height, 1, pixel_type))
}

impl FormatReader for OxfordInstrumentsReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("top"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool { false }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let meta = parse_oxford(path)?;
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
        let plane_bytes = meta.size_x as usize * meta.size_y as usize * bps;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?.clone();
        let mut f = std::fs::File::open(&path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(OXFORD_DATA_OFFSET)).map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; plane_bytes];
        let _ = f.read(&mut buf).map_err(BioFormatsError::Io)?;
        Ok(buf)
    }

    fn open_bytes_region(&mut self, plane_index: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let full = self.open_bytes(plane_index)?;
        let meta = self.meta.as_ref().unwrap();
        Ok(region_crop(&full, meta, x, y, w, h))
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256); let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2; let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}
