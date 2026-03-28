//! Princeton Instruments SPE format reader (SPE 2.x).
//!
//! The SPE 2.x file has a 4100-byte binary header followed by raw pixel data.
//! Key fields: datatype at offset 108 (i16), xdim at 42 (u16), ydim at 656 (u16),
//! numframes at 1446 (i32).
//!
//! Note: SPE 3.x uses a different XML footer format; this reader supports 2.x.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;

const HEADER_SIZE: u64 = 4100;

fn r_i16_le(b: &[u8], off: usize) -> i16 { i16::from_le_bytes([b[off], b[off+1]]) }
fn r_u16_le(b: &[u8], off: usize) -> u16 { u16::from_le_bytes([b[off], b[off+1]]) }
fn r_i32_le(b: &[u8], off: usize) -> i32 { i32::from_le_bytes([b[off], b[off+1], b[off+2], b[off+3]]) }
fn r_f32_le(b: &[u8], off: usize) -> f32 { f32::from_le_bytes([b[off], b[off+1], b[off+2], b[off+3]]) }

/// SPE datatype codes
fn spe_pixel_type(datatype: i16) -> (PixelType, u8) {
    match datatype {
        0 => (PixelType::Float32, 32),
        1 => (PixelType::Int32,   32),
        2 => (PixelType::Int16,   16),
        3 => (PixelType::Uint16,  16),
        8 => (PixelType::Uint32,  32),
        _ => (PixelType::Uint16,  16),
    }
}

pub struct SpeReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl SpeReader {
    pub fn new() -> Self { SpeReader { path: None, meta: None } }
}

impl Default for SpeReader { fn default() -> Self { Self::new() } }

impl FormatReader for SpeReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension().and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("spe"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        // No universal magic byte; rely on extension
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        let mut hdr = vec![0u8; HEADER_SIZE as usize];
        f.read_exact(&mut hdr).map_err(BioFormatsError::Io)?;

        // SPE 2.x layout (all little-endian):
        //  42: datatype (i16)  — 0=f32, 1=i32, 2=i16, 3=u16, 8=u32
        //  656: xdim (u16)     — image width
        //  6: ydim (u16)       — image height (also at 1510 in some versions)
        //  1446: numframes (i32)
        //  576: exposure_time (f32)
        //  Note: in SPE 2.x spec, "YCOM" (height) = offset 6, "XDIM" = offset 656
        //  and "NUMFRAMES" = offset 1446
        let datatype   = r_i16_le(&hdr, 108);  // confirmed SPE2 offset
        let xdim       = r_u16_le(&hdr, 42).max(1) as u32;  // confirmed
        let ydim       = r_u16_le(&hdr, 656).max(1) as u32; // confirmed
        let numframes  = r_i32_le(&hdr, 1446).max(1) as u32;
        let exp_time   = r_f32_le(&hdr, 10);

        // Date/comment strings (best-effort)
        let date = std::str::from_utf8(&hdr[20..30]).unwrap_or("").trim_end_matches('\0').to_string();

        let (pixel_type, bpp) = spe_pixel_type(datatype);

        let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
        meta_map.insert("exposure_time_s".into(), MetadataValue::Float(exp_time as f64));
        if !date.is_empty() { meta_map.insert("date".into(), MetadataValue::String(date)); }

        self.meta = Some(ImageMetadata {
            size_x: xdim,
            size_y: ydim,
            size_z: numframes,
            size_c: 1,
            size_t: 1,
            pixel_type,
            bits_per_pixel: bpp,
            image_count: numframes,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: true,
            resolution_count: 1,
            series_metadata: meta_map,
            lookup_table: None,
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
        let plane_bytes = (meta.size_x * meta.size_y) as usize * bps;
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

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        use crate::common::metadata::MetadataValue;
        use crate::common::ome_metadata::OmeMetadata;
        let meta = self.meta.as_ref()?;
        let mut ome = OmeMetadata::from_image_metadata(meta);
        // Populate per-plane exposure time from the single exposure stored in the header
        if let Some(MetadataValue::Float(exp)) = meta.series_metadata.get("exposure_time_s") {
            let img = &mut ome.images[0];
            img.planes = (0..meta.image_count).map(|i| {
                let z = i % meta.size_z;
                let c = (i / meta.size_z) % meta.size_c;
                let t = i / (meta.size_z * meta.size_c);
                crate::common::ome_metadata::OmePlane {
                    the_z: z, the_c: c, the_t: t,
                    exposure_time: Some(*exp),
                    ..Default::default()
                }
            }).collect();
        }
        Some(ome)
    }
}
