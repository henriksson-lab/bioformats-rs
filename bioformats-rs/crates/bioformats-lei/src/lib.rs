//! Leica LEI older confocal format reader.
//!
//! LEI (Leica Image) is an older format used by Leica confocal systems.
//! The .lei file is a binary file referencing one or more .lif or raw image
//! data files in the same directory.
//!
//! Header layout (LE):
//!   Offset  0: magic (u32): 0x49494949 or 'ILIS'
//!   Offset  8: width  (u32)
//!   Offset 12: height (u32)
//!   Offset 16: depth  (u32)  — z-planes
//!   Offset 20: channels (u32)
//!   Offset 24: bit_depth (u32)
//!   Data at offset 512.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use bioformats_common::error::{BioFormatsError, Result};
use bioformats_common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use bioformats_common::pixel_type::PixelType;
use bioformats_common::reader::FormatReader;

fn r_u32_le(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off+1], b[off+2], b[off+3]])
}

pub struct LeiReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    data_offset: u64,
}

impl LeiReader {
    pub fn new() -> Self { LeiReader { path: None, meta: None, data_offset: 512 } }
}
impl Default for LeiReader { fn default() -> Self { Self::new() } }

impl FormatReader for LeiReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension().and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("lei"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // "ILIS" magic or 0x49 repeated
        header.len() >= 4 && (&header[..4] == b"ILIS" || header[..4] == [0x49,0x49,0x49,0x49])
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        let mut hdr = vec![0u8; 512.min(
            f.metadata().map_err(BioFormatsError::Io)?.len() as usize
        )];
        f.read_exact(&mut hdr).map_err(BioFormatsError::Io)?;

        let width     = if hdr.len() > 11 { r_u32_le(&hdr, 8).max(1)  } else { 512 };
        let height    = if hdr.len() > 15 { r_u32_le(&hdr, 12).max(1) } else { 512 };
        let depth     = if hdr.len() > 19 { r_u32_le(&hdr, 16).max(1) } else { 1   };
        let channels  = if hdr.len() > 23 { r_u32_le(&hdr, 20).max(1) } else { 1   };
        let bit_depth = if hdr.len() > 27 { r_u32_le(&hdr, 24)        } else { 8   };

        let (pixel_type, bpp): (PixelType, u8) = match bit_depth {
            8  => (PixelType::Uint8,   8),
            16 => (PixelType::Uint16, 16),
            32 => (PixelType::Float32, 32),
            _  => (PixelType::Uint8,   8),
        };

        let image_count = depth * channels;
        let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
        meta_map.insert("format".into(), MetadataValue::String("Leica LEI".into()));

        self.meta = Some(ImageMetadata {
            size_x: width, size_y: height, size_z: depth, size_c: channels, size_t: 1,
            pixel_type, bits_per_pixel: bpp, image_count,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: false, is_interleaved: false, is_indexed: false,
            is_little_endian: true, resolution_count: 1,
            series_metadata: meta_map, lookup_table: None,
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
        let bps = meta.pixel_type.bytes_per_sample();
        let plane_bytes = (meta.size_x * meta.size_y) as usize * bps;
        let offset = self.data_offset + plane_index as u64 * plane_bytes as u64;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(offset)).map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; plane_bytes];
        let _ = f.read(&mut buf).map_err(BioFormatsError::Io)?;
        Ok(buf)
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
