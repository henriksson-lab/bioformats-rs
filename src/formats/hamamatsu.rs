//! Hamamatsu DCIMG time-lapse format reader.
//!
//! DCIMG is a proprietary Hamamatsu format for sCMOS camera time-lapse data.
//! The file starts with the 8-byte magic "DCIMG\0\0\0".
//!
//! Header layout (all little-endian):
//!   Offset   0: magic "DCIMG" (5 bytes)
//!   Offset  16: header_size (u32)
//!   Offset  20: n_frames (u32)
//!   Offset  32: width (u32)
//!   Offset  36: height (u32)
//!   Offset  40: bit_depth (u32) — typically 16
//!   Offset  48: bytes_per_row (u32)

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

pub struct DcimgReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    data_offset: u64,
    bytes_per_row: usize,
}

impl DcimgReader {
    pub fn new() -> Self {
        DcimgReader { path: None, meta: None, data_offset: 0, bytes_per_row: 0 }
    }
}

impl Default for DcimgReader { fn default() -> Self { Self::new() } }

impl FormatReader for DcimgReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension().and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("dcimg"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.len() >= 5 && &header[..5] == b"DCIMG"
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        let mut hdr = vec![0u8; 64];
        f.read_exact(&mut hdr).map_err(BioFormatsError::Io)?;

        let header_size   = r_u32_le(&hdr, 16) as u64;
        let n_frames      = r_u32_le(&hdr, 20).max(1);
        let width         = r_u32_le(&hdr, 32).max(1);
        let height        = r_u32_le(&hdr, 36).max(1);
        let bit_depth     = r_u32_le(&hdr, 40);
        let bytes_per_row = r_u32_le(&hdr, 48) as usize;

        let (pixel_type, bpp): (PixelType, u8) = match bit_depth {
            8  => (PixelType::Uint8,   8),
            32 => (PixelType::Float32, 32),
            _  => (PixelType::Uint16, 16), // default: 16-bit
        };

        let bps = pixel_type.bytes_per_sample();
        let bpr = if bytes_per_row > 0 { bytes_per_row } else { width as usize * bps };

        let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
        meta_map.insert("format".into(), MetadataValue::String("Hamamatsu DCIMG".into()));
        meta_map.insert("bit_depth".into(), MetadataValue::Int(bit_depth as i64));

        self.meta = Some(ImageMetadata {
            size_x: width, size_y: height,
            size_z: n_frames, size_c: 1, size_t: 1,
            pixel_type, bits_per_pixel: bpp,
            image_count: n_frames,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: false, is_interleaved: false, is_indexed: false,
            is_little_endian: true, resolution_count: 1,
            series_metadata: meta_map, lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        });
        self.data_offset = if header_size > 64 { header_size } else { 64 };
        self.bytes_per_row = bpr;
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
        let bps = meta.pixel_type.bytes_per_sample();
        let row_bytes = if self.bytes_per_row > 0 { self.bytes_per_row } else { meta.size_x as usize * bps };
        let plane_bytes = row_bytes * meta.size_y as usize;
        let offset = self.data_offset + plane_index as u64 * plane_bytes as u64;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(offset)).map_err(BioFormatsError::Io)?;
        let mut raw = vec![0u8; plane_bytes];
        f.read_exact(&mut raw).map_err(BioFormatsError::Io)?;
        let out_row = meta.size_x as usize * bps;
        if row_bytes == out_row { return Ok(raw); }
        let mut out = Vec::with_capacity(meta.size_y as usize * out_row);
        for r in 0..meta.size_y as usize {
            out.extend_from_slice(&raw[r * row_bytes .. r * row_bytes + out_row]);
        }
        Ok(out)
    }

    fn open_bytes_region(&mut self, plane_index: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let full = self.open_bytes(plane_index)?;
        let meta = self.meta.as_ref().unwrap();
        let bps = meta.pixel_type.bytes_per_sample();
        let row = meta.size_x as usize * bps;
        let out_row = w as usize * bps;
        let mut out = Vec::with_capacity(h as usize * out_row);
        for r in 0..h as usize {
            let src = &full[(y as usize + r) * row..];
            out.extend_from_slice(&src[x as usize*bps .. x as usize*bps + out_row]);
        }
        Ok(out)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = ((meta.size_x - tw) / 2, (meta.size_y - th) / 2);
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}
