//! Norpix StreamPix SEQ and IPLab format readers.

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
fn r_i32_le(b: &[u8], off: usize) -> i32 {
    i32::from_le_bytes([b[off], b[off+1], b[off+2], b[off+3]])
}

// ─── Norpix StreamPix SEQ ─────────────────────────────────────────────────────
//
// StreamPix .seq files have a 1024-byte header with the following layout:
//   Offset   0: Description (24 bytes), often "Norpix seq\0..."
//   Offset  24: Version (i64)
//   Offset  32: Header size (i32)
//   Offset 548: Allocated frames (u32)
//   Offset 572: True image size (u32) = width * height * bytes_per_pixel
//   Offset 592: Description format (u32): 0=mono8, 1=mono16, 2=color24, 100=jpg
//   Offset 596: Width (u32)
//   Offset 600: Height (u32)
//   Offset 604: Bit depth (u32) — bits per pixel (8 or 16)
//   Offset 612: Compression (u32): 0=uncompressed
//
// Pixel data starts at offset 1024.
// Each frame may be preceded by a 4-byte offset table if indexed,
// but for uncompressed data frames are tightly packed.

pub struct NorpixReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    frame_size: usize,
}

impl NorpixReader {
    pub fn new() -> Self { NorpixReader { path: None, meta: None, frame_size: 0 } }
}
impl Default for NorpixReader { fn default() -> Self { Self::new() } }

impl FormatReader for NorpixReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension().and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("seq"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        if header.len() < 24 { return false; }
        // Check description starts with "Norpix seq"
        let desc = std::str::from_utf8(&header[..24]).unwrap_or("");
        desc.starts_with("Norpix seq") || desc.starts_with("Norpix SEQ")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        let mut hdr = vec![0u8; 1024];
        f.read_exact(&mut hdr).map_err(BioFormatsError::Io)?;

        let n_frames   = r_u32_le(&hdr, 548).max(1);
        let img_size   = r_u32_le(&hdr, 572);
        let desc_fmt   = r_u32_le(&hdr, 592);
        let width      = r_u32_le(&hdr, 596).max(1);
        let height     = r_u32_le(&hdr, 600).max(1);
        let bit_depth  = r_u32_le(&hdr, 604);

        let (pixel_type, bpp, channels): (PixelType, u8, u32) = match desc_fmt {
            0   => (PixelType::Uint8,   8, 1),  // mono 8-bit
            1   => (PixelType::Uint16, 16, 1),  // mono 16-bit
            2   => (PixelType::Uint8,   8, 3),  // color BGR24
            101 => (PixelType::Uint16, 16, 1),  // mono 16-bit alt
            _ => {
                // fall back on bit_depth
                if bit_depth <= 8 { (PixelType::Uint8, 8, 1) } else { (PixelType::Uint16, 16, 1) }
            }
        };

        let bps = pixel_type.bytes_per_sample();
        let frame_size = if img_size > 0 {
            img_size as usize
        } else {
            width as usize * height as usize * bps * channels as usize
        };
        let is_rgb = channels == 3;

        let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
        meta_map.insert("format".into(), MetadataValue::String("Norpix StreamPix SEQ".into()));

        self.meta = Some(ImageMetadata {
            size_x: width, size_y: height,
            size_z: n_frames, size_c: channels, size_t: 1,
            pixel_type, bits_per_pixel: bpp,
            image_count: n_frames,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb, is_interleaved: true, is_indexed: false,
            is_little_endian: true, resolution_count: 1,
            series_metadata: meta_map, lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        });
        self.frame_size = frame_size;
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
        let plane_bytes = (meta.size_x * meta.size_y * meta.size_c) as usize * bps;
        let frame = if self.frame_size > 0 { self.frame_size } else { plane_bytes };
        let offset = 1024u64 + plane_index as u64 * frame as u64;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(offset)).map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; plane_bytes];
        f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
        Ok(buf)
    }

    fn open_bytes_region(&mut self, plane_index: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let full = self.open_bytes(plane_index)?;
        let meta = self.meta.as_ref().unwrap();
        let spp = meta.size_c as usize;
        let bps = meta.pixel_type.bytes_per_sample();
        let row = meta.size_x as usize * spp * bps;
        let out_row = w as usize * spp * bps;
        let mut out = Vec::with_capacity(h as usize * out_row);
        for r in 0..h as usize {
            let src = &full[(y as usize + r) * row..];
            out.extend_from_slice(&src[x as usize*spp*bps .. x as usize*spp*bps + out_row]);
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

// ─── IPLab ────────────────────────────────────────────────────────────────────
//
// IPLab (.ipl) is a format from Scanalytics used for multi-dimensional images.
//
// Header layout (little-endian):
//   Offset  0: magic — "ipl bina" (8 bytes) for binary data files
//   Offset  8: version (i32)
//   Offset 12: width (i32)
//   Offset 16: height (i32)
//   Offset 20: depth (i32) — number of z planes
//   Offset 24: n_channels (i32)
//   Offset 28: n_frames (i32) — time points
//   Offset 32: data_type (i32): 0=int8, 1=uint16, 2=int16, 3=float32, 4=uint8, 5=RGB, ...
//   Offset 36: color_mode (i32)
//   Pixel data starts at offset 96.

pub struct IplabReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl IplabReader {
    pub fn new() -> Self { IplabReader { path: None, meta: None } }
}
impl Default for IplabReader { fn default() -> Self { Self::new() } }

impl FormatReader for IplabReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension().and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("ipl") || e.eq_ignore_ascii_case("ipm"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.len() >= 8 && &header[..8] == b"ipl bina"
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        let mut hdr = vec![0u8; 96];
        f.read_exact(&mut hdr).map_err(BioFormatsError::Io)?;

        let width      = r_i32_le(&hdr, 12).max(1) as u32;
        let height     = r_i32_le(&hdr, 16).max(1) as u32;
        let depth      = r_i32_le(&hdr, 20).max(1) as u32;
        let n_channels = r_i32_le(&hdr, 24).max(1) as u32;
        let n_frames   = r_i32_le(&hdr, 28).max(1) as u32;
        let data_type  = r_i32_le(&hdr, 32);

        let (pixel_type, bpp, spp): (PixelType, u8, u32) = match data_type {
            0 => (PixelType::Uint8,   8, 1),  // int8 → report as uint8
            1 => (PixelType::Uint16, 16, 1),
            2 => (PixelType::Int16,  16, 1),
            3 => (PixelType::Float32, 32, 1),
            4 => (PixelType::Uint8,   8, 1),
            5 => (PixelType::Uint8,   8, 3), // RGB
            _ => (PixelType::Uint16, 16, 1),
        };
        let is_rgb = spp == 3;
        let image_count = depth * n_channels * n_frames;

        let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
        meta_map.insert("format".into(), MetadataValue::String("IPLab".into()));

        self.meta = Some(ImageMetadata {
            size_x: width, size_y: height,
            size_z: depth, size_c: n_channels * spp, size_t: n_frames,
            pixel_type, bits_per_pixel: bpp,
            image_count,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb, is_interleaved: is_rgb, is_indexed: false,
            is_little_endian: true, resolution_count: 1,
            series_metadata: meta_map, lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        });
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
        let spp = if meta.is_rgb { 3usize } else { 1usize };
        let plane_bytes = (meta.size_x * meta.size_y) as usize * spp * bps;
        let offset = 96u64 + plane_index as u64 * plane_bytes as u64;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(offset)).map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; plane_bytes];
        f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
        Ok(buf)
    }

    fn open_bytes_region(&mut self, plane_index: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let full = self.open_bytes(plane_index)?;
        let meta = self.meta.as_ref().unwrap();
        let spp = if meta.is_rgb { 3usize } else { 1usize };
        let bps = meta.pixel_type.bytes_per_sample();
        let row = meta.size_x as usize * spp * bps;
        let out_row = w as usize * spp * bps;
        let mut out = Vec::with_capacity(h as usize * out_row);
        for r in 0..h as usize {
            let src = &full[(y as usize + r) * row..];
            out.extend_from_slice(&src[x as usize*spp*bps .. x as usize*spp*bps + out_row]);
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
