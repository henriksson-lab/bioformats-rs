//! SimFCS FLIM binary format reader.
//!
//! SimFCS stores raw binary FLIM data with no file header.
//! The file extension indicates the data type.
//!
//! Also includes LambertFlimReader for Lambert Instruments FLIM .asc files.

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;

// ── SimFCS Reader ─────────────────────────────────────────────────────────────

pub struct SimfcsReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl SimfcsReader {
    pub fn new() -> Self {
        SimfcsReader {
            path: None,
            meta: None,
        }
    }
}

impl Default for SimfcsReader {
    fn default() -> Self {
        Self::new()
    }
}

fn simfcs_pixel_type(ext: &str) -> Option<PixelType> {
    match ext {
        "b64" => Some(PixelType::Uint8),
        "r64" => Some(PixelType::Float32),
        "i64" => Some(PixelType::Int32),
        _ => None,
    }
}

impl FormatReader for SimfcsReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("b64") | Some("r64") | Some("i64"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .unwrap_or_default();
        let pixel_type = simfcs_pixel_type(&ext)
            .ok_or_else(|| BioFormatsError::Format(format!("Unknown SimFCS extension: {}", ext)))?;

        let bps = pixel_type.bytes_per_sample();
        let file_size = fs::metadata(path).map_err(BioFormatsError::Io)?.len() as usize;
        let frame_bytes = 256 * 256 * bps;
        if file_size == 0 || file_size % frame_bytes != 0 {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "SimFCS payload length {file_size} is not a whole number of 256x256 frames"
            )));
        }
        let image_count = (file_size / frame_bytes) as u32;

        let meta = ImageMetadata {
            size_x: 256,
            size_y: 256,
            size_z: image_count,
            size_c: 1,
            size_t: 1,
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
        };

        self.path = Some(path.to_path_buf());
        self.meta = Some(meta);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        Ok(())
    }

    fn series_count(&self) -> usize {
        usize::from(self.meta.is_some())
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.meta.is_none() {
            Err(BioFormatsError::NotInitialized)
        } else if s == 0 {
            Ok(())
        } else {
            Err(BioFormatsError::SeriesOutOfRange(s))
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
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let bps = meta.pixel_type.bytes_per_sample();
        let plane_bytes = 256 * 256 * bps;
        let offset = plane_index as u64 * plane_bytes as u64;

        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = fs::File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(offset))
            .map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; plane_bytes];
        f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
        Ok(buf)
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
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        crop_full_plane("SimFCS", &full, meta, 1, x, y, w, h)
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

// ── Lambert FLIM Reader ───────────────────────────────────────────────────────

pub struct LambertFlimReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    pixels: Vec<u8>,
    plane_len: usize,
}

impl LambertFlimReader {
    pub fn new() -> Self {
        LambertFlimReader {
            path: None,
            meta: None,
            pixels: Vec::new(),
            plane_len: 0,
        }
    }

    fn unsupported() -> BioFormatsError {
        BioFormatsError::UnsupportedFormat(
            "Lambert FLIM native/fallback decoding is not supported; only BFLAMBERT_ASCII_V1 blind ASCII is accepted".to_string(),
        )
    }

    fn parse_blind_ascii(path: &Path) -> Result<(ImageMetadata, Vec<u8>, usize)> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(Self::unsupported());
            }
            Err(err) => return Err(BioFormatsError::Io(err)),
        };
        if !text.lines().any(|line| line.trim() == "BFLAMBERT_ASCII_V1") {
            return Err(Self::unsupported());
        }

        let mut width = None;
        let mut height = None;
        let mut frames = Some(1u32);
        let mut pixel_type = None;
        let mut data_hex = String::new();
        let mut in_data = false;

        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if in_data {
                data_hex.push_str(line);
                continue;
            }
            if line == "DATA_HEX" {
                in_data = true;
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            match key.trim() {
                "Width" | "SizeX" => {
                    width = Some(parse_lambert_positive_u32(value.trim(), "Width")?)
                }
                "Height" | "SizeY" => {
                    height = Some(parse_lambert_positive_u32(value.trim(), "Height")?)
                }
                "Frames" | "FrameCount" | "Planes" => {
                    frames = Some(parse_lambert_positive_u32(value.trim(), "Frames")?)
                }
                "PixelType" | "Type" => {
                    pixel_type = Some(parse_lambert_pixel_type(value.trim())?);
                }
                "DataHex" => data_hex.push_str(value.trim()),
                _ => {}
            }
        }

        let width =
            width.ok_or_else(|| BioFormatsError::Format("Lambert FLIM missing Width".into()))?;
        let height =
            height.ok_or_else(|| BioFormatsError::Format("Lambert FLIM missing Height".into()))?;
        let frames = frames.unwrap_or(1);
        let pixel_type = pixel_type
            .ok_or_else(|| BioFormatsError::Format("Lambert FLIM missing PixelType".into()))?;
        let plane_len = (width as usize)
            .checked_mul(height as usize)
            .and_then(|px| px.checked_mul(pixel_type.bytes_per_sample()))
            .ok_or_else(|| BioFormatsError::Format("Lambert FLIM plane size overflows".into()))?;
        let expected_len = plane_len
            .checked_mul(frames as usize)
            .ok_or_else(|| BioFormatsError::Format("Lambert FLIM payload size overflows".into()))?;
        let pixels = decode_lambert_hex(&data_hex)?;
        if pixels.len() != expected_len {
            return Err(BioFormatsError::Format(format!(
                "Lambert FLIM payload length {} does not match declared size {expected_len}",
                pixels.len()
            )));
        }

        Ok((
            ImageMetadata {
                size_x: width,
                size_y: height,
                size_z: 1,
                size_c: 1,
                size_t: frames,
                pixel_type,
                bits_per_pixel: (pixel_type.bytes_per_sample() * 8) as u8,
                image_count: frames,
                dimension_order: DimensionOrder::XYCZT,
                is_little_endian: true,
                ..ImageMetadata::default()
            },
            pixels,
            plane_len,
        ))
    }
}

fn parse_lambert_pixel_type(value: &str) -> Result<PixelType> {
    match value.to_ascii_lowercase().as_str() {
        "uint8" | "u8" | "byte" => Ok(PixelType::Uint8),
        "uint16" | "u16" | "uint16le" | "ushort" => Ok(PixelType::Uint16),
        "float32" | "f32" | "single" => Ok(PixelType::Float32),
        other => Err(BioFormatsError::Format(format!(
            "Lambert FLIM blind subset pixel type {other} is not supported"
        ))),
    }
}

fn parse_lambert_positive_u32(value: &str, field: &str) -> Result<u32> {
    let parsed = value
        .parse::<u32>()
        .map_err(|_| BioFormatsError::Format(format!("Lambert FLIM invalid {field}")))?;
    if parsed == 0 {
        Err(BioFormatsError::Format(format!(
            "Lambert FLIM {field} must be positive"
        )))
    } else {
        Ok(parsed)
    }
}

fn decode_lambert_hex(data: &str) -> Result<Vec<u8>> {
    let compact: String = data.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    if compact.len() % 2 != 0 {
        return Err(BioFormatsError::Format(
            "Lambert FLIM hex payload has odd length".into(),
        ));
    }
    let mut out = Vec::with_capacity(compact.len() / 2);
    for chunk in compact.as_bytes().chunks_exact(2) {
        let s = std::str::from_utf8(chunk).unwrap();
        out.push(u8::from_str_radix(s, 16).map_err(|_| {
            BioFormatsError::Format("Lambert FLIM hex payload contains non-hex bytes".into())
        })?);
    }
    Ok(out)
}

impl Default for LambertFlimReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for LambertFlimReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("asc"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // Check for Lambert Instruments ASCII heuristic
        if header.len() < 8 {
            return false;
        }
        let s = std::str::from_utf8(&header[..header.len().min(256)]).unwrap_or("");
        s.contains("Lambert") || s.contains("GlobalImages")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.pixels.clear();
        self.plane_len = 0;
        let (meta, pixels, plane_len) = Self::parse_blind_ascii(path)?;
        self.path = Some(path.to_path_buf());
        self.meta = Some(meta);
        self.pixels = pixels;
        self.plane_len = plane_len;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.pixels.clear();
        self.plane_len = 0;
        Ok(())
    }

    fn series_count(&self) -> usize {
        usize::from(self.meta.is_some())
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.meta.is_none() {
            Err(BioFormatsError::NotInitialized)
        } else if s == 0 {
            Ok(())
        } else {
            Err(BioFormatsError::SeriesOutOfRange(s))
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
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let start = self
            .plane_len
            .checked_mul(plane_index as usize)
            .ok_or_else(|| BioFormatsError::Format("Lambert FLIM plane offset overflows".into()))?;
        let end = start
            .checked_add(self.plane_len)
            .ok_or_else(|| BioFormatsError::Format("Lambert FLIM plane end overflows".into()))?;
        Ok(self.pixels[start..end].to_vec())
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
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        crop_full_plane("Lambert FLIM", &full, meta, 1, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        self.open_bytes(plane_index)
    }
}
