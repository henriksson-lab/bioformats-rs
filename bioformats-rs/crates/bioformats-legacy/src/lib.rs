//! Legacy and obscure format readers.
//!
//! - KodakBipReader: Kodak thermal camera (.bip)
//! - WoolzReader: Woolz graph-based image format (.wlz) — extension-only placeholder
//! - PictReader: Apple PICT format (.pict, .pct) — extension-only placeholder

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use bioformats_common::error::{BioFormatsError, Result};
use bioformats_common::metadata::{DimensionOrder, ImageMetadata};
use bioformats_common::pixel_type::PixelType;
use bioformats_common::reader::FormatReader;

fn placeholder_meta() -> ImageMetadata {
    ImageMetadata {
        size_x: 512,
        size_y: 512,
        size_z: 1,
        size_c: 1,
        size_t: 1,
        pixel_type: PixelType::Uint8,
        bits_per_pixel: 8,
        image_count: 1,
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

// ── KodakBipReader ────────────────────────────────────────────────────────────

const KODAK_BIP_HEADER_SIZE: u64 = 512;

pub struct KodakBipReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl KodakBipReader {
    pub fn new() -> Self {
        KodakBipReader { path: None, meta: None }
    }
}

impl Default for KodakBipReader {
    fn default() -> Self { Self::new() }
}

fn parse_kodak_bip(path: &Path) -> Result<ImageMetadata> {
    let data = std::fs::read(path).map_err(BioFormatsError::Io)?;

    let (width, height) = if data.len() >= 22 {
        // Offset 16: width (u16 LE), offset 20: height (u16 LE)
        let w = u16::from_le_bytes([data[16], data[17]]) as u32;
        let h = u16::from_le_bytes([data[20], data[21]]) as u32;
        let w = if w == 0 { 512 } else { w };
        let h = if h == 0 { 512 } else { h };
        (w, h)
    } else {
        (512, 512)
    };

    Ok(ImageMetadata {
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
        series_metadata: HashMap::new(),
        lookup_table: None,
    })
}

impl FormatReader for KodakBipReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("bip"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool { false }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let meta = parse_kodak_bip(path)?;
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
        if plane_index != 0 { return Err(BioFormatsError::PlaneOutOfRange(plane_index)); }
        let bps = meta.pixel_type.bytes_per_sample();
        let plane_bytes = meta.size_x as usize * meta.size_y as usize * bps;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?.clone();
        let mut f = std::fs::File::open(&path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(KODAK_BIP_HEADER_SIZE)).map_err(BioFormatsError::Io)?;
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
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}

// ── WoolzReader ───────────────────────────────────────────────────────────────

pub struct WoolzReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl WoolzReader {
    pub fn new() -> Self {
        WoolzReader { path: None, meta: None }
    }
}

impl Default for WoolzReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for WoolzReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("wlz"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool { false }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.path = Some(path.to_path_buf());
        self.meta = Some(placeholder_meta());
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
        Ok(vec![0u8; meta.size_x as usize * meta.size_y as usize * bps])
    }

    fn open_bytes_region(&mut self, plane_index: u32, _x: u32, _y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let bps = meta.pixel_type.bytes_per_sample();
        Ok(vec![0u8; w as usize * h as usize * bps])
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

// ── PictReader ────────────────────────────────────────────────────────────────

pub struct PictReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl PictReader {
    pub fn new() -> Self {
        PictReader { path: None, meta: None }
    }
}

impl Default for PictReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for PictReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("pict") | Some("pct"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool { false }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.path = Some(path.to_path_buf());
        self.meta = Some(placeholder_meta());
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
        Ok(vec![0u8; meta.size_x as usize * meta.size_y as usize * bps])
    }

    fn open_bytes_region(&mut self, plane_index: u32, _x: u32, _y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let bps = meta.pixel_type.bytes_per_sample();
        Ok(vec![0u8; w as usize * h as usize * bps])
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
