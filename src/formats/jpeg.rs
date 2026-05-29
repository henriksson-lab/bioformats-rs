use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;
use std::path::{Path, PathBuf};

pub struct JpegReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    pixels: Option<Vec<u8>>,
}

impl JpegReader {
    pub fn new() -> Self {
        JpegReader {
            path: None,
            meta: None,
            pixels: None,
        }
    }
}

impl Default for JpegReader {
    fn default() -> Self {
        Self::new()
    }
}

fn load_jpeg(path: &Path) -> Result<(ImageMetadata, Vec<u8>)> {
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

impl FormatReader for JpegReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| matches!(e.to_ascii_lowercase().as_str(), "jpg" | "jpeg"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.starts_with(&[0xFF, 0xD8, 0xFF])
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let (meta, pixels) = load_jpeg(path)?;
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
        usize::from(self.meta.is_some())
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.meta.is_none() || s != 0 {
            Err(BioFormatsError::SeriesOutOfRange(s))
        } else {
            Ok(())
        }
    }
    fn series(&self) -> usize {
        0
    }

    fn metadata(&self) -> &ImageMetadata {
        self.meta
            .as_ref()
            .unwrap_or(crate::common::reader::uninitialized_metadata())
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
        crop_full_plane("JPEG", &full, meta, 3, x, y, w, h)
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

pub struct JpegWriter {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    quality: u8,
    wrote: bool,
}

impl JpegWriter {
    pub fn new() -> Self {
        JpegWriter {
            path: None,
            meta: None,
            quality: 90,
            wrote: false,
        }
    }
    pub fn with_quality(mut self, q: u8) -> Self {
        self.quality = q;
        self
    }
}

impl Default for JpegWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatWriter for JpegWriter {
    fn is_this_type(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| matches!(e.to_ascii_lowercase().as_str(), "jpg" | "jpeg"))
            .unwrap_or(false)
    }

    fn set_metadata(&mut self, meta: &ImageMetadata) -> Result<()> {
        let logical_c = if meta.is_rgb { 1 } else { meta.size_c.max(1) };
        let required_planes = meta
            .size_z
            .max(1)
            .checked_mul(logical_c)
            .and_then(|v| v.checked_mul(meta.size_t.max(1)))
            .ok_or_else(|| BioFormatsError::Format("JPEG writer plane count overflow".into()))?;
        if required_planes > 1 || meta.image_count > 1 {
            return Err(BioFormatsError::UnsupportedFormat(
                "JPEG writer supports only one plane".into(),
            ));
        }
        if meta.pixel_type != PixelType::Uint8 {
            return Err(BioFormatsError::UnsupportedFormat(
                "JPEG writer only supports Uint8".into(),
            ));
        }
        self.meta = Some(meta.clone());
        self.wrote = false;
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
        if self.path.is_some() && !self.wrote {
            return Err(BioFormatsError::Format(
                "JPEG writer closed before plane 0 was written".into(),
            ));
        }
        self.path = None;
        self.meta = None;
        self.wrote = false;
        Ok(())
    }

    fn save_bytes(&mut self, plane_index: u32, data: &[u8]) -> Result<()> {
        if plane_index != 0 {
            return Err(BioFormatsError::Format(
                "JPEG writer supports only one plane".into(),
            ));
        }
        if self.wrote {
            return Err(BioFormatsError::Format(
                "JPEG writer already wrote plane 0".into(),
            ));
        }
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let (w, h) = (meta.size_x, meta.size_y);
        let spp = meta.size_c as usize;

        let img: image::DynamicImage = match spp {
            1 => image::GrayImage::from_raw(w, h, data.to_vec())
                .map(image::DynamicImage::ImageLuma8)
                .ok_or_else(|| BioFormatsError::InvalidData("bad data length".into()))?,
            3 => image::RgbImage::from_raw(w, h, data.to_vec())
                .map(image::DynamicImage::ImageRgb8)
                .ok_or_else(|| BioFormatsError::InvalidData("bad data length".into()))?,
            _ => {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "JPEG writer: unsupported spp={}",
                    spp
                )))
            }
        };

        let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(
            std::fs::File::create(path).map_err(BioFormatsError::Io)?,
            self.quality,
        );
        img.write_with_encoder(encoder)
            .map_err(|e| BioFormatsError::Format(e.to_string()))?;
        self.wrote = true;
        Ok(())
    }

    fn can_do_stacks(&self) -> bool {
        false
    }
}
