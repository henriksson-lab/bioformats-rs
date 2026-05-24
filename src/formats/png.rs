use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;
use std::fs;
use std::path::{Path, PathBuf};

pub struct PngReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    pixels: Option<Vec<u8>>,
}

impl PngReader {
    pub fn new() -> Self {
        PngReader {
            path: None,
            meta: None,
            pixels: None,
        }
    }
}

impl Default for PngReader {
    fn default() -> Self {
        Self::new()
    }
}

fn load_png(path: &Path) -> Result<(ImageMetadata, Vec<u8>)> {
    use image::GenericImageView;
    let img = image::open(path).map_err(|e| BioFormatsError::Format(e.to_string()))?;
    let (w, h) = img.dimensions();

    let (pixel_type, is_rgb, spp, raw) = match img {
        image::DynamicImage::ImageLuma8(buf) => (PixelType::Uint8, false, 1u32, buf.into_raw()),
        image::DynamicImage::ImageLumaA8(buf) => (PixelType::Uint8, false, 2, buf.into_raw()),
        image::DynamicImage::ImageRgb8(buf) => (PixelType::Uint8, true, 3, buf.into_raw()),
        image::DynamicImage::ImageRgba8(buf) => (PixelType::Uint8, true, 4, buf.into_raw()),
        image::DynamicImage::ImageLuma16(buf) => {
            let raw: Vec<u8> = buf
                .into_raw()
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect();
            (PixelType::Uint16, false, 1, raw)
        }
        image::DynamicImage::ImageRgb16(buf) => {
            let raw: Vec<u8> = buf
                .into_raw()
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect();
            (PixelType::Uint16, true, 3, raw)
        }
        image::DynamicImage::ImageRgba16(buf) => {
            let raw: Vec<u8> = buf
                .into_raw()
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect();
            (PixelType::Uint16, true, 4, raw)
        }
        other => {
            let rgb8 = other.to_rgb8();
            (PixelType::Uint8, true, 3, rgb8.into_raw())
        }
    };

    let bpp = pixel_type.bytes_per_sample() as u8 * 8;
    let meta = ImageMetadata {
        size_x: w,
        size_y: h,
        size_z: 1,
        size_c: spp,
        size_t: 1,
        pixel_type,
        bits_per_pixel: bpp,
        image_count: 1,
        dimension_order: DimensionOrder::XYCZT,
        is_rgb,
        is_interleaved: true,
        is_indexed: false,
        is_little_endian: true,
        resolution_count: 1,
        ..Default::default()
    };
    Ok((meta, raw))
}

fn contains_apng_animation_control(path: &Path) -> Result<bool> {
    let bytes = fs::read(path)?;
    let Some(mut offset) = bytes
        .strip_prefix(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A])
        .map(|_| 8usize)
    else {
        return Ok(false);
    };

    while offset.checked_add(8).is_some_and(|end| end <= bytes.len()) {
        let length = u32::from_be_bytes([
            bytes[offset],
            bytes[offset + 1],
            bytes[offset + 2],
            bytes[offset + 3],
        ]) as usize;
        let chunk_type = &bytes[offset + 4..offset + 8];
        if chunk_type == b"acTL" {
            return Ok(true);
        }
        if chunk_type == b"IDAT" || chunk_type == b"IEND" {
            return Ok(false);
        }
        offset = offset
            .checked_add(12)
            .and_then(|v| v.checked_add(length))
            .ok_or_else(|| BioFormatsError::InvalidData("PNG chunk offset overflows".into()))?;
    }

    Ok(false)
}

impl FormatReader for PngReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("png"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A])
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        if contains_apng_animation_control(path)? {
            return Err(BioFormatsError::UnsupportedFormat(
                "animated PNG is not supported; use a still PNG image".into(),
            ));
        }
        let (meta, pixels) = load_png(path)?;
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
        crop_full_plane("PNG", &full, meta, meta.size_c as usize, x, y, w, h)
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

pub struct PngWriter {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl PngWriter {
    pub fn new() -> Self {
        PngWriter {
            path: None,
            meta: None,
        }
    }
}

impl Default for PngWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatWriter for PngWriter {
    fn is_this_type(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("png"))
            .unwrap_or(false)
    }

    fn set_metadata(&mut self, meta: &ImageMetadata) -> Result<()> {
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
                "PNG writer supports only one plane".into(),
            ));
        }
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;

        let (w, h) = (meta.size_x, meta.size_y);
        let spp = meta.size_c as usize;

        let img: image::DynamicImage = match (meta.pixel_type, spp) {
            (PixelType::Uint8, 1) => image::GrayImage::from_raw(w, h, data.to_vec())
                .map(image::DynamicImage::ImageLuma8)
                .ok_or_else(|| BioFormatsError::InvalidData("bad data length".into()))?,
            (PixelType::Uint8, 3) => image::RgbImage::from_raw(w, h, data.to_vec())
                .map(image::DynamicImage::ImageRgb8)
                .ok_or_else(|| BioFormatsError::InvalidData("bad data length".into()))?,
            (PixelType::Uint8, 4) => image::RgbaImage::from_raw(w, h, data.to_vec())
                .map(image::DynamicImage::ImageRgba8)
                .ok_or_else(|| BioFormatsError::InvalidData("bad data length".into()))?,
            (PixelType::Uint16, 1) => {
                let pixels: Vec<u16> = data
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect();
                image::ImageBuffer::<image::Luma<u16>, _>::from_raw(w, h, pixels)
                    .map(image::DynamicImage::ImageLuma16)
                    .ok_or_else(|| BioFormatsError::InvalidData("bad data length".into()))?
            }
            (PixelType::Uint16, 3) => {
                let pixels: Vec<u16> = data
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect();
                image::ImageBuffer::<image::Rgb<u16>, _>::from_raw(w, h, pixels)
                    .map(image::DynamicImage::ImageRgb16)
                    .ok_or_else(|| BioFormatsError::InvalidData("bad data length".into()))?
            }
            _ => {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "PNG writer: unsupported {:?} spp={}",
                    meta.pixel_type, spp
                )));
            }
        };

        img.save(path)
            .map_err(|e| BioFormatsError::Format(e.to_string()))
    }

    fn can_do_stacks(&self) -> bool {
        false
    }
}
