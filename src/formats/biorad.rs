//! Bio-Rad PIC confocal format reader.
//!
//! 76-byte little-endian header followed by raw pixel data.
//! Magic: int16 at offset 54 == 12345

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;

const HEADER_SIZE: u64 = 76;
const FILE_ID: i16 = 12345;

fn r_i16(b: &[u8], off: usize) -> i16 { i16::from_le_bytes([b[off], b[off+1]]) }
fn r_f32(b: &[u8], off: usize) -> f32 {
    f32::from_le_bytes([b[off], b[off+1], b[off+2], b[off+3]])
}

pub struct BioRadReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    npic: u32,
    bytes_per_pixel: usize,
}

impl BioRadReader {
    pub fn new() -> Self {
        BioRadReader { path: None, meta: None, npic: 1, bytes_per_pixel: 1 }
    }
}

impl Default for BioRadReader { fn default() -> Self { Self::new() } }

impl FormatReader for BioRadReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension().and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("pic"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // Magic: file_id at offset 54 == 12345 (little-endian)
        header.len() >= 56 && i16::from_le_bytes([header[54], header[55]]) == FILE_ID
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        let mut hdr = [0u8; HEADER_SIZE as usize];
        f.read_exact(&mut hdr).map_err(BioFormatsError::Io)?;

        if r_i16(&hdr, 54) != FILE_ID {
            return Err(BioFormatsError::Format("Not a Bio-Rad PIC file".into()));
        }

        let nx    = r_i16(&hdr, 0).max(1) as u32;
        let ny    = r_i16(&hdr, 2).max(1) as u32;
        let npic  = r_i16(&hdr, 4).max(1) as u32;
        let byte_format = r_i16(&hdr, 14); // 0=uint16 (2 bytes), 1=uint8 (1 byte)
        let bpp = if byte_format == 1 { 1usize } else { 2usize };
        let pixel_type = if bpp == 1 { PixelType::Uint8 } else { PixelType::Uint16 };
        let name_bytes = &hdr[18..50];
        let name = String::from_utf8_lossy(name_bytes)
            .trim_end_matches('\0').to_string();

        let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
        if !name.is_empty() {
            meta_map.insert("name".into(), MetadataValue::String(name));
        }
        meta_map.insert("lens".into(), MetadataValue::Int(r_i16(&hdr, 64) as i64));
        meta_map.insert("mag_factor".into(), MetadataValue::Float(r_f32(&hdr, 66) as f64));

        self.meta = Some(ImageMetadata {
            size_x: nx,
            size_y: ny,
            size_z: npic,
            size_c: 1,
            size_t: 1,
            pixel_type,
            bits_per_pixel: (bpp * 8) as u8,
            image_count: npic,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: true,
            resolution_count: 1,
            series_metadata: meta_map,
            lookup_table: None,
        });
        self.npic = npic;
        self.bytes_per_pixel = bpp;
        self.path = Some(path.to_path_buf());
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
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let plane_bytes = (meta.size_x * meta.size_y) as usize * self.bytes_per_pixel;
        let offset = HEADER_SIZE + plane_index as u64 * plane_bytes as u64;

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
        let bps = self.bytes_per_pixel;
        let row = meta.size_x as usize * bps;
        let out_row = w as usize * bps;
        let mut out = Vec::with_capacity(h as usize * out_row);
        for r in 0..h as usize {
            let src = &full[(y as usize + r) * row..];
            out.extend_from_slice(&src[x as usize * bps .. x as usize * bps + out_row]);
        }
        Ok(out)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = ((meta.size_x - tw) / 2, (meta.size_y - th) / 2);
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        use crate::common::metadata::MetadataValue;
        use crate::common::ome_metadata::OmeMetadata;
        let meta = self.meta.as_ref()?;
        let mut ome = OmeMetadata::from_image_metadata(meta);
        let img = &mut ome.images[0];
        if let Some(MetadataValue::String(n)) = meta.series_metadata.get("name") {
            img.name = Some(n.clone());
        }
        Some(ome)
    }
}
