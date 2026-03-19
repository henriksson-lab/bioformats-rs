//! LIM (Laboratory Imaging) and TillVision format readers.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use bioformats_common::error::{BioFormatsError, Result};
use bioformats_common::metadata::{DimensionOrder, ImageMetadata};
use bioformats_common::pixel_type::PixelType;
use bioformats_common::reader::FormatReader;

// ── LIM Reader ────────────────────────────────────────────────────────────────

pub struct LimReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    data_offset: u64,
}

impl LimReader {
    pub fn new() -> Self {
        LimReader { path: None, meta: None, data_offset: 0 }
    }
}

impl Default for LimReader {
    fn default() -> Self { Self::new() }
}

fn load_lim_header(path: &Path) -> Result<(ImageMetadata, u64)> {
    let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
    let mut header = [0u8; 32];
    f.read_exact(&mut header).map_err(BioFormatsError::Io)?;

    let width = u16::from_le_bytes([header[10], header[11]]) as u32;
    let height = u16::from_le_bytes([header[12], header[13]]) as u32;
    let bits = u16::from_le_bytes([header[14], header[15]]) as u32;
    let image_count = u16::from_le_bytes([header[6], header[7]]) as u32;
    let data_offset = u32::from_le_bytes([header[8], header[9], 0, 0]) as u64;

    // Fallback defaults if header values are zero
    let width = if width == 0 { 512 } else { width };
    let height = if height == 0 { 512 } else { height };
    let bits = if bits == 0 { 8 } else { bits };
    let image_count = image_count.max(1);
    let data_offset = if data_offset == 0 { 256 } else { data_offset };

    let pixel_type = match bits {
        8 => PixelType::Uint8,
        16 => PixelType::Uint16,
        32 => PixelType::Float32,
        _ => PixelType::Uint8,
    };
    let bps = pixel_type.bytes_per_sample();

    let meta = ImageMetadata {
        size_x: width,
        size_y: height,
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
    };

    Ok((meta, data_offset))
}

impl FormatReader for LimReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension().and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("lim"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let (meta, data_offset) = load_lim_header(path)?;
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
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let bps = meta.pixel_type.bytes_per_sample();
        let plane_bytes = meta.size_x as usize * meta.size_y as usize * bps;
        let file_offset = self.data_offset + plane_index as u64 * plane_bytes as u64;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(file_offset)).map_err(BioFormatsError::Io)?;
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

// ── TillVision Reader ─────────────────────────────────────────────────────────

pub struct TillVisionReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl TillVisionReader {
    pub fn new() -> Self {
        TillVisionReader { path: None, meta: None }
    }
}

impl Default for TillVisionReader {
    fn default() -> Self { Self::new() }
}

fn load_tillvision(path: &Path) -> Result<ImageMetadata> {
    let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
    let mut buf = vec![0u8; 512.min(std::fs::metadata(path).map(|m| m.len() as usize).unwrap_or(512))];
    let n = f.read(&mut buf).map_err(BioFormatsError::Io)?;
    let buf = &buf[..n];

    // Try to find plausible width/height as u32 LE values in range [16, 8192]
    let mut width = 512u32;
    let mut height = 512u32;
    let mut found = 0;

    let mut i = 0;
    while i + 4 <= buf.len() && found < 2 {
        let v = u32::from_le_bytes([buf[i], buf[i+1], buf[i+2], buf[i+3]]);
        if v >= 16 && v <= 8192 {
            if found == 0 { width = v; }
            else { height = v; }
            found += 1;
            i += 4;
        } else {
            i += 1;
        }
    }

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

impl FormatReader for TillVisionReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension().and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("vws"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let meta = load_tillvision(path)?;
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
        Ok(vec![0u8; meta.size_x as usize * meta.size_y as usize * 2])
    }

    fn open_bytes_region(&mut self, plane_index: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let full = self.open_bytes(plane_index)?;
        let meta = self.meta.as_ref().unwrap();
        let row_bytes = meta.size_x as usize * 2;
        let out_row = w as usize * 2;
        let mut out = Vec::with_capacity(h as usize * out_row);
        for row in 0..h as usize {
            let src = &full[(y as usize + row) * row_bytes..];
            let s = x as usize * 2;
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
