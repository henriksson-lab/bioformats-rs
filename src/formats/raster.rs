//! Readers (and writers where possible) for additional raster formats via the `image` crate:
//! GIF, TGA, WebP, PNM, HDR/RGBE, OpenEXR, DDS, Farbfeld.
//!
//! All share the same generic implementation; the only difference is the extension/magic check.
//!
//! Animated GIFs are read as an image stack (one plane per frame), matching the
//! Java `GIFReader`; animated PNG (APNG) is still rejected.
//! Indexed/paletted inputs that the `image` crate can decode, including GIF and TGA,
//! are expanded to concrete samples and reported as non-indexed RGB/RGBA data.

use image::GenericImageView;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;
use crate::common::writer::FormatWriter;

// ---- generic helper ---------------------------------------------------------

#[derive(Clone, Copy)]
enum RasterBehavior {
    Still,
}

fn load_image(path: &Path, behavior: RasterBehavior) -> Result<(ImageMetadata, Vec<u8>)> {
    match behavior {
        RasterBehavior::Still => {}
    }

    let img = image::open(path).map_err(|e| BioFormatsError::Format(e.to_string()))?;
    let (w, h) = img.dimensions();

    let (pixel_type, spp, raw): (PixelType, u32, Vec<u8>) = match img {
        image::DynamicImage::ImageLuma8(b) => (PixelType::Uint8, 1, b.into_raw()),
        image::DynamicImage::ImageLumaA8(b) => (PixelType::Uint8, 2, b.into_raw()),
        image::DynamicImage::ImageRgb8(b) => (PixelType::Uint8, 3, b.into_raw()),
        image::DynamicImage::ImageRgba8(b) => (PixelType::Uint8, 4, b.into_raw()),
        image::DynamicImage::ImageLuma16(b) => {
            let raw: Vec<u8> = b.into_raw().iter().flat_map(|v| v.to_le_bytes()).collect();
            (PixelType::Uint16, 1, raw)
        }
        image::DynamicImage::ImageLumaA16(b) => {
            let raw: Vec<u8> = b.into_raw().iter().flat_map(|v| v.to_le_bytes()).collect();
            (PixelType::Uint16, 2, raw)
        }
        image::DynamicImage::ImageRgb16(b) => {
            let raw: Vec<u8> = b.into_raw().iter().flat_map(|v| v.to_le_bytes()).collect();
            (PixelType::Uint16, 3, raw)
        }
        image::DynamicImage::ImageRgba16(b) => {
            let raw: Vec<u8> = b.into_raw().iter().flat_map(|v| v.to_le_bytes()).collect();
            (PixelType::Uint16, 4, raw)
        }
        image::DynamicImage::ImageRgb32F(b) => {
            let raw: Vec<u8> = b.into_raw().iter().flat_map(|v| v.to_le_bytes()).collect();
            (PixelType::Float32, 3, raw)
        }
        image::DynamicImage::ImageRgba32F(b) => {
            let raw: Vec<u8> = b.into_raw().iter().flat_map(|v| v.to_le_bytes()).collect();
            (PixelType::Float32, 4, raw)
        }
        other => {
            let rgb = other.to_rgb8();
            (PixelType::Uint8, 3, rgb.into_raw())
        }
    };

    let bpp = (pixel_type.bytes_per_sample() as u8) * 8;
    let is_rgb = spp >= 3;
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

// ---- generic reader struct --------------------------------------------------

struct GenericReader {
    exts: &'static [&'static str],
    /// Returns true if the header matches.
    magic_fn: fn(&[u8]) -> bool,
    behavior: RasterBehavior,
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    pixels: Option<Vec<u8>>,
}

impl GenericReader {
    fn new(
        exts: &'static [&'static str],
        magic_fn: fn(&[u8]) -> bool,
        behavior: RasterBehavior,
    ) -> Self {
        GenericReader {
            exts,
            magic_fn,
            behavior,
            path: None,
            meta: None,
            pixels: None,
        }
    }
}

impl FormatReader for GenericReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        self.exts.iter().any(|&e| ext.as_deref() == Some(e))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        (self.magic_fn)(header)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let (meta, pixels) = load_image(path, self.behavior)?;
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
        self.meta
            .as_ref()
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, idx: u32) -> Result<Vec<u8>> {
        if idx != 0 {
            return Err(BioFormatsError::PlaneOutOfRange(idx));
        }
        self.pixels.clone().ok_or(BioFormatsError::NotInitialized)
    }

    fn open_bytes_region(&mut self, idx: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let full = self.open_bytes(idx)?;
        let meta = self.meta.as_ref().unwrap();
        crop_full_plane("raster", &full, meta, meta.size_c as usize, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, idx: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = ((meta.size_x - tw) / 2, (meta.size_y - th) / 2);
        self.open_bytes_region(idx, tx, ty, tw, th)
    }
}

// ---- public constructors for each format ------------------------------------

pub fn gif_reader() -> impl FormatReader {
    GifReader::new()
}

/// Multi-frame GIF reader.
///
/// Faithful to the Java `GIFReader`, which reads every frame of an (animated)
/// GIF as a separate plane, producing an image stack (`sizeT = imageCount`).
/// The `image` crate's `GifDecoder` composites each frame (applying disposal
/// and transparency, as the Java reader does), so frames are exposed as
/// interleaved 8-bit RGBA planes rather than indexed data.
pub struct GifReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    frames: Vec<Vec<u8>>,
}

impl GifReader {
    pub fn new() -> Self {
        GifReader {
            path: None,
            meta: None,
            frames: Vec::new(),
        }
    }
}

impl Default for GifReader {
    fn default() -> Self {
        Self::new()
    }
}

fn load_gif_frames(path: &Path) -> Result<(ImageMetadata, Vec<Vec<u8>>)> {
    use image::AnimationDecoder;

    let file = File::open(path)?;
    let decoder = image::codecs::gif::GifDecoder::new(BufReader::new(file))
        .map_err(|e| BioFormatsError::Format(e.to_string()))?;

    let mut width = 0u32;
    let mut height = 0u32;
    let mut frames: Vec<Vec<u8>> = Vec::new();

    for frame in decoder.into_frames() {
        let frame = frame.map_err(|e| BioFormatsError::Format(e.to_string()))?;
        let buffer = frame.into_buffer(); // RgbaImage, fully composited
        if width == 0 {
            width = buffer.width();
            height = buffer.height();
        }
        frames.push(buffer.into_raw());
    }

    if frames.is_empty() {
        return Err(BioFormatsError::InvalidData(
            "GIF contains no frames".into(),
        ));
    }

    let image_count = frames.len() as u32;
    let meta = ImageMetadata {
        size_x: width,
        size_y: height,
        size_z: 1,
        size_c: 4,
        size_t: image_count,
        pixel_type: PixelType::Uint8,
        bits_per_pixel: 8,
        image_count,
        // Java GIFReader uses XYCTZ (frames vary over T).
        dimension_order: DimensionOrder::XYCTZ,
        is_rgb: true,
        is_interleaved: true,
        is_indexed: false,
        is_little_endian: true,
        resolution_count: 1,
        ..Default::default()
    };
    Ok((meta, frames))
}

impl FormatReader for GifReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("gif"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, h: &[u8]) -> bool {
        h.starts_with(b"GIF87a") || h.starts_with(b"GIF89a")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let (meta, frames) = load_gif_frames(path)?;
        self.path = Some(path.to_path_buf());
        self.meta = Some(meta);
        self.frames = frames;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.frames.clear();
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
        self.meta
            .as_ref()
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, idx: u32) -> Result<Vec<u8>> {
        let frame = self
            .frames
            .get(idx as usize)
            .ok_or(BioFormatsError::PlaneOutOfRange(idx))?;
        Ok(frame.clone())
    }

    fn open_bytes_region(&mut self, idx: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let full = self.open_bytes(idx)?;
        let meta = self.meta.as_ref().unwrap();
        crop_full_plane("gif", &full, meta, meta.size_c as usize, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, idx: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = ((meta.size_x - tw) / 2, (meta.size_y - th) / 2);
        self.open_bytes_region(idx, tx, ty, tw, th)
    }
}

pub fn tga_reader() -> impl FormatReader {
    // TGA has no reliable magic; extension-only detection
    GenericReader::new(&["tga", "tpic"], |_| false, RasterBehavior::Still)
}

pub fn webp_reader() -> impl FormatReader {
    GenericReader::new(
        &["webp"],
        |h| h.len() >= 12 && &h[0..4] == b"RIFF" && &h[8..12] == b"WEBP",
        RasterBehavior::Still,
    )
}

pub fn pnm_reader() -> impl FormatReader {
    GenericReader::new(
        &["pbm", "pgm", "ppm", "pnm", "pfm"],
        |h| h.len() >= 2 && h[0] == b'P' && h[1] >= b'1' && h[1] <= b'7',
        RasterBehavior::Still,
    )
}

pub fn hdr_reader() -> impl FormatReader {
    // Radiance HDR: starts with "#?RADIANCE\n" or "#?RGBE\n"
    GenericReader::new(
        &["hdr", "rgbe"],
        |h| h.starts_with(b"#?RADIANCE") || h.starts_with(b"#?RGBE"),
        RasterBehavior::Still,
    )
}

pub fn exr_reader() -> impl FormatReader {
    // OpenEXR: magic 0x76 0x2f 0x31 0x01
    GenericReader::new(
        &["exr"],
        |h| h.starts_with(&[0x76, 0x2f, 0x31, 0x01]),
        RasterBehavior::Still,
    )
}

pub fn dds_reader() -> impl FormatReader {
    GenericReader::new(&["dds"], |h| h.starts_with(b"DDS "), RasterBehavior::Still)
}

pub fn farbfeld_reader() -> impl FormatReader {
    GenericReader::new(
        &["ff", "farbfeld"],
        |h| h.starts_with(b"farbfeld"),
        RasterBehavior::Still,
    )
}

// ---- TGA writer (via image crate) -------------------------------------------

pub struct TgaWriter {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    wrote: bool,
}

impl TgaWriter {
    pub fn new() -> Self {
        TgaWriter {
            path: None,
            meta: None,
            wrote: false,
        }
    }
}

impl Default for TgaWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatWriter for TgaWriter {
    fn is_this_type(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("tga"))
            .unwrap_or(false)
    }
    fn set_metadata(&mut self, meta: &ImageMetadata) -> Result<()> {
        let logical_c = if meta.is_rgb { 1 } else { meta.size_c.max(1) };
        let required_planes = meta
            .size_z
            .max(1)
            .checked_mul(logical_c)
            .and_then(|v| v.checked_mul(meta.size_t.max(1)))
            .ok_or_else(|| BioFormatsError::Format("TGA writer plane count overflow".into()))?;
        if required_planes > 1 || meta.image_count > 1 {
            return Err(BioFormatsError::UnsupportedFormat(
                "TGA writer supports only one plane".into(),
            ));
        }
        if meta.pixel_type != PixelType::Uint8 {
            return Err(BioFormatsError::UnsupportedFormat(
                "TGA writer only supports Uint8 data".into(),
            ));
        }
        if meta.size_c != 1 && !(meta.is_rgb && matches!(meta.size_c, 3 | 4)) {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "TGA writer supports grayscale or RGB/RGBA Uint8 data, got {} channels",
                meta.size_c
            )));
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
        self.path = None;
        self.meta = None;
        self.wrote = false;
        Ok(())
    }
    fn save_bytes(&mut self, idx: u32, data: &[u8]) -> Result<()> {
        if idx != 0 {
            return Err(BioFormatsError::Format("TGA: single plane only".into()));
        }
        if self.wrote {
            return Err(BioFormatsError::Format(
                "TGA writer supports only one plane".into(),
            ));
        }
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let expected = (meta.size_x as usize)
            .checked_mul(meta.size_y as usize)
            .and_then(|px| px.checked_mul(meta.size_c as usize))
            .ok_or_else(|| BioFormatsError::Format("TGA writer image plane is too large".into()))?;
        if data.len() != expected {
            return Err(BioFormatsError::InvalidData(format!(
                "TGA writer: plane 0 has {} bytes, expected {}",
                data.len(),
                expected
            )));
        }
        let (w, h) = (meta.size_x, meta.size_y);
        let spp = meta.size_c as usize;
        let img: image::DynamicImage = match spp {
            1 => image::GrayImage::from_raw(w, h, data.to_vec())
                .map(image::DynamicImage::ImageLuma8)
                .ok_or_else(|| BioFormatsError::InvalidData("bad length".into()))?,
            3 => image::RgbImage::from_raw(w, h, data.to_vec())
                .map(image::DynamicImage::ImageRgb8)
                .ok_or_else(|| BioFormatsError::InvalidData("bad length".into()))?,
            4 => image::RgbaImage::from_raw(w, h, data.to_vec())
                .map(image::DynamicImage::ImageRgba8)
                .ok_or_else(|| BioFormatsError::InvalidData("bad length".into()))?,
            _ => {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "TGA: spp={}",
                    spp
                )))
            }
        };
        img.save(path)
            .map_err(|e| BioFormatsError::Format(e.to_string()))?;
        self.wrote = true;
        Ok(())
    }
    fn can_do_stacks(&self) -> bool {
        false
    }
}
