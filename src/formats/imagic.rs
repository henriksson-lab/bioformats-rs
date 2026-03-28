//! IMAGIC electron microscopy format reader (.hed + .img).
//!
//! IMAGIC-5 stores images as a pair of files:
//!   .hed — header file (one 1024-byte record per image, each as 256 int32 values)
//!   .img — pixel data file (images stored sequentially)
//!
//! Key header record fields:
//!   Word 1 (off   0): IMGNUM  (i32) — image serial number
//!   Word 2 (off   4): NROWS   (i32) — image height
//!   Word 3 (off   8): NPIXEL  (i32) — total pixels = width × height
//!   Word 6 (off  20): IFORM   (i32) — pixel format: 0=uint8, 1=int16, 2=float32, 3=complex

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;

const HDR_RECORD_BYTES: usize = 1024;

fn r_i32_le(b: &[u8], off: usize) -> i32 {
    i32::from_le_bytes([b[off], b[off+1], b[off+2], b[off+3]])
}

fn imagic_pixel_type(iform: i32) -> (PixelType, u8) {
    match iform {
        0 => (PixelType::Uint8,   8),
        1 => (PixelType::Int16,  16),
        2 => (PixelType::Float32, 32),
        3 => (PixelType::Float32, 32), // complex = 2×float32, report as float32 per value
        _ => (PixelType::Float32, 32), // default to float32
    }
}

pub struct ImagicReader {
    hed_path: Option<PathBuf>,
    img_path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    bytes_per_sample: usize,
}

impl ImagicReader {
    pub fn new() -> Self {
        ImagicReader { hed_path: None, img_path: None, meta: None, bytes_per_sample: 4 }
    }
}

impl Default for ImagicReader { fn default() -> Self { Self::new() } }

impl FormatReader for ImagicReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("hed") | Some("img"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // Heuristic: IMGNUM at offset 0 should be 1, NROWS at 4 should be positive
        if header.len() < 12 { return false; }
        let imgnum = r_i32_le(header, 0);
        let nrows  = r_i32_le(header, 4);
        let npixel = r_i32_le(header, 8);
        imgnum == 1 && nrows > 0 && nrows < 65536 && npixel > 0 && npixel % nrows == 0
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        // Determine .hed and .img paths
        let stem = path.file_stem().unwrap_or_default();
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let hed_path = if path.extension().and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("hed")).unwrap_or(false)
        {
            path.to_path_buf()
        } else {
            parent.join(format!("{}.hed", stem.to_string_lossy()))
        };
        let img_path = parent.join(format!("{}.img", stem.to_string_lossy()));

        // Read first .hed record
        let mut f = File::open(&hed_path).map_err(BioFormatsError::Io)?;
        let file_len = f.metadata().map_err(BioFormatsError::Io)?.len();
        let num_images = (file_len / HDR_RECORD_BYTES as u64).max(1);

        let mut rec = vec![0u8; HDR_RECORD_BYTES];
        f.read_exact(&mut rec).map_err(BioFormatsError::Io)?;

        let nrows  = r_i32_le(&rec, 4).max(1) as u32;
        let npixel = r_i32_le(&rec, 8).max(1) as u32;
        let iform  = r_i32_le(&rec, 20);
        let ncols  = npixel / nrows;

        let (pixel_type, bpp) = imagic_pixel_type(iform);

        let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
        meta_map.insert("format".into(), MetadataValue::String("IMAGIC-5 EM".into()));
        meta_map.insert("iform".into(), MetadataValue::Int(iform as i64));

        self.meta = Some(ImageMetadata {
            size_x: ncols, size_y: nrows,
            size_z: num_images as u32, size_c: 1, size_t: 1,
            pixel_type, bits_per_pixel: bpp,
            image_count: num_images as u32,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: false, is_interleaved: false, is_indexed: false,
            is_little_endian: true, resolution_count: 1,
            series_metadata: meta_map, lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        });
        self.bytes_per_sample = pixel_type.bytes_per_sample();
        self.hed_path = Some(hed_path);
        self.img_path = Some(img_path);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.hed_path = None; self.img_path = None; self.meta = None; Ok(())
    }
    fn series_count(&self) -> usize { 1 }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if s != 0 { Err(BioFormatsError::SeriesOutOfRange(s)) } else { Ok(()) }
    }
    fn series(&self) -> usize { 0 }
    fn metadata(&self) -> &ImageMetadata { self.meta.as_ref().expect("set_id not called") }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count { return Err(BioFormatsError::PlaneOutOfRange(plane_index)); }
        let plane_bytes = (meta.size_x * meta.size_y) as usize * self.bytes_per_sample;
        let offset = plane_index as u64 * plane_bytes as u64;
        let img_path = self.img_path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = File::open(img_path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(offset)).map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; plane_bytes];
        f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
        Ok(buf)
    }

    fn open_bytes_region(&mut self, plane_index: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let full = self.open_bytes(plane_index)?;
        let meta = self.meta.as_ref().unwrap();
        let bps = self.bytes_per_sample;
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
