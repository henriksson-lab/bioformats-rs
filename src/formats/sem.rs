//! Scanning Electron Microscopy (SEM) and related format readers.
//!
//! Includes real binary readers for INR and Veeco/Nanoscope formats,
//! a TIFF wrapper for Zeiss, and extension-only placeholders.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;

// ---------------------------------------------------------------------------
// Macro: thin TIFF wrapper
// ---------------------------------------------------------------------------
macro_rules! tiff_wrapper {
    (
        $(#[$attr:meta])*
        pub struct $name:ident;
        extensions: [$($ext:literal),+];
    ) => {
        $(#[$attr])*
        pub struct $name {
            inner: crate::tiff::TiffReader,
        }

        impl $name {
            pub fn new() -> Self {
                $name { inner: crate::tiff::TiffReader::new() }
            }
        }

        impl Default for $name {
            fn default() -> Self { Self::new() }
        }

        impl FormatReader for $name {
            fn is_this_type_by_name(&self, path: &Path) -> bool {
                let ext = path.extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.to_ascii_lowercase());
                matches!(ext.as_deref(), $(Some($ext))|+)
            }

            fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool { false }

            fn set_id(&mut self, path: &Path) -> Result<()> {
                self.inner.set_id(path)
            }

            fn close(&mut self) -> Result<()> {
                self.inner.close()
            }

            fn series_count(&self) -> usize {
                self.inner.series_count()
            }

            fn set_series(&mut self, s: usize) -> Result<()> {
                self.inner.set_series(s)
            }

            fn series(&self) -> usize {
                self.inner.series()
            }

            fn metadata(&self) -> &ImageMetadata {
                self.inner.metadata()
            }

            fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
                self.inner.open_bytes(p)
            }

            fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
                self.inner.open_bytes_region(p, x, y, w, h)
            }

            fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
                self.inner.open_thumb_bytes(p)
            }

            fn resolution_count(&self) -> usize {
                self.inner.resolution_count()
            }

            fn set_resolution(&mut self, level: usize) -> Result<()> {
                self.inner.set_resolution(level)
            }
        }
    };
}

// ---------------------------------------------------------------------------
// Macro: extension-only placeholder reader
// ---------------------------------------------------------------------------
macro_rules! placeholder_reader {
    (
        $(#[$attr:meta])*
        pub struct $name:ident;
        extensions: [$($ext:literal),+];
    ) => {
        $(#[$attr])*
        pub struct $name {
            path: Option<PathBuf>,
            meta: Option<ImageMetadata>,
        }

        impl $name {
            pub fn new() -> Self {
                $name { path: None, meta: None }
            }
        }

        impl Default for $name {
            fn default() -> Self { Self::new() }
        }

        impl FormatReader for $name {
            fn is_this_type_by_name(&self, path: &Path) -> bool {
                let ext = path.extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.to_ascii_lowercase());
                matches!(ext.as_deref(), $(Some($ext))|+)
            }

            fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool { false }

            fn set_id(&mut self, _path: &Path) -> Result<()> {
                Err(BioFormatsError::UnsupportedFormat(
                    concat!(stringify!($name), " format reading is not yet implemented").to_string()
                ))
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

            fn open_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
                Err(BioFormatsError::UnsupportedFormat(
                    concat!(stringify!($name), " format reading is not yet implemented").to_string()
                ))
            }

            fn open_bytes_region(&mut self, _plane_index: u32, _x: u32, _y: u32, _w: u32, _h: u32) -> Result<Vec<u8>> {
                Err(BioFormatsError::UnsupportedFormat(
                    concat!(stringify!($name), " format reading is not yet implemented").to_string()
                ))
            }

            fn open_thumb_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
                Err(BioFormatsError::UnsupportedFormat(
                    concat!(stringify!($name), " format reading is not yet implemented").to_string()
                ))
            }

            fn resolution_count(&self) -> usize { 1 }

            fn set_resolution(&mut self, level: usize) -> Result<()> {
                if level != 0 {
                    Err(BioFormatsError::Format(format!("resolution {} out of range", level)))
                } else {
                    Ok(())
                }
            }
        }
    };
}

// ===========================================================================
// Real binary reader 1 — INR format
// ===========================================================================

/// Pixel type classification used during INR header parsing.
#[derive(Debug, Clone, Copy, PartialEq)]
enum InrType {
    Uint,
    Int,
    Float,
}

/// INRIMAGE-4 volumetric format (`.inr`).
///
/// Header is 256 ASCII bytes with `#INRIMAGE-4#{` magic, followed by raw
/// pixel data. Key=value pairs in the header define dimensions and pixel type.
pub struct InrReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl InrReader {
    pub fn new() -> Self {
        InrReader {
            path: None,
            meta: None,
        }
    }
}

impl Default for InrReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for InrReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("inr"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.len() >= 13 && &header[0..13] == b"#INRIMAGE-4#{"
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;

        // Header is first 256 bytes interpreted as ASCII text
        if data.len() < 256 || !data.starts_with(b"#INRIMAGE-4#{") {
            return Err(BioFormatsError::UnsupportedFormat(
                "INR file is missing the 256-byte INRIMAGE-4 header".into(),
            ));
        }
        let header_bytes = &data[..256];
        let header_text = String::from_utf8_lossy(header_bytes);

        let mut size_x: Option<u32> = None;
        let mut size_y: Option<u32> = None;
        let mut size_z: u32 = 1;
        let mut size_c: u32 = 1;
        let mut bpp: Option<u32> = None;
        let mut inr_type: Option<InrType> = None;
        let mut little_endian = true;

        for line in header_text.split('\n') {
            let line = line.trim();
            if let Some(pos) = line.find('=') {
                let key = line[..pos].trim();
                let val = line[pos + 1..].trim();
                match key {
                    "XDIM" => {
                        if let Ok(n) = val.parse::<u32>() {
                            size_x = Some(n);
                        }
                    }
                    "YDIM" => {
                        if let Ok(n) = val.parse::<u32>() {
                            size_y = Some(n);
                        }
                    }
                    "ZDIM" => {
                        if let Ok(n) = val.parse::<u32>() {
                            size_z = n;
                        }
                    }
                    "VDIM" => {
                        if let Ok(n) = val.parse::<u32>() {
                            size_c = n;
                        }
                    }
                    "PIXSIZE" => {
                        // Format: "N bits"
                        if let Some(n_str) = val.split_whitespace().next() {
                            if let Ok(n) = n_str.parse::<u32>() {
                                bpp = Some(n);
                            }
                        }
                    }
                    "TYPE" => {
                        let mut parsed_type = if val.contains("unsigned")
                            || val.contains("fixed") && !val.contains("signed")
                        {
                            InrType::Uint
                        } else if val.contains("signed") {
                            InrType::Int
                        } else if val.contains("float") {
                            InrType::Float
                        } else {
                            InrType::Uint
                        };
                        // More precise: check exact values
                        if val == "unsigned fixed" {
                            parsed_type = InrType::Uint;
                        } else if val == "signed fixed" {
                            parsed_type = InrType::Int;
                        } else if val == "float" {
                            parsed_type = InrType::Float;
                        }
                        inr_type = Some(parsed_type);
                    }
                    "CPU" => {
                        little_endian = matches!(val, "decm" | "pc");
                        if val == "sun" || val == "sgi" {
                            little_endian = false;
                        }
                    }
                    _ => {}
                }
            }
        }

        let size_x = size_x
            .filter(|&v| v > 0)
            .ok_or_else(|| BioFormatsError::UnsupportedFormat("INR header missing XDIM".into()))?;
        let size_y = size_y
            .filter(|&v| v > 0)
            .ok_or_else(|| BioFormatsError::UnsupportedFormat("INR header missing YDIM".into()))?;
        let bpp = bpp.ok_or_else(|| {
            BioFormatsError::UnsupportedFormat("INR header missing PIXSIZE".into())
        })?;
        let inr_type = inr_type
            .ok_or_else(|| BioFormatsError::UnsupportedFormat("INR header missing TYPE".into()))?;
        let pixel_type = match (bpp, inr_type) {
            (8, InrType::Uint) => PixelType::Uint8,
            (8, InrType::Int) => PixelType::Uint8,
            (16, InrType::Uint) => PixelType::Uint16,
            (16, InrType::Int) => PixelType::Int16,
            (32, InrType::Uint) => PixelType::Uint32,
            (32, InrType::Int) => PixelType::Int32,
            (32, InrType::Float) => PixelType::Float32,
            (64, InrType::Float) => PixelType::Float64,
            _ => {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "INR unsupported pixel size/type combination: {bpp} bits"
                )));
            }
        };

        let image_count = size_z * size_c;
        let bps = (bpp / 8) as u64;
        let expected = 256u64
            .checked_add(
                (size_x as u64)
                    .checked_mul(size_y as u64)
                    .and_then(|v| v.checked_mul(image_count as u64))
                    .and_then(|v| v.checked_mul(bps))
                    .ok_or_else(|| BioFormatsError::Format("INR image size overflows".into()))?,
            )
            .ok_or_else(|| BioFormatsError::Format("INR image size overflows".into()))?;
        if (data.len() as u64) < expected {
            return Err(BioFormatsError::UnsupportedFormat(
                "INR pixel payload is shorter than declared dimensions".into(),
            ));
        }

        self.path = Some(path.to_path_buf());
        self.meta = Some(ImageMetadata {
            size_x,
            size_y,
            size_z,
            size_c,
            size_t: 1,
            pixel_type,
            bits_per_pixel: bpp as u8,
            image_count,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: little_endian,
            resolution_count: 1,
            series_metadata: HashMap::new(),
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        });
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
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
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let bps = (meta.bits_per_pixel / 8) as usize;
        let plane_bytes = meta.size_x as usize * meta.size_y as usize * bps;
        let offset = 256u64 + (plane_index as u64) * (plane_bytes as u64);

        let path = self.path.clone().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = std::fs::File::open(&path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(offset))
            .map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; plane_bytes];
        f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
        Ok(buf)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        _x: u32,
        _y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let meta = self
            .meta
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?
            .clone();
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        // Read full plane then crop (simple approach)
        let full = self.open_bytes(plane_index)?;
        crop_full_plane("INR", &full, &meta, 1, _x, _y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn resolution_count(&self) -> usize {
        1
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if level != 0 {
            Err(BioFormatsError::Format(format!(
                "resolution {} out of range",
                level
            )))
        } else {
            Ok(())
        }
    }
}

// ===========================================================================
// Real binary reader 2 — FEI/Philips XL SEM
// ===========================================================================

const FEI_PHILIPS_MAGIC: &[u8; 2] = b"XL";
const FEI_INVALID_PIXELS: u32 = 112;
const FEI_DIMENSION_OFFSET: u64 = 514;

/// FEI/Philips XL `.img` SEM files.
///
/// Ported from Bio-Formats `FEIReader`: the header stores the physical scan
/// parameters at fixed offsets, width/height at offset 514, and pixels as an
/// 8-bit grayscale plane split into four row passes and two column passes.
pub struct FeiPhilipsReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    header_size: u64,
}

impl FeiPhilipsReader {
    pub fn new() -> Self {
        FeiPhilipsReader {
            path: None,
            meta: None,
            header_size: 0,
        }
    }
}

impl Default for FeiPhilipsReader {
    fn default() -> Self {
        Self::new()
    }
}

fn read_le_u16_at(data: &[u8], offset: usize, label: &str) -> Result<u16> {
    let bytes = data.get(offset..offset + 2).ok_or_else(|| {
        BioFormatsError::UnsupportedFormat(format!("FEI/Philips header missing {label}"))
    })?;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn read_le_f32_at(data: &[u8], offset: usize) -> Option<f32> {
    let bytes = data.get(offset..offset + 4)?;
    Some(f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

impl FormatReader for FeiPhilipsReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("img"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.starts_with(FEI_PHILIPS_MAGIC)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        if !self.is_this_type_by_bytes(&data) {
            return Err(BioFormatsError::UnsupportedFormat(
                "FEI/Philips IMG header does not start with XL".into(),
            ));
        }

        let stored_width = read_le_u16_at(&data, 514, "width")? as u32;
        let height = read_le_u16_at(&data, 516, "height")? as u32;
        let header_size = read_le_u16_at(&data, 522, "pixel offset")? as u64;
        if stored_width <= FEI_INVALID_PIXELS || height == 0 || header_size < FEI_DIMENSION_OFFSET {
            return Err(BioFormatsError::UnsupportedFormat(
                "FEI/Philips IMG header contains invalid dimensions or pixel offset".into(),
            ));
        }

        let width = stored_width - FEI_INVALID_PIXELS;
        if width % 2 != 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "FEI/Philips IMG width must be even for interlaced decode".into(),
            ));
        }

        let encoded_row_bytes = (width / 2 + FEI_INVALID_PIXELS / 2) as u64 * 2;
        let encoded_bytes = encoded_row_bytes
            .checked_mul(height as u64)
            .ok_or_else(|| BioFormatsError::Format("FEI/Philips payload size overflows".into()))?;
        let expected = header_size
            .checked_add(encoded_bytes)
            .ok_or_else(|| BioFormatsError::Format("FEI/Philips file size overflows".into()))?;
        if (data.len() as u64) < expected {
            return Err(BioFormatsError::UnsupportedFormat(
                "FEI/Philips IMG payload is shorter than declared dimensions".into(),
            ));
        }

        let mut series_metadata = HashMap::new();
        if let Some(v) = read_le_f32_at(&data, 44) {
            series_metadata.insert("Magnification".into(), MetadataValue::Float(v as f64));
        }
        if let Some(v) = read_le_f32_at(&data, 48) {
            series_metadata.insert("kV".into(), MetadataValue::Float((v / 1000.0) as f64));
        }
        if let Some(v) = read_le_f32_at(&data, 52) {
            series_metadata.insert("Working distance".into(), MetadataValue::Float(v as f64));
        }
        if let Some(v) = read_le_f32_at(&data, 68) {
            series_metadata.insert("Spot".into(), MetadataValue::Float(v as f64));
        }

        self.path = Some(path.to_path_buf());
        self.header_size = header_size;
        self.meta = Some(ImageMetadata {
            size_x: width,
            size_y: height,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type: PixelType::Uint8,
            bits_per_pixel: 8,
            image_count: 1,
            dimension_order: DimensionOrder::XYCZT,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: true,
            resolution_count: 1,
            series_metadata,
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        });
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.header_size = 0;
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
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index != 0 {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }

        let path = self.path.clone().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = std::fs::File::open(&path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(self.header_size))
            .map_err(BioFormatsError::Io)?;

        let width = meta.size_x as usize;
        let height = meta.size_y as usize;
        let segment_len = width / 2;
        let invalid_len = (FEI_INVALID_PIXELS / 2) as usize;
        let mut segment = vec![0u8; segment_len];
        let mut invalid = vec![0u8; invalid_len];
        let mut plane = vec![0u8; width * height];

        for row_pass in 0..4 {
            let mut row = row_pass;
            while row < height {
                for col_pass in 0..2 {
                    f.read_exact(&mut segment).map_err(BioFormatsError::Io)?;
                    f.read_exact(&mut invalid).map_err(BioFormatsError::Io)?;
                    let mut col = col_pass;
                    while col < width {
                        plane[row * width + col] = segment[col / 2];
                        col += 2;
                    }
                }
                row += 4;
            }
        }

        Ok(plane)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let meta = self
            .meta
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?
            .clone();
        let full = self.open_bytes(plane_index)?;
        crop_full_plane("FEI/Philips IMG", &full, &meta, 1, x, y, w, h)
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

// ===========================================================================
// Real binary reader 3 — Veeco/Nanoscope AFM
// ===========================================================================

/// Veeco/Bruker Nanoscope AFM format (numeric extensions like `.001`, `.afm`).
///
/// Text header followed by raw binary pixel data. Detects via `*` first byte
/// and "NANOSCOPE" in the first 30 bytes.
pub struct VeecoReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    data_offset: usize,
}

impl VeecoReader {
    pub fn new() -> Self {
        VeecoReader {
            path: None,
            meta: None,
            data_offset: 0,
        }
    }
}

impl Default for VeecoReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for VeecoReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        // Match .afm or purely numeric extensions of 1-3 chars (e.g. "001")
        ext.eq_ignore_ascii_case("afm")
            || (ext.len() >= 1 && ext.len() <= 3 && ext.chars().all(|c| c.is_ascii_digit()))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        if header.is_empty() || header[0] != b'*' {
            return false;
        }
        let s = String::from_utf8_lossy(&header[..header.len().min(30)]);
        s.to_ascii_uppercase().contains("NANOSCOPE")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        let text = String::from_utf8_lossy(&data).into_owned();

        let mut width: Option<u32> = None;
        let mut height: Option<u32> = None;
        let mut bpp: Option<u32> = None;
        let mut data_offset: Option<usize> = None;

        for line in text.lines() {
            if line.contains("\\Samps/line:") {
                if let Some(val) = line.split_whitespace().last() {
                    if let Ok(n) = val.parse::<u32>() {
                        width = Some(n);
                    }
                }
            } else if line.contains("\\Number of lines:") {
                if let Some(val) = line.split_whitespace().last() {
                    if let Ok(n) = val.parse::<u32>() {
                        height = Some(n);
                    }
                }
            } else if line.contains("\\Bytes/pixel:") {
                if let Some(val) = line.split_whitespace().last() {
                    if let Ok(n) = val.parse::<u32>() {
                        bpp = Some(n);
                    }
                }
            } else if line.contains("\\Data offset:") {
                if let Some(val) = line.split_whitespace().last() {
                    if let Ok(n) = val.parse::<usize>() {
                        data_offset = Some(n);
                    }
                }
            }
        }

        let width = width.filter(|&v| v > 0).ok_or_else(|| {
            BioFormatsError::UnsupportedFormat("Nanoscope header missing Samps/line".into())
        })?;
        let height = height.filter(|&v| v > 0).ok_or_else(|| {
            BioFormatsError::UnsupportedFormat("Nanoscope header missing Number of lines".into())
        })?;
        let bpp = bpp.filter(|&v| v == 1 || v == 2).ok_or_else(|| {
            BioFormatsError::UnsupportedFormat(
                "Nanoscope header missing supported Bytes/pixel".into(),
            )
        })?;
        let data_offset = data_offset.ok_or_else(|| {
            BioFormatsError::UnsupportedFormat("Nanoscope header missing Data offset".into())
        })?;
        let expected = (data_offset as u64)
            .checked_add(
                (width as u64)
                    .saturating_mul(height as u64)
                    .saturating_mul(bpp as u64),
            )
            .ok_or_else(|| BioFormatsError::Format("Nanoscope plane size overflows".into()))?;
        if expected > data.len() as u64 {
            return Err(BioFormatsError::UnsupportedFormat(
                "Nanoscope pixel payload is shorter than declared dimensions".into(),
            ));
        }

        let pixel_type = if bpp == 1 {
            PixelType::Uint8
        } else {
            PixelType::Uint16
        };
        let bits_per_pixel = (bpp * 8) as u8;

        self.data_offset = data_offset;
        self.path = Some(path.to_path_buf());
        self.meta = Some(ImageMetadata {
            size_x: width,
            size_y: height,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type,
            bits_per_pixel,
            image_count: 1,
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
        });
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.data_offset = 0;
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
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let bps = (meta.bits_per_pixel / 8) as usize;
        let n_bytes = meta.size_x as usize * meta.size_y as usize * bps;
        let path = self.path.clone().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = std::fs::File::open(&path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(self.data_offset as u64))
            .map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; n_bytes];
        f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
        Ok(buf)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        _x: u32,
        _y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let meta = self
            .meta
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?
            .clone();
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let full = self.open_bytes(plane_index)?;
        crop_full_plane("Nanoscope", &full, &meta, 1, _x, _y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn resolution_count(&self) -> usize {
        1
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if level != 0 {
            Err(BioFormatsError::Format(format!(
                "resolution {} out of range",
                level
            )))
        } else {
            Ok(())
        }
    }
}

// ===========================================================================
// TIFF wrapper — Zeiss
// ===========================================================================

// ---------------------------------------------------------------------------
// ZeissTiffReader
// ---------------------------------------------------------------------------
tiff_wrapper! {
    /// Zeiss TIFF wrapper (`.tif`). Extension-only, no distinct magic.
    pub struct ZeissTiffReader;
    extensions: ["tif"];
}

// ===========================================================================
// Extension-only placeholder readers
// ===========================================================================

// ===========================================================================
// Helper: compute square dimensions from file size assuming uint16
// ===========================================================================

fn unsupported_raw_sem(format_name: &str) -> BioFormatsError {
    BioFormatsError::UnsupportedFormat(format!(
        "{format_name} binary layout is not implemented; refusing heuristic dimensions"
    ))
}

// ===========================================================================
// Real binary reader — JEOL SEM
// ===========================================================================

/// JEOL SEM data file reader (`.dat`).
///
/// JEOL SEM .dat files are typically raw 16-bit LE images. We compute
/// dimensions as sqrt(filesize / 2) rounded to nearest square.
pub struct JeolReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    data_offset: u64,
}

impl JeolReader {
    pub fn new() -> Self {
        JeolReader {
            path: None,
            meta: None,
            data_offset: 0,
        }
    }
}

impl Default for JeolReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for JeolReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("dat"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let _ = path;
        Err(unsupported_raw_sem("JEOL SEM"))
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.data_offset = 0;
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
        let _ = plane_index;
        Err(unsupported_raw_sem("JEOL SEM"))
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        _x: u32,
        _y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let _ = (plane_index, _x, _y, w, h);
        Err(unsupported_raw_sem("JEOL SEM"))
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn resolution_count(&self) -> usize {
        1
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if level != 0 {
            Err(BioFormatsError::Format(format!(
                "resolution {} out of range",
                level
            )))
        } else {
            Ok(())
        }
    }
}

// ===========================================================================
// Hitachi S-4800 SEM — INI text file + companion pixels file
// ===========================================================================

/// Hitachi S-4800 SEM reader.
///
/// Ported from `HitachiReader.java`. A Hitachi dataset is a `.txt` INI file
/// whose `[SemImageFile]` section names the actual pixels file (`ImageName`),
/// a similarly-named `.tif`, `.bmp`, or `.jpg` placed alongside the `.txt`.
/// Detection requires the magic string `[SemImageFile]` (the file may be
/// either ASCII or UTF-16 encoded). The pixels are read by delegating to the
/// auto-detecting `ImageReader`, exactly as the Java reader delegates to a
/// helper `ImageReader` with `HitachiReader` removed from the class list.
pub struct HitachiReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    /// Resolved path to the companion pixels file (.tif/.bmp/.jpg).
    pixels_file: Option<PathBuf>,
    /// Parsed `[SemImageFile]` key/value pairs.
    ini: HashMap<String, String>,
}

impl HitachiReader {
    const MAGIC: &'static str = "[SemImageFile]";

    pub fn new() -> Self {
        HitachiReader {
            path: None,
            meta: None,
            pixels_file: None,
            ini: HashMap::new(),
        }
    }

    /// Decode the header text as ASCII, falling back to UTF-16 (matching the
    /// Java reader's `new String(b, ENCODING)` then `new String(b, "UTF-16")`).
    fn decode_header(bytes: &[u8]) -> String {
        let ascii = String::from_utf8_lossy(bytes);
        if ascii.contains(Self::MAGIC) {
            return ascii.into_owned();
        }
        // UTF-16: try little-endian then big-endian.
        for be in [false, true] {
            let units: Vec<u16> = bytes
                .chunks_exact(2)
                .map(|c| {
                    if be {
                        u16::from_be_bytes([c[0], c[1]])
                    } else {
                        u16::from_le_bytes([c[0], c[1]])
                    }
                })
                .collect();
            let s = String::from_utf16_lossy(&units);
            if s.contains(Self::MAGIC) {
                return s;
            }
        }
        ascii.into_owned()
    }

    /// Parse a flat INI: lines `key=value` after the `[SemImageFile]` header.
    fn parse_ini(text: &str) -> HashMap<String, String> {
        let mut map = HashMap::new();
        let mut in_section = false;
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('[') && line.ends_with(']') {
                in_section = line.eq_ignore_ascii_case(Self::MAGIC);
                continue;
            }
            if !in_section {
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                map.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
        map
    }
}

impl Default for HitachiReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for HitachiReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("txt"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // Accept ASCII or UTF-16 occurrences of the magic.
        Self::decode_header(header).contains(Self::MAGIC)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        // If handed the companion pixels file, redirect to the sibling .txt
        // (Java initFile() does the same).
        let txt_path = if path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("txt"))
            .unwrap_or(false)
        {
            path.to_path_buf()
        } else {
            path.with_extension("txt")
        };

        let bytes = std::fs::read(&txt_path).map_err(BioFormatsError::Io)?;
        let text = Self::decode_header(&bytes);
        if !text.contains(Self::MAGIC) {
            return Err(BioFormatsError::UnsupportedFormat(
                "Hitachi: missing [SemImageFile] section".into(),
            ));
        }
        let ini = Self::parse_ini(&text);

        // Resolve the pixels file: stored ImageName next to the .txt, else
        // fall back to a same-base .tif/.jpg/.bmp.
        let parent = txt_path.parent().unwrap_or_else(|| Path::new("."));
        let mut pixels_file: Option<PathBuf> = None;
        if let Some(name) = ini.get("ImageName") {
            let candidate = parent.join(name);
            if candidate.exists() {
                pixels_file = Some(candidate);
            }
        }
        if pixels_file.is_none() {
            for ext in ["tif", "jpg", "bmp"] {
                let candidate = txt_path.with_extension(ext);
                if candidate.exists() {
                    pixels_file = Some(candidate);
                    break;
                }
            }
        }
        let pixels_file = pixels_file.ok_or_else(|| {
            BioFormatsError::UnsupportedFormat("Hitachi: could not find pixels file".into())
        })?;

        // Delegate to the auto-detecting reader for the companion image.
        let mut helper = crate::registry::ImageReader::open(&pixels_file)?;
        let mut meta = helper.metadata().clone();
        helper.close().ok();

        // Carry the [SemImageFile] metadata into series_metadata.
        for (k, v) in &ini {
            meta.series_metadata.insert(
                k.clone(),
                crate::common::metadata::MetadataValue::String(v.clone()),
            );
        }
        meta.series_metadata.insert(
            "format".into(),
            crate::common::metadata::MetadataValue::String("Hitachi".into()),
        );

        self.ini = ini;
        self.pixels_file = Some(pixels_file);
        self.path = Some(txt_path);
        self.meta = Some(meta);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.pixels_file = None;
        self.ini.clear();
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
        let pixels = self
            .pixels_file
            .clone()
            .ok_or(BioFormatsError::NotInitialized)?;
        let mut helper = crate::registry::ImageReader::open(&pixels)?;
        let bytes = helper.open_bytes(plane_index);
        helper.close().ok();
        bytes
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let pixels = self
            .pixels_file
            .clone()
            .ok_or(BioFormatsError::NotInitialized)?;
        let mut helper = crate::registry::ImageReader::open(&pixels)?;
        let bytes = helper.open_bytes_region(plane_index, x, y, w, h);
        helper.close().ok();
        bytes
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let pixels = self
            .pixels_file
            .clone()
            .ok_or(BioFormatsError::NotInitialized)?;
        let mut helper = crate::registry::ImageReader::open(&pixels)?;
        let bytes = helper.open_thumb_bytes(plane_index);
        helper.close().ok();
        bytes
    }

    fn resolution_count(&self) -> usize {
        1
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if level != 0 {
            Err(BioFormatsError::Format(format!(
                "resolution {} out of range",
                level
            )))
        } else {
            Ok(())
        }
    }
}

// ===========================================================================
// LEO / Zeiss EM — TIFF with proprietary LEO tag (34118)
// ===========================================================================

/// LEO EM reader (`.sxm`, `.tif`, `.tiff`).
///
/// Ported from `LEOReader.java` (extends `BaseTiffReader`). LEO files are
/// ordinary TIFFs distinguished by the presence of private tag 34118
/// (`LEO_TAG`), an ISO-8859-1 text blob of `AP_`/`DP_`/`SV_` key/value lines.
/// Pixel reading is plain TIFF; this reader delegates pixels to `TiffReader`
/// and parses the LEO tag for metadata.
pub struct LeoReader {
    inner: crate::tiff::TiffReader,
    meta: Option<ImageMetadata>,
}

impl LeoReader {
    const LEO_TAG: u16 = 34118;

    pub fn new() -> Self {
        LeoReader {
            inner: crate::tiff::TiffReader::new(),
            meta: None,
        }
    }
}

impl Default for LeoReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for LeoReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("sxm") | Some("tif") | Some("tiff"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        // Like Java (suffixSufficient=false), detection requires opening the
        // TIFF and checking for the LEO tag; header bytes alone are not enough.
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.inner.set_id(path)?;

        // Require the LEO private tag in the first IFD.
        let first = self
            .inner
            .ifd(0)
            .ok_or_else(|| BioFormatsError::UnsupportedFormat("LEO: no IFD".into()))?;
        if first.get(Self::LEO_TAG).is_none() {
            return Err(BioFormatsError::UnsupportedFormat(
                "LEO: TIFF is missing the LEO tag (34118)".into(),
            ));
        }

        let mut meta = self.inner.metadata().clone();
        meta.series_metadata
            .insert("format".into(), MetadataValue::String("LEO".into()));

        // Parse the LEO tag text: lines of `AP_*`/`DP_*`/`SV_*` keys whose
        // value lives on the following line (Java initStandardMetadata()).
        if let Some(tag_text) = first.get_str(Self::LEO_TAG) {
            let lines: Vec<&str> = tag_text.split('\n').collect();
            let mut i = 0usize;
            while i < lines.len() {
                let t = lines[i].trim();
                if (t.starts_with("AP_") || t.starts_with("DP_") || t.starts_with("SV_"))
                    && i + 1 < lines.len()
                {
                    let sep = if t == "AP_TIME" || t == "AP_DATE" {
                        ':'
                    } else {
                        '='
                    };
                    let val_line = lines[i + 1].trim();
                    if let Some((k, v)) = val_line.split_once(sep) {
                        meta.series_metadata.insert(
                            k.trim().to_string(),
                            MetadataValue::String(v.trim().to_string()),
                        );
                    }
                    i += 2;
                } else {
                    i += 1;
                }
            }
        }

        self.meta = Some(meta);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.meta = None;
        self.inner.close()
    }

    fn series_count(&self) -> usize {
        self.inner.series_count()
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        self.inner.set_series(s)
    }

    fn series(&self) -> usize {
        self.inner.series()
    }

    fn metadata(&self) -> &ImageMetadata {
        self.meta.as_ref().expect("set_id not called")
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        self.inner.open_bytes(plane_index)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        self.inner.open_bytes_region(plane_index, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        self.inner.open_thumb_bytes(plane_index)
    }

    fn resolution_count(&self) -> usize {
        self.inner.resolution_count()
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        self.inner.set_resolution(level)
    }
}

// ===========================================================================
// Real binary reader — Zeiss LMS
// ===========================================================================

/// Zeiss LMS reader (`.lms`).
///
/// Assumes raw uint16 LE binary data. Computes square dimensions from file size.
pub struct ZeissLmsReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    data_offset: u64,
}

impl ZeissLmsReader {
    pub fn new() -> Self {
        ZeissLmsReader {
            path: None,
            meta: None,
            data_offset: 0,
        }
    }
}

impl Default for ZeissLmsReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for ZeissLmsReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("lms"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let _ = path;
        Err(unsupported_raw_sem("Zeiss LMS"))
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.data_offset = 0;
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
        let _ = plane_index;
        Err(unsupported_raw_sem("Zeiss LMS"))
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        _x: u32,
        _y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let _ = (plane_index, _x, _y, w, h);
        Err(unsupported_raw_sem("Zeiss LMS"))
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn resolution_count(&self) -> usize {
        1
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if level != 0 {
            Err(BioFormatsError::Format(format!(
                "resolution {} out of range",
                level
            )))
        } else {
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// IMOD mesh format
// ---------------------------------------------------------------------------
placeholder_reader! {
    /// IMOD mesh format placeholder reader (`.mod`).
    pub struct ImrodReader;
    extensions: ["mod"];
}
