//! Bruker OPUS FTIR spectroscopy and ISS Vista FLIM format readers.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;

fn r_u32_le(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off+1], b[off+2], b[off+3]])
}

// ─── Bruker OPUS ──────────────────────────────────────────────────────────────
//
// Bruker OPUS is a binary format for FTIR/Raman spectroscopy data.
// The file starts with a block directory. The magic is version-dependent:
//   byte[0] == 0x0A and byte[1] in {0x00, 0x01, 0x02} for versions 5-7.
// Spectral images are stored as 2D or 3D arrays (x, y, wavenumber).

pub struct BrukerOpusReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    data_offset: u64,
}

impl BrukerOpusReader {
    pub fn new() -> Self { BrukerOpusReader { path: None, meta: None, data_offset: 0 } }
}
impl Default for BrukerOpusReader { fn default() -> Self { Self::new() } }

impl FormatReader for BrukerOpusReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        // OPUS files have numeric extensions (.0, .1, ...) or .abs, .dpt
        let ext = path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase());
        match ext.as_deref() {
            Some("abs") | Some("dpt") | Some("spa") => true,
            Some(e) => e.chars().all(|c| c.is_ascii_digit()),
            None => false,
        }
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // OPUS magic: first byte 0x0A, second byte in {0,1,2}
        header.len() >= 2 && header[0] == 0x0A && header[1] <= 0x02
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        // Read header block to find spectral image dimensions
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        let mut hdr = vec![0u8; 512.min(
            f.metadata().map_err(BioFormatsError::Io)?.len() as usize)];
        f.read_exact(&mut hdr).map_err(BioFormatsError::Io)?;

        // OPUS block directory: 4-byte entries at offset 12
        // Each entry: block_type(u32) + offset(u32) + length(u32)
        // For simplicity: try to extract spatial dimensions from block content
        // Default to 1×1 single spectrum if no image data found
        let width  = 1u32;
        let height = 1u32;
        let n_pts  = if hdr.len() >= 8 { r_u32_le(&hdr, 4).max(1) } else { 1 };

        let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
        meta_map.insert("format".into(), MetadataValue::String("Bruker OPUS".into()));
        meta_map.insert("spectral_points".into(), MetadataValue::Int(n_pts as i64));

        self.meta = Some(ImageMetadata {
            size_x: width, size_y: height, size_z: 1, size_c: n_pts, size_t: 1,
            pixel_type: PixelType::Float32, bits_per_pixel: 32,
            image_count: n_pts,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: false, is_interleaved: false, is_indexed: false,
            is_little_endian: true, resolution_count: 1,
            series_metadata: meta_map, lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        });
        self.data_offset = 512;
        self.path = Some(path.to_path_buf());
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
        let plane_bytes = (meta.size_x * meta.size_y) as usize * 4;
        let offset = self.data_offset + plane_index as u64 * plane_bytes as u64;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(offset)).map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; plane_bytes];
        let _ = f.read(&mut buf);
        Ok(buf)
    }

    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let full = self.open_bytes(p)?;
        let meta = self.meta.as_ref().unwrap();
        let row = meta.size_x as usize * 4;
        let out_row = w as usize * 4;
        let mut out = Vec::with_capacity(h as usize * out_row);
        for r in 0..h as usize {
            let src = &full[(y as usize + r) * row..];
            out.extend_from_slice(&src[x as usize*4 .. x as usize*4 + out_row]);
        }
        Ok(out)
    }

    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = ((meta.size_x - tw) / 2, (meta.size_y - th) / 2);
        self.open_bytes_region(p, tx, ty, tw, th)
    }
}

// ─── ISS Vista FLIM ───────────────────────────────────────────────────────────
//
// ISS (formerly ISS Inc.) FLIM data files (.iss).
// Binary format with a header encoding image dimensions.

pub struct IssFlimReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    data_offset: u64,
}

impl IssFlimReader {
    pub fn new() -> Self { IssFlimReader { path: None, meta: None, data_offset: 256 } }
}
impl Default for IssFlimReader { fn default() -> Self { Self::new() } }

impl FormatReader for IssFlimReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension().and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("iss"))
            .unwrap_or(false)
    }
    fn is_this_type_by_bytes(&self, _: &[u8]) -> bool { false }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        let mut hdr = vec![0u8; 256.min(
            f.metadata().map_err(BioFormatsError::Io)?.len() as usize)];
        f.read_exact(&mut hdr).map_err(BioFormatsError::Io)?;

        let width     = if hdr.len() > 11 { r_u32_le(&hdr, 8).max(1)  } else { 256 };
        let height    = if hdr.len() > 15 { r_u32_le(&hdr, 12).max(1) } else { 256 };
        let n_channels= if hdr.len() > 19 { r_u32_le(&hdr, 16).max(1) } else { 1   };

        let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
        meta_map.insert("format".into(), MetadataValue::String("ISS Vista FLIM".into()));

        self.meta = Some(ImageMetadata {
            size_x: width, size_y: height, size_z: 1, size_c: n_channels, size_t: 1,
            pixel_type: PixelType::Float32, bits_per_pixel: 32,
            image_count: n_channels,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: false, is_interleaved: false, is_indexed: false,
            is_little_endian: true, resolution_count: 1,
            series_metadata: meta_map, lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        });
        self.data_offset = 256;
        self.path = Some(path.to_path_buf());
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
        let plane_bytes = (meta.size_x * meta.size_y) as usize * 4;
        let offset = self.data_offset + plane_index as u64 * plane_bytes as u64;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(offset)).map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; plane_bytes];
        let _ = f.read(&mut buf);
        Ok(buf)
    }

    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let full = self.open_bytes(p)?;
        let meta = self.meta.as_ref().unwrap();
        let row = meta.size_x as usize * 4;
        let out_row = w as usize * 4;
        let mut out = Vec::with_capacity(h as usize * out_row);
        for r in 0..h as usize {
            let src = &full[(y as usize + r) * row..];
            out.extend_from_slice(&src[x as usize*4 .. x as usize*4 + out_row]);
        }
        Ok(out)
    }

    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = ((meta.size_x - tw) / 2, (meta.size_y - th) / 2);
        self.open_bytes_region(p, tx, ty, tw, th)
    }
}
