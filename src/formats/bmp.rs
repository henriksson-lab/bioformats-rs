use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;

pub struct BmpReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    pixels: Option<Vec<u8>>,
}

impl BmpReader {
    pub fn new() -> Self {
        BmpReader {
            path: None,
            meta: None,
            pixels: None,
        }
    }
}

impl Default for BmpReader {
    fn default() -> Self {
        Self::new()
    }
}

fn load_bmp(path: &Path) -> Result<(ImageMetadata, Vec<u8>)> {
    use image::GenericImageView;
    let img = image::open(path).map_err(|e| BioFormatsError::Format(e.to_string()))?;
    let (w, h) = img.dimensions();
    let rgb = img.to_rgb8();
    let raw = rgb.into_raw();
    let meta = ImageMetadata {
        size_x: w,
        size_y: h,
        size_z: 1,
        size_c: 3,
        size_t: 1,
        pixel_type: PixelType::Uint8,
        bits_per_pixel: 8,
        image_count: 1,
        dimension_order: DimensionOrder::XYCZT,
        is_rgb: true,
        is_interleaved: true,
        is_indexed: false,
        is_little_endian: true,
        resolution_count: 1,
        ..Default::default()
    };
    Ok((meta, raw))
}

impl FormatReader for BmpReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("bmp"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.starts_with(b"BM")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let (meta, pixels) = load_bmp(path)?;
        self.path = Some(path.to_path_buf());
        self.meta = Some(meta);
        self.pixels = Some(pixels);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.pixels = None;
        Ok(())
    }

    fn series_count(&self) -> usize {
        1
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if s != 0 {
            Err(BioFormatsError::SeriesOutOfRange(s))
        } else {
            Ok(())
        }
    }
    fn series(&self) -> usize {
        0
    }

    fn metadata(&self) -> &ImageMetadata {
        self.meta.as_ref().expect("set_id not called")
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        if plane_index != 0 {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        self.pixels.clone().ok_or(BioFormatsError::NotInitialized)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let full = self.open_bytes(plane_index)?;
        let meta = self.meta.as_ref().unwrap();
        crop_full_plane("BMP", &full, meta, 3, x, y, w, h)
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

use crate::common::writer::FormatWriter;

pub struct BmpWriter {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl BmpWriter {
    pub fn new() -> Self {
        BmpWriter {
            path: None,
            meta: None,
        }
    }
}

impl Default for BmpWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatWriter for BmpWriter {
    fn is_this_type(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("bmp"))
            .unwrap_or(false)
    }

    fn set_metadata(&mut self, meta: &ImageMetadata) -> Result<()> {
        if meta.pixel_type != PixelType::Uint8 {
            return Err(BioFormatsError::UnsupportedFormat(
                "BMP writer only supports Uint8".into(),
            ));
        }
        self.meta = Some(meta.clone());
        Ok(())
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.meta
            .as_ref()
            .ok_or_else(|| BioFormatsError::Format("set_metadata first".into()))?;
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        Ok(())
    }

    fn save_bytes(&mut self, plane_index: u32, data: &[u8]) -> Result<()> {
        if plane_index != 0 {
            return Err(BioFormatsError::Format(
                "BMP writer supports only one plane".into(),
            ));
        }
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let (w, h) = (meta.size_x, meta.size_y);

        let img = image::RgbImage::from_raw(w, h, data.to_vec())
            .map(image::DynamicImage::ImageRgb8)
            .ok_or_else(|| BioFormatsError::InvalidData("bad data length".into()))?;

        img.save(path)
            .map_err(|e| BioFormatsError::Format(e.to_string()))
    }

    fn can_do_stacks(&self) -> bool {
        false
    }
}
