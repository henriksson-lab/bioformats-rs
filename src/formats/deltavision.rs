//! Applied Precision DeltaVision (.dv / .r3d) format reader.
//!
//! DeltaVision uses the PRIISM image file format — a 1024-byte header (possibly
//! followed by an extended header) and then raw pixel planes.
//!
//! Magic: int16 at offset 96 == -16224 (bytes [0xA0, 0xC0] little-endian).

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;

const HEADER_SIZE: usize = 1024;
const DV_MAGIC_LE: i16 = -16224; // 0xC0A0 as signed int16 LE

fn r_i16(b: &[u8], off: usize, le: bool) -> i16 {
    let bytes = [b[off], b[off+1]];
    if le { i16::from_le_bytes(bytes) } else { i16::from_be_bytes(bytes) }
}
fn r_i32(b: &[u8], off: usize, le: bool) -> i32 {
    let bytes = [b[off], b[off+1], b[off+2], b[off+3]];
    if le { i32::from_le_bytes(bytes) } else { i32::from_be_bytes(bytes) }
}
fn r_f32(b: &[u8], off: usize, le: bool) -> f32 {
    let bytes = [b[off], b[off+1], b[off+2], b[off+3]];
    if le { f32::from_le_bytes(bytes) } else { f32::from_be_bytes(bytes) }
}

/// Pixel type codes used in .dv files
fn dv_pixel_type(mode: i32) -> (PixelType, u8) {
    match mode {
        0 => (PixelType::Int16,   16),
        1 => (PixelType::Uint16,  16),
        2 => (PixelType::Float32, 32),
        3 => (PixelType::Int16,   16), // complex int16 — report as int16
        4 => (PixelType::Float32, 32), // complex float32
        5 => (PixelType::Uint8,    8),
        6 => (PixelType::Uint8,    8), // RGB, 3 channels
        _ => (PixelType::Int16,   16),
    }
}

pub struct DeltavisionReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    data_offset: u64,
}

impl DeltavisionReader {
    pub fn new() -> Self {
        DeltavisionReader { path: None, meta: None, data_offset: HEADER_SIZE as u64 }
    }
}

impl Default for DeltavisionReader { fn default() -> Self { Self::new() } }

impl FormatReader for DeltavisionReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension().and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("dv") | Some("r3d"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        if header.len() < 98 { return false; }
        // Check magic at offset 96 for both LE and BE
        let le = i16::from_le_bytes([header[96], header[97]]);
        let be = i16::from_be_bytes([header[96], header[97]]);
        le == DV_MAGIC_LE || be == DV_MAGIC_LE
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        let mut hdr = vec![0u8; HEADER_SIZE];
        f.read_exact(&mut hdr).map_err(BioFormatsError::Io)?;

        // Detect endianness
        let magic_le = i16::from_le_bytes([hdr[96], hdr[97]]);
        let le = magic_le == DV_MAGIC_LE;

        let num_x = r_i32(&hdr, 0, le).max(1) as u32;
        let num_y = r_i32(&hdr, 4, le).max(1) as u32;
        let num_z = r_i32(&hdr, 8, le).max(1) as u32; // total sections
        let mode  = r_i32(&hdr, 12, le);
        let ext_hdr_size = r_i32(&hdr, 92, le).max(0) as u64;
        // pixel spacings
        let dx = r_f32(&hdr, 28, le);
        let dy = r_f32(&hdr, 32, le);
        let dz = r_f32(&hdr, 36, le);

        // NumWaves at offset 196, NumTimes at offset 180 (Bio-Formats offsets)
        let num_waves = if hdr.len() > 197 { r_i16(&hdr, 196, le).max(1) as u32 } else { 1 };
        let num_times = if hdr.len() > 181 { r_i16(&hdr, 180, le).max(1) as u32 } else { 1 };

        let (pixel_type, bpp) = dv_pixel_type(mode);
        // mode 6 = RGB (3 channels, uint8)
        let channels = if mode == 6 { 3u32 } else { num_waves };
        let is_rgb = mode == 6;

        // Total sections: for most files numZ = z × t × c
        // Simple decomposition: z = numZ / (channels × times), or just use numZ
        let size_z = (num_z / (channels.max(1) * num_times.max(1))).max(1);
        let image_count = size_z * channels * num_times;

        let data_offset = HEADER_SIZE as u64 + ext_hdr_size;

        let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
        meta_map.insert("pixel_spacing_x".into(), MetadataValue::Float(dx as f64));
        meta_map.insert("pixel_spacing_y".into(), MetadataValue::Float(dy as f64));
        meta_map.insert("pixel_spacing_z".into(), MetadataValue::Float(dz as f64));
        meta_map.insert("dv_mode".into(), MetadataValue::Int(mode as i64));

        self.meta = Some(ImageMetadata {
            size_x: num_x,
            size_y: num_y,
            size_z: size_z,
            size_c: channels,
            size_t: num_times,
            pixel_type,
            bits_per_pixel: bpp,
            image_count,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: le,
            resolution_count: 1,
            series_metadata: meta_map,
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        });
        self.data_offset = data_offset;
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
        let offset = self.data_offset + plane_index as u64 * plane_bytes as u64;
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

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        use crate::common::metadata::MetadataValue;
        use crate::common::ome_metadata::OmeMetadata;
        let meta = self.meta.as_ref()?;
        let mut ome = OmeMetadata::from_image_metadata(meta);
        let img = &mut ome.images[0];
        let get_f = |k: &str| -> Option<f64> {
            if let Some(MetadataValue::Float(v)) = meta.series_metadata.get(k) { Some(*v) } else { None }
        };
        // DeltaVision pixel_spacing is stored in µm
        img.physical_size_x = get_f("pixel_spacing_x");
        img.physical_size_y = get_f("pixel_spacing_y");
        img.physical_size_z = get_f("pixel_spacing_z");
        Some(ome)
    }
}
