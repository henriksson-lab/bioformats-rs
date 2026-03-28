//! Synthetic "fake" image format for testing.
//!
//! The filename encodes image parameters as `&key=value` pairs before the
//! `.fake` extension.  Example:
//!   `test_&sizeX=512&sizeY=256&sizeZ=5&pixelType=uint16.fake`
//!
//! Defaults: sizeX=512, sizeY=512, sizeZ=1, sizeC=1, sizeT=1, pixelType=uint8.
//! Pixel data is a simple gradient: value = (x + y + plane_index) % 256.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;

pub struct FakeReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl FakeReader {
    pub fn new() -> Self {
        FakeReader { path: None, meta: None }
    }
}

impl Default for FakeReader {
    fn default() -> Self {
        Self::new()
    }
}

fn parse_fake_params(path: &Path) -> ImageMetadata {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();

    let mut size_x = 512u32;
    let mut size_y = 512u32;
    let mut size_z = 1u32;
    let mut size_c = 1u32;
    let mut size_t = 1u32;
    let mut pixel_type = PixelType::Uint8;

    // Parameters are separated by '&' in the filename
    for part in stem.split('&') {
        if let Some((key, val)) = part.split_once('=') {
            let key = key.trim();
            let val = val.trim();
            match key {
                "sizeX" => { if let Ok(v) = val.parse() { size_x = v; } }
                "sizeY" => { if let Ok(v) = val.parse() { size_y = v; } }
                "sizeZ" => { if let Ok(v) = val.parse() { size_z = v; } }
                "sizeC" => { if let Ok(v) = val.parse() { size_c = v; } }
                "sizeT" => { if let Ok(v) = val.parse() { size_t = v; } }
                "pixelType" => {
                    pixel_type = match val.to_ascii_lowercase().as_str() {
                        "uint8"   => PixelType::Uint8,
                        "uint16"  => PixelType::Uint16,
                        "uint32"  => PixelType::Uint32,
                        "int8"    => PixelType::Int8,
                        "int16"   => PixelType::Int16,
                        "int32"   => PixelType::Int32,
                        "float"   | "float32" => PixelType::Float32,
                        "double"  | "float64" => PixelType::Float64,
                        _ => PixelType::Uint8,
                    };
                }
                _ => {}
            }
        }
    }

    let image_count = size_z * size_c * size_t;
    let bps = pixel_type.bytes_per_sample();

    ImageMetadata {
        size_x,
        size_y,
        size_z,
        size_c,
        size_t,
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
        modulo_z: None,
        modulo_c: None,
        modulo_t: None,
    }
}

impl FormatReader for FakeReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("fake"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.meta = Some(parse_fake_params(path));
        self.path = Some(path.to_path_buf());
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
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let bps = meta.pixel_type.bytes_per_sample();
        let w = meta.size_x as usize;
        let h = meta.size_y as usize;
        let mut buf = vec![0u8; w * h * bps];
        let pidx = plane_index as usize;
        for y in 0..h {
            for x in 0..w {
                let val = ((x + y + pidx) % 256) as u8;
                let off = (y * w + x) * bps;
                for b in 0..bps {
                    buf[off + b] = val;
                }
            }
        }
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
            out.extend_from_slice(&src[x as usize * bps..x as usize * bps + out_row]);
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
