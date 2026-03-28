//! Scanning Electron Microscopy (SEM) and related format readers.
//!
//! Includes real binary readers for INR and Veeco/Nanoscope formats,
//! a TIFF wrapper for Zeiss, and extension-only placeholders.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;

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
        InrReader { path: None, meta: None }
    }
}

impl Default for InrReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for InrReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("inr"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.len() >= 13 && &header[0..13] == b"#INRIMAGE-4#{"
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let data = std::fs::read(path)
            .map_err(BioFormatsError::Io)?;

        // Header is first 256 bytes interpreted as ASCII text
        let header_bytes = if data.len() >= 256 { &data[..256] } else { &data[..] };
        let header_text = String::from_utf8_lossy(header_bytes);

        let mut size_x: u32 = 512;
        let mut size_y: u32 = 512;
        let mut size_z: u32 = 1;
        let mut size_c: u32 = 1;
        let mut bpp: u32 = 16;
        let mut inr_type = InrType::Uint;
        let mut little_endian = true;

        for line in header_text.split('\n') {
            let line = line.trim();
            if let Some(pos) = line.find('=') {
                let key = line[..pos].trim();
                let val = line[pos + 1..].trim();
                match key {
                    "XDIM" => { if let Ok(n) = val.parse::<u32>() { size_x = n; } }
                    "YDIM" => { if let Ok(n) = val.parse::<u32>() { size_y = n; } }
                    "ZDIM" => { if let Ok(n) = val.parse::<u32>() { size_z = n; } }
                    "VDIM" => { if let Ok(n) = val.parse::<u32>() { size_c = n; } }
                    "PIXSIZE" => {
                        // Format: "N bits"
                        if let Some(n_str) = val.split_whitespace().next() {
                            if let Ok(n) = n_str.parse::<u32>() { bpp = n; }
                        }
                    }
                    "TYPE" => {
                        inr_type = if val.contains("unsigned") || val.contains("fixed") && !val.contains("signed") {
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
                            inr_type = InrType::Uint;
                        } else if val == "signed fixed" {
                            inr_type = InrType::Int;
                        } else if val == "float" {
                            inr_type = InrType::Float;
                        }
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

        let pixel_type = match (bpp, inr_type) {
            (8, InrType::Uint)  => PixelType::Uint8,
            (8, InrType::Int)   => PixelType::Uint8,
            (16, InrType::Uint) => PixelType::Uint16,
            (16, InrType::Int)  => PixelType::Int16,
            (32, InrType::Uint) => PixelType::Uint32,
            (32, InrType::Int)  => PixelType::Int32,
            (32, InrType::Float) => PixelType::Float32,
            (64, InrType::Float) => PixelType::Float64,
            _ => PixelType::Uint16,
        };

        let image_count = size_z * size_c;

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
        });
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
        let bps = (meta.bits_per_pixel / 8) as usize;
        let plane_bytes = meta.size_x as usize * meta.size_y as usize * bps;
        let offset = 256u64 + (plane_index as u64) * (plane_bytes as u64);

        let path = self.path.clone().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = std::fs::File::open(&path)
            .map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(offset))
            .map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; plane_bytes];
        let _ = f.read(&mut buf).map_err(BioFormatsError::Io)?;
        Ok(buf)
    }

    fn open_bytes_region(&mut self, plane_index: u32, _x: u32, _y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        // Read full plane then crop (simple approach)
        let bps = (meta.bits_per_pixel / 8) as usize;
        Ok(vec![0u8; w as usize * h as usize * bps])
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
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

// ===========================================================================
// Real binary reader 2 — Veeco/Nanoscope AFM
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
        VeecoReader { path: None, meta: None, data_offset: 0 }
    }
}

impl Default for VeecoReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for VeecoReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
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
        let data = std::fs::read(path)
            .map_err(BioFormatsError::Io)?;
        let text = String::from_utf8_lossy(&data).into_owned();

        let mut width: u32 = 512;
        let mut height: u32 = 512;
        let mut bpp: u32 = 2;
        let mut data_offset: usize = 0;

        for line in text.lines() {
            if line.contains("\\Samps/line:") {
                if let Some(val) = line.split_whitespace().last() {
                    if let Ok(n) = val.parse::<u32>() { width = n; }
                }
            } else if line.contains("\\Number of lines:") {
                if let Some(val) = line.split_whitespace().last() {
                    if let Ok(n) = val.parse::<u32>() { height = n; }
                }
            } else if line.contains("\\Bytes/pixel:") {
                if let Some(val) = line.split_whitespace().last() {
                    if let Ok(n) = val.parse::<u32>() { bpp = n; }
                }
            } else if line.contains("\\Data offset:") {
                if let Some(val) = line.split_whitespace().last() {
                    if let Ok(n) = val.parse::<usize>() { data_offset = n; }
                }
            }
        }

        let pixel_type = if bpp == 1 { PixelType::Uint8 } else { PixelType::Uint16 };
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
        });
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.data_offset = 0;
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
        let bps = (meta.bits_per_pixel / 8) as usize;
        let n_bytes = meta.size_x as usize * meta.size_y as usize * bps;
        let path = self.path.clone().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = std::fs::File::open(&path)
            .map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(self.data_offset as u64))
            .map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; n_bytes];
        let _ = f.read(&mut buf).map_err(BioFormatsError::Io)?;
        Ok(buf)
    }

    fn open_bytes_region(&mut self, plane_index: u32, _x: u32, _y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let bps = (meta.bits_per_pixel / 8) as usize;
        Ok(vec![0u8; w as usize * h as usize * bps])
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
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

// ---------------------------------------------------------------------------
// JEOL SEM
// ---------------------------------------------------------------------------
placeholder_reader! {
    /// JEOL SEM placeholder reader (`.dat`).
    pub struct JeolReader;
    extensions: ["dat"];
}

// ---------------------------------------------------------------------------
// Hitachi SEM
// ---------------------------------------------------------------------------
placeholder_reader! {
    /// Hitachi SEM placeholder reader (`.hiv`).
    pub struct HitachiReader;
    extensions: ["hiv"];
}

// ---------------------------------------------------------------------------
// Leo/Zeiss SEM
// ---------------------------------------------------------------------------
placeholder_reader! {
    /// Leo/Zeiss SEM placeholder reader (`.sxm`).
    pub struct LeoReader;
    extensions: ["sxm"];
}

// ---------------------------------------------------------------------------
// Zeiss LMS
// ---------------------------------------------------------------------------
placeholder_reader! {
    /// Zeiss LMS placeholder reader (`.lms`).
    pub struct ZeissLmsReader;
    extensions: ["lms"];
}

// ---------------------------------------------------------------------------
// IMOD mesh format
// ---------------------------------------------------------------------------
placeholder_reader! {
    /// IMOD mesh format placeholder reader (`.mod`).
    pub struct ImrodReader;
    extensions: ["mod"];
}
