//! Scanco AIM micro-CT format reader.
//!
//! Supports ISQ (.isq) and AIM (.aim) files from Scanco Medical micro-CT scanners.
//! ISQ files have magic "CTDATA-HEADER_V1" and a 512-byte header.
//! AIM files use extension-only detection.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use bioformats_common::error::{BioFormatsError, Result};
use bioformats_common::metadata::{DimensionOrder, ImageMetadata};
use bioformats_common::pixel_type::PixelType;
use bioformats_common::reader::FormatReader;

pub struct AimReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    data_offset: u64,
}

impl AimReader {
    pub fn new() -> Self {
        AimReader { path: None, meta: None, data_offset: 512 }
    }
}

impl Default for AimReader {
    fn default() -> Self { Self::new() }
}

fn load_aim_header(path: &Path) -> Result<(ImageMetadata, u64)> {
    let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
    let mut header = [0u8; 512];
    let n = f.read(&mut header).map_err(BioFormatsError::Io)?;
    let header = &header[..n];

    // Check for ISQ magic
    let is_isq = header.len() >= 20 && &header[..16] == b"CTDATA-HEADER_V1";
    let is_aim = !is_isq && (
        (header.len() >= 4 && &header[..4] == b"!AIM") ||
        path.extension().and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("aim"))
            .unwrap_or(false)
    );

    let (width, height, depth, data_offset) = if is_isq && header.len() >= 44 {
        let w = i32::from_le_bytes([header[28], header[29], header[30], header[31]]).max(1) as u32;
        let h = i32::from_le_bytes([header[32], header[33], header[34], header[35]]).max(1) as u32;
        let d = i32::from_le_bytes([header[36], header[37], header[38], header[39]]).max(1) as u32;
        (w, h, d, 512u64)
    } else if is_aim {
        // Try to find ISQ-style dimensions in first 512 bytes
        // Scan for plausible i32 LE values in range [1, 4096]
        let mut dims = Vec::new();
        let mut i = 4usize; // skip magic
        while i + 4 <= header.len() && dims.len() < 3 {
            let v = i32::from_le_bytes([header[i], header[i+1], header[i+2], header[i+3]]);
            if v >= 16 && v <= 4096 {
                dims.push(v as u32);
                i += 4;
            } else {
                i += 1;
            }
        }
        let w = dims.get(0).copied().unwrap_or(256);
        let h = dims.get(1).copied().unwrap_or(256);
        let d = dims.get(2).copied().unwrap_or(1);
        (w, h, d, 512u64)
    } else {
        (256, 256, 1, 512u64)
    };

    let image_count = depth.max(1);

    let meta = ImageMetadata {
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
    };

    Ok((meta, data_offset))
}

impl FormatReader for AimReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension().and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("aim") | Some("isq"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // ISQ magic: "CTDATA-HEADER_V1"
        header.len() >= 16 && &header[..16] == b"CTDATA-HEADER_V1"
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
