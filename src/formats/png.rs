use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, LookupTable};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;
use std::fs;
use std::io::Read;
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
    if let Some(indexed) = load_indexed_png(path)? {
        return Ok(indexed);
    }

    use image::GenericImageView;
    let img = image::open(path).map_err(|e| BioFormatsError::Format(e.to_string()))?;
    let (w, h) = img.dimensions();

    let (pixel_type, is_rgb, spp, raw) = match img {
        image::DynamicImage::ImageLuma8(buf) => (PixelType::Uint8, false, 1u32, buf.into_raw()),
        image::DynamicImage::ImageLumaA8(buf) => (PixelType::Uint8, true, 2, buf.into_raw()),
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
        // APNGReader.java sets dimensionOrder "XYCTZ"; the core metadata
        // defaults to big-endian (littleEndian = false).
        dimension_order: DimensionOrder::XYCTZ,
        is_rgb,
        is_interleaved: true,
        is_indexed: false,
        is_little_endian: false,
        resolution_count: 1,
        ..Default::default()
    };
    Ok((meta, raw))
}

fn load_indexed_png(path: &Path) -> Result<Option<(ImageMetadata, Vec<u8>)>> {
    let bytes = fs::read(path)?;
    let Some(mut offset) = bytes
        .strip_prefix(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A])
        .map(|_| 8usize)
    else {
        return Ok(None);
    };

    let mut width = 0u32;
    let mut height = 0u32;
    let mut bit_depth = 0u8;
    let mut color_type = 0u8;
    let mut compression = 0u8;
    let mut filter = 0u8;
    let mut interlace = 0u8;
    let mut palette: Option<Vec<u8>> = None;
    let mut idat = Vec::new();

    while offset.checked_add(8).is_some_and(|end| end <= bytes.len()) {
        let length = u32::from_be_bytes([
            bytes[offset],
            bytes[offset + 1],
            bytes[offset + 2],
            bytes[offset + 3],
        ]) as usize;
        let chunk_type = &bytes[offset + 4..offset + 8];
        let data_start = offset + 8;
        let Some(data_end) = data_start.checked_add(length) else {
            return Ok(None);
        };
        if data_end > bytes.len() {
            return Ok(None);
        }

        match chunk_type {
            b"IHDR" if length >= 13 => {
                width = u32::from_be_bytes([
                    bytes[data_start],
                    bytes[data_start + 1],
                    bytes[data_start + 2],
                    bytes[data_start + 3],
                ]);
                height = u32::from_be_bytes([
                    bytes[data_start + 4],
                    bytes[data_start + 5],
                    bytes[data_start + 6],
                    bytes[data_start + 7],
                ]);
                bit_depth = bytes[data_start + 8];
                color_type = bytes[data_start + 9];
                compression = bytes[data_start + 10];
                filter = bytes[data_start + 11];
                interlace = bytes[data_start + 12];
            }
            b"PLTE" => palette = Some(bytes[data_start..data_end].to_vec()),
            b"IDAT" => idat.extend_from_slice(&bytes[data_start..data_end]),
            b"IEND" => break,
            _ => {}
        }

        let Some(next_offset) = data_end.checked_add(4) else {
            return Ok(None);
        };
        offset = next_offset;
    }

    if color_type != 3 {
        return Ok(None);
    }
    if width == 0 || height == 0 || compression != 0 || filter != 0 || interlace != 0 {
        return Ok(None);
    }
    if !matches!(bit_depth, 1 | 2 | 4 | 8) {
        return Ok(None);
    }

    let palette = match palette {
        Some(palette) if palette.len() >= 3 => palette,
        _ => return Ok(None),
    };

    let row_bits = width as usize * bit_depth as usize;
    let row_bytes = row_bits.div_ceil(8);
    let expected = (row_bytes + 1)
        .checked_mul(height as usize)
        .ok_or_else(|| BioFormatsError::InvalidData("PNG indexed payload overflows".into()))?;

    let mut inflated = Vec::new();
    flate2::read::ZlibDecoder::new(idat.as_slice())
        .read_to_end(&mut inflated)
        .map_err(BioFormatsError::Io)?;
    if inflated.len() < expected {
        return Err(BioFormatsError::InvalidData(format!(
            "PNG indexed payload ended after {} bytes, expected at least {}",
            inflated.len(),
            expected
        )));
    }

    let mut unfiltered = vec![0u8; row_bytes * height as usize];
    let mut src = 0usize;
    for row in 0..height as usize {
        let filter_type = inflated[src];
        src += 1;
        let row_start = row * row_bytes;
        let prev_start = row.checked_sub(1).map(|prev| prev * row_bytes);
        for col in 0..row_bytes {
            let raw = inflated[src + col];
            let left = if col > 0 {
                unfiltered[row_start + col - 1]
            } else {
                0
            };
            let up = prev_start.map(|base| unfiltered[base + col]).unwrap_or(0);
            let up_left = if col > 0 {
                prev_start
                    .map(|base| unfiltered[base + col - 1])
                    .unwrap_or(0)
            } else {
                0
            };
            unfiltered[row_start + col] = match filter_type {
                0 => raw,
                1 => raw.wrapping_add(left),
                2 => raw.wrapping_add(up),
                3 => raw.wrapping_add(((left as u16 + up as u16) / 2) as u8),
                4 => raw.wrapping_add(paeth_predictor(left, up, up_left)),
                _ => {
                    return Err(BioFormatsError::InvalidData(format!(
                        "PNG invalid filter type {filter_type}"
                    )));
                }
            };
        }
        src += row_bytes;
    }

    let mut pixels = vec![0u8; width as usize * height as usize];
    for row in 0..height as usize {
        let row_data = &unfiltered[row * row_bytes..(row + 1) * row_bytes];
        for col in 0..width as usize {
            pixels[row * width as usize + col] = if bit_depth == 8 {
                row_data[col]
            } else {
                let bit = col * bit_depth as usize;
                let byte = row_data[bit / 8];
                let shift = 8 - bit_depth as usize - (bit % 8);
                (byte >> shift) & ((1u16 << bit_depth) - 1) as u8
            };
        }
    }

    let mut red = vec![0u16; 256];
    let mut green = vec![0u16; 256];
    let mut blue = vec![0u16; 256];
    for (i, rgb) in palette.chunks_exact(3).take(256).enumerate() {
        red[i] = rgb[0] as u16;
        green[i] = rgb[1] as u16;
        blue[i] = rgb[2] as u16;
    }

    let meta = ImageMetadata {
        size_x: width,
        size_y: height,
        size_z: 1,
        size_c: 1,
        size_t: 1,
        pixel_type: PixelType::Uint8,
        bits_per_pixel: bit_depth,
        image_count: 1,
        dimension_order: DimensionOrder::XYCTZ,
        is_rgb: false,
        is_interleaved: false,
        is_indexed: true,
        is_little_endian: false,
        resolution_count: 1,
        lookup_table: Some(LookupTable { red, green, blue }),
        ..Default::default()
    };

    Ok(Some((meta, pixels)))
}

fn paeth_predictor(left: u8, up: u8, up_left: u8) -> u8 {
    let a = left as i32;
    let b = up as i32;
    let c = up_left as i32;
    let p = a + b - c;
    let pa = (p - a).abs();
    let pb = (p - b).abs();
    let pc = (p - c).abs();
    if pa <= pb && pa <= pc {
        left
    } else if pb <= pc {
        up
    } else {
        up_left
    }
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
        self.close()?;
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

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        use crate::common::ome_metadata::OmeMetadata;
        let meta = self.meta.as_ref()?;
        let mut ome = OmeMetadata::from_image_metadata(meta);
        // MetadataTools.populatePixels sets the image name to the file's basename.
        if let (Some(path), Some(img)) = (self.path.as_ref(), ome.images.get_mut(0)) {
            img.name = path
                .file_name()
                .and_then(|n| n.to_str())
                .map(|s| s.to_string());
        }
        Some(ome)
    }
}

use crate::common::writer::FormatWriter;

pub struct PngWriter {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    wrote: bool,
}

impl PngWriter {
    pub fn new() -> Self {
        PngWriter {
            path: None,
            meta: None,
            wrote: false,
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
        let logical_c = if meta.is_rgb { 1 } else { meta.size_c.max(1) };
        let required_planes = meta
            .size_z
            .max(1)
            .checked_mul(logical_c)
            .and_then(|v| v.checked_mul(meta.size_t.max(1)))
            .ok_or_else(|| BioFormatsError::Format("PNG writer plane count overflow".into()))?;
        if required_planes > 1 || meta.image_count > 1 {
            return Err(BioFormatsError::UnsupportedFormat(
                "PNG writer supports only one plane".into(),
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
                "PNG writer closed before plane 0 was written".into(),
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
                "PNG writer supports only one plane".into(),
            ));
        }
        if self.wrote {
            return Err(BioFormatsError::Format(
                "PNG writer already wrote plane 0".into(),
            ));
        }
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;

        let (w, h) = (meta.size_x, meta.size_y);
        let spp = meta.size_c as usize;
        let expected_len = (meta.size_x as usize)
            .checked_mul(meta.size_y as usize)
            .and_then(|px| px.checked_mul(spp))
            .and_then(|samples| samples.checked_mul(meta.pixel_type.bytes_per_sample()))
            .ok_or_else(|| BioFormatsError::Format("PNG writer image plane is too large".into()))?;
        if data.len() != expected_len {
            return Err(BioFormatsError::InvalidData(format!(
                "PNG writer: plane 0 has {} bytes, expected {}",
                data.len(),
                expected_len
            )));
        }

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
            .map_err(|e| BioFormatsError::Format(e.to_string()))?;
        self.wrote = true;
        Ok(())
    }

    fn can_do_stacks(&self) -> bool {
        false
    }
}
