//! Extended format readers for Bio-Formats Rust.
//!
//! Group A: TIFF-based wrappers (DNG, QPTIFF, GEL).
//! Group B: Binary readers with structure (Imspector OBF, Hamamatsu VMS, Cellomics).
//! Group C: Extension-only placeholder readers (MRW, Yokogawa, etc.).

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn placeholder_meta_u16(w: u32, h: u32) -> ImageMetadata {
    ImageMetadata {
        size_x: w,
        size_y: h,
        size_z: 1,
        size_c: 1,
        size_t: 1,
        pixel_type: PixelType::Uint16,
        bits_per_pixel: 16u8,
        image_count: 1,
        dimension_order: DimensionOrder::XYZCT,
        is_rgb: false,
        is_interleaved: false,
        is_indexed: false,
        is_little_endian: false, // store as-is, big-endian
        resolution_count: 1,
        series_metadata: HashMap::new(),
        lookup_table: None,
        modulo_z: None,
        modulo_c: None,
        modulo_t: None,
    }
}

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
#[allow(unused_macros)]
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
                if level != 0 { Err(BioFormatsError::Format(format!("resolution {} out of range", level))) }
                else { Ok(()) }
            }
        }
    };
}

// ===========================================================================
// Group A — TIFF-based wrappers
// ===========================================================================

// ---------------------------------------------------------------------------
// 1. Adobe DNG (Digital Negative) RAW
// ---------------------------------------------------------------------------
tiff_wrapper! {
    /// Adobe DNG (Digital Negative) RAW format — TIFF-based (`.dng`).
    pub struct DngReader;
    extensions: ["dng"];
}

// ---------------------------------------------------------------------------
// 2. Akoya/PerkinElmer Phenocycler QPTIFF
// ---------------------------------------------------------------------------
tiff_wrapper! {
    /// Akoya/PerkinElmer Phenocycler QPTIFF — TIFF-based (`.qptiff`).
    pub struct QptiffReader;
    extensions: ["qptiff"];
}

// ===========================================================================
// Group A — Binary readers with structure
// ===========================================================================

// ---------------------------------------------------------------------------
// 3. Molecular Dynamics PhosphorImager GEL
// ---------------------------------------------------------------------------

/// Molecular Dynamics PhosphorImager GEL format (`.gel`).
///
/// 16-bit big-endian grayscale. Width at offset 10 (u16 BE), height at
/// offset 12 (u16 BE). Pixel data starts at offset 64.
pub struct GelReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl GelReader {
    pub fn new() -> Self {
        GelReader {
            path: None,
            meta: None,
        }
    }
}

impl Default for GelReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for GelReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("gel"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let data = std::fs::read(path).map_err(|e| BioFormatsError::Io(e))?;
        if data.len() < 64 {
            return Err(BioFormatsError::UnsupportedFormat(
                "GEL header is too short for Molecular Dynamics dimensions".into(),
            ));
        }

        let w = u16::from_be_bytes([data[10], data[11]]) as u32;
        let h = u16::from_be_bytes([data[12], data[13]]) as u32;
        if w == 0 || h == 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "GEL header contains zero dimensions".into(),
            ));
        }
        let plane_bytes = (w as usize)
            .checked_mul(h as usize)
            .and_then(|n| n.checked_mul(2))
            .ok_or_else(|| BioFormatsError::Format("GEL plane size overflows".into()))?;
        let expected_len = 64usize
            .checked_add(plane_bytes)
            .ok_or_else(|| BioFormatsError::Format("GEL file size overflows".into()))?;
        if data.len() < expected_len {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "GEL payload is shorter than declared image: got {} bytes, expected at least {expected_len}",
                data.len()
            )));
        }

        self.path = Some(path.to_path_buf());
        self.meta = Some(placeholder_meta_u16(w, h));
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
        let n_bytes = meta.size_x as usize * meta.size_y as usize * 2;
        let path = self.path.clone().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = std::fs::File::open(&path).map_err(|e| BioFormatsError::Io(e))?;
        f.seek(SeekFrom::Start(64))
            .map_err(|e| BioFormatsError::Io(e))?;
        let mut buf = vec![0u8; n_bytes];
        f.read_exact(&mut buf).map_err(|e| BioFormatsError::Io(e))?;
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
        let meta = self
            .meta
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?
            .clone();
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let full = self.open_bytes(plane_index)?;
        crop_full_plane("GEL", &full, &meta, 1, x, y, w, h)
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
// 4. Imspector OBF STED microscopy
// ---------------------------------------------------------------------------

const IMSPECTOR_FILE_MAGIC: &[u8; 8] = b"OMAS_BF\n";
const IMSPECTOR_MAGIC_NUMBER: u16 = 0xffff;
const IMSPECTOR_MIN_HEADER_LEN: usize = 14;

#[derive(Debug, Clone, PartialEq, Eq)]
struct ImspectorHeader {
    version: i32,
}

fn parse_imspector_header(bytes: &[u8]) -> Result<ImspectorHeader> {
    if bytes.len() < IMSPECTOR_MIN_HEADER_LEN {
        return Err(BioFormatsError::Format(
            "Imspector OBF/MSR header is truncated".to_string(),
        ));
    }
    if &bytes[..IMSPECTOR_FILE_MAGIC.len()] != IMSPECTOR_FILE_MAGIC {
        return Err(BioFormatsError::Format(
            "Not an Imspector OBF/MSR file".to_string(),
        ));
    }
    let magic_offset = IMSPECTOR_FILE_MAGIC.len();
    let magic = u16::from_le_bytes([bytes[magic_offset], bytes[magic_offset + 1]]);
    if magic != IMSPECTOR_MAGIC_NUMBER {
        return Err(BioFormatsError::Format(
            "Imspector OBF/MSR header has invalid magic number".to_string(),
        ));
    }
    let version_offset = magic_offset + 2;
    let version = i32::from_le_bytes([
        bytes[version_offset],
        bytes[version_offset + 1],
        bytes[version_offset + 2],
        bytes[version_offset + 3],
    ]);
    Ok(ImspectorHeader { version })
}

#[allow(dead_code)]
fn imspector_pixel_type(type_code: i32) -> Result<PixelType> {
    match type_code {
        0x01 => Ok(PixelType::Uint8),
        0x02 => Ok(PixelType::Int8),
        0x04 => Ok(PixelType::Uint16),
        0x08 => Ok(PixelType::Int16),
        0x10 => Ok(PixelType::Uint32),
        0x20 => Ok(PixelType::Int32),
        0x40 => Ok(PixelType::Float32),
        0x80 => Ok(PixelType::Float64),
        _ => Err(BioFormatsError::Format(format!(
            "Unsupported Imspector OBF/MSR data type {type_code}"
        ))),
    }
}

#[allow(dead_code)]
fn imspector_bits_per_pixel(type_code: i32) -> Result<u8> {
    Ok(match type_code {
        0x01 | 0x02 => 8,
        0x04 | 0x08 => 16,
        0x10 | 0x20 | 0x40 => 32,
        0x80 => 64,
        _ => {
            return Err(BioFormatsError::Format(format!(
                "Unsupported Imspector OBF/MSR data type {type_code}"
            )));
        }
    })
}

#[allow(dead_code)]
fn imspector_stack_length(length: i64) -> Result<u64> {
    if length >= 0 {
        Ok(length as u64)
    } else {
        Err(BioFormatsError::Format(
            "Negative Imspector OBF/MSR stack length on disk".to_string(),
        ))
    }
}

#[allow(dead_code)]
fn imspector_compression_flag(compression: i32) -> Result<bool> {
    match compression {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(BioFormatsError::Format(format!(
            "Unsupported Imspector OBF/MSR compression {compression}"
        ))),
    }
}

fn imspector_read_len_string(bytes: &[u8], offset: &mut usize) -> Result<String> {
    if bytes.len().saturating_sub(*offset) < 4 {
        return Err(BioFormatsError::Format(
            "Imspector OBF/MSR length-prefixed string is truncated".to_string(),
        ));
    }
    let len = i32::from_le_bytes([
        bytes[*offset],
        bytes[*offset + 1],
        bytes[*offset + 2],
        bytes[*offset + 3],
    ]);
    *offset += 4;
    if len <= 0 {
        return Ok(String::new());
    }
    let len = len as usize;
    if bytes.len().saturating_sub(*offset) < len {
        return Err(BioFormatsError::Format(
            "Imspector OBF/MSR length-prefixed string overruns input".to_string(),
        ));
    }
    let value = std::str::from_utf8(&bytes[*offset..*offset + len])
        .map_err(|e| {
            BioFormatsError::Format(format!("Imspector OBF/MSR string is not UTF-8: {e}"))
        })?
        .to_string();
    *offset += len;
    Ok(value)
}

/// Imspector OBF/MSR STED microscopy format stub (`.obf`, `.msr`).
///
/// Header parsing is translated from Bio-Formats' `OBFReader`; stack metadata
/// and payload decoding are still intentionally rejected until ported.
pub struct ImspectorReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl ImspectorReader {
    pub fn new() -> Self {
        ImspectorReader {
            path: None,
            meta: None,
        }
    }
}

impl Default for ImspectorReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for ImspectorReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("obf") | Some("msr"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        parse_imspector_header(header).is_ok()
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.path = None;
        self.meta = None;
        let bytes = std::fs::read(path).map_err(BioFormatsError::Io)?;
        let header = parse_imspector_header(&bytes)?;
        let mut detail = format!(
            "Imspector OBF/MSR stack metadata and payload decoding is not implemented (version {})",
            header.version
        );
        if bytes.len() > IMSPECTOR_MIN_HEADER_LEN + 12 {
            let mut offset = IMSPECTOR_MIN_HEADER_LEN + 8;
            if let Ok(description) = imspector_read_len_string(&bytes, &mut offset) {
                if !description.is_empty() {
                    detail.push_str("; header description parsed");
                }
            }
        }
        Err(BioFormatsError::UnsupportedFormat(detail))
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
        let _ = plane_index;
        Err(BioFormatsError::UnsupportedFormat(
            "Imspector OBF/MSR payload decoding is not implemented".to_string(),
        ))
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        _x: u32,
        _y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let _ = (plane_index, w, h);
        Err(BioFormatsError::UnsupportedFormat(
            "Imspector OBF/MSR payload decoding is not implemented".to_string(),
        ))
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let _ = plane_index;
        Err(BioFormatsError::UnsupportedFormat(
            "Imspector OBF/MSR payload decoding is not implemented".to_string(),
        ))
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

#[cfg(test)]
mod imspector_tests {
    use super::{
        imspector_bits_per_pixel, imspector_compression_flag, imspector_pixel_type,
        imspector_read_len_string, imspector_stack_length, parse_imspector_header, ImspectorReader,
        IMSPECTOR_FILE_MAGIC, IMSPECTOR_MAGIC_NUMBER,
    };
    use crate::common::error::BioFormatsError;
    use crate::common::pixel_type::PixelType;
    use crate::common::reader::FormatReader;
    use std::path::PathBuf;

    fn imspector_header(version: i32) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(IMSPECTOR_FILE_MAGIC);
        bytes.extend_from_slice(&IMSPECTOR_MAGIC_NUMBER.to_le_bytes());
        bytes.extend_from_slice(&version.to_le_bytes());
        bytes
    }

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("bioformats_imspector_{name}"))
    }

    #[test]
    fn imspector_header_requires_exact_java_magic_and_magic_number() {
        let good = imspector_header(6);
        assert_eq!(parse_imspector_header(&good).unwrap().version, 6);

        let mut wrong_magic = good.clone();
        wrong_magic[7] = b'_';
        assert!(matches!(
            parse_imspector_header(&wrong_magic),
            Err(BioFormatsError::Format(message)) if message.contains("Not an Imspector")
        ));

        let mut wrong_number = good;
        wrong_number[8..10].copy_from_slice(&0x1234u16.to_le_bytes());
        assert!(matches!(
            parse_imspector_header(&wrong_number),
            Err(BioFormatsError::Format(message)) if message.contains("invalid magic number")
        ));
    }

    #[test]
    fn imspector_reader_detects_only_complete_obf_header() {
        let reader = ImspectorReader::new();
        assert!(!reader.is_this_type_by_bytes(b"OMAS_BF_not enough"));
        assert!(reader.is_this_type_by_bytes(&imspector_header(4)));
    }

    #[test]
    fn imspector_helpers_match_bioformats_type_contracts() {
        assert_eq!(imspector_pixel_type(0x01).unwrap(), PixelType::Uint8);
        assert_eq!(imspector_pixel_type(0x08).unwrap(), PixelType::Int16);
        assert_eq!(imspector_pixel_type(0x40).unwrap(), PixelType::Float32);
        assert_eq!(imspector_bits_per_pixel(0x80).unwrap(), 64);
        assert!(matches!(
            imspector_pixel_type(0x03),
            Err(BioFormatsError::Format(message)) if message.contains("Unsupported")
        ));

        assert_eq!(imspector_stack_length(17).unwrap(), 17);
        assert!(matches!(
            imspector_stack_length(-1),
            Err(BioFormatsError::Format(message)) if message.contains("Negative")
        ));
        assert!(!imspector_compression_flag(0).unwrap());
        assert!(imspector_compression_flag(1).unwrap());
        assert!(matches!(
            imspector_compression_flag(2),
            Err(BioFormatsError::Format(message)) if message.contains("Unsupported")
        ));
    }

    #[test]
    fn imspector_length_prefixed_string_tracks_offset_and_bounds() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&5i32.to_le_bytes());
        bytes.extend_from_slice(b"hello");
        bytes.extend_from_slice(&0i32.to_le_bytes());

        let mut offset = 0;
        assert_eq!(
            imspector_read_len_string(&bytes, &mut offset).unwrap(),
            "hello"
        );
        assert_eq!(offset, 9);
        assert_eq!(imspector_read_len_string(&bytes, &mut offset).unwrap(), "");
        assert_eq!(offset, 13);

        let mut truncated_offset = 0;
        assert!(matches!(
            imspector_read_len_string(&[4, 0, 0, 0, b'x'], &mut truncated_offset),
            Err(BioFormatsError::Format(message)) if message.contains("overruns")
        ));
    }

    #[test]
    fn imspector_set_id_parses_header_then_refuses_unported_stack_decoder() {
        let path = temp_path("header_only.obf");
        let mut bytes = imspector_header(7);
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&4i32.to_le_bytes());
        bytes.extend_from_slice(b"desc");
        std::fs::write(&path, bytes).unwrap();

        let mut reader = ImspectorReader::new();
        let err = reader.set_id(&path).unwrap_err();
        assert!(matches!(
            err,
            BioFormatsError::UnsupportedFormat(message)
                if message.contains("version 7")
                    && message.contains("stack metadata and payload decoding")
        ));

        let _ = std::fs::remove_file(path);
    }
}

// ---------------------------------------------------------------------------
// 5. Hamamatsu VMS whole-slide
// ---------------------------------------------------------------------------

/// Hamamatsu VMS/VMU whole-slide format stub (`.vms`, `.vmu`).
///
/// Full tile metadata and JPEG payload decoding are not implemented.
pub struct HamamatsuVmsReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl HamamatsuVmsReader {
    pub fn new() -> Self {
        HamamatsuVmsReader {
            path: None,
            meta: None,
        }
    }
}

impl Default for HamamatsuVmsReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for HamamatsuVmsReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("vms") | Some("vmu"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, _path: &Path) -> Result<()> {
        self.path = None;
        self.meta = None;
        Err(BioFormatsError::UnsupportedFormat(
            "Hamamatsu VMS/VMU JPEG tile payload decoding is not implemented".to_string(),
        ))
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
        let _ = plane_index;
        Err(BioFormatsError::UnsupportedFormat(
            "Hamamatsu VMS/VMU JPEG tile payload decoding is not implemented".to_string(),
        ))
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        _x: u32,
        _y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let _ = (plane_index, w, h);
        Err(BioFormatsError::UnsupportedFormat(
            "Hamamatsu VMS/VMU JPEG tile payload decoding is not implemented".to_string(),
        ))
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let _ = plane_index;
        Err(BioFormatsError::UnsupportedFormat(
            "Hamamatsu VMS/VMU JPEG tile payload decoding is not implemented".to_string(),
        ))
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
// 6. Cellomics HCS
// ---------------------------------------------------------------------------

/// Cellomics HCS format (`.c01`).
///
/// Binary format. Header at offset 4: width (u16 LE), offset 6: height (u16 LE),
/// offset 8: bit_depth (u16 LE). Pixel data at offset 52.
pub struct CellomicsReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    pixel_offset: u64,
}

impl CellomicsReader {
    pub fn new() -> Self {
        CellomicsReader {
            path: None,
            meta: None,
            pixel_offset: 52,
        }
    }
}

impl Default for CellomicsReader {
    fn default() -> Self {
        Self::new()
    }
}

fn parse_legacy_cellomics_header(data: &[u8]) -> Result<(u32, u32, u32, PixelType, u8, u64)> {
    let w = u16::from_le_bytes([data[4], data[5]]) as u32;
    let h = u16::from_le_bytes([data[6], data[7]]) as u32;
    let bd = u16::from_le_bytes([data[8], data[9]]);
    if w == 0 || h == 0 || w > 32768 || h > 32768 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "Cellomics legacy header has missing or invalid image dimensions {w}x{h}"
        )));
    }
    let (pt, bpp) = match bd {
        8 => (PixelType::Uint8, 8u8),
        16 => (PixelType::Uint16, 16u8),
        _ => {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "Cellomics legacy bits per pixel {bd} is not supported"
            )));
        }
    };
    Ok((w, h, 1, pt, bpp, 52))
}

impl FormatReader for CellomicsReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("c01") | Some("dib"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let data = std::fs::read(path).map_err(|e| BioFormatsError::Io(e))?;

        let (w, h, image_count, pixel_type, bpp, pixel_offset) = if data.len() >= 52 {
            let dib_header_size = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
            if dib_header_size >= 40 {
                let w = i32::from_le_bytes([data[4], data[5], data[6], data[7]]).unsigned_abs();
                let h = i32::from_le_bytes([data[8], data[9], data[10], data[11]]).unsigned_abs();
                let n_planes = u16::from_le_bytes([data[12], data[13]]) as u32;
                let bd = u16::from_le_bytes([data[14], data[15]]);
                let compression = u32::from_le_bytes([data[16], data[17], data[18], data[19]]);
                if compression != 0 {
                    return Err(BioFormatsError::UnsupportedFormat(format!(
                        "Cellomics DIB compressed pixel data is not supported: compression={compression}"
                    )));
                }
                if w == 0 || h == 0 || w > 32768 || h > 32768 {
                    return Err(BioFormatsError::InvalidData(format!(
                        "Cellomics DIB has invalid dimensions {w}x{h}"
                    )));
                }
                let (pt, bpp) = match bd {
                    8 => (PixelType::Uint8, 8u8),
                    16 => (PixelType::Uint16, 16u8),
                    _ => {
                        return Err(BioFormatsError::UnsupportedFormat(format!(
                            "Cellomics DIB bits per pixel {bd} is not supported"
                        )));
                    }
                };
                let bytes_per_pixel = (bpp / 8) as u64;
                let image_count = n_planes.max(1);
                let plane_bytes = (w as u64)
                    .checked_mul(h as u64)
                    .and_then(|n| n.checked_mul(bytes_per_pixel))
                    .ok_or_else(|| {
                        BioFormatsError::Format("Cellomics DIB plane size overflows".to_string())
                    })?;
                let expected = 52u64
                    .checked_add(plane_bytes.checked_mul(image_count as u64).ok_or_else(|| {
                        BioFormatsError::Format(
                            "Cellomics DIB total pixel size overflows".to_string(),
                        )
                    })?)
                    .ok_or_else(|| {
                        BioFormatsError::Format("Cellomics DIB file size overflows".to_string())
                    })?;
                if (data.len() as u64) < expected {
                    return Err(BioFormatsError::InvalidData(format!(
                        "Cellomics DIB is too short: got {} bytes, expected at least {expected}",
                        data.len()
                    )));
                }
                (w, h, image_count, pt, bpp, 52u64)
            } else {
                parse_legacy_cellomics_header(&data)?
            }
        } else if data.len() >= 10 {
            parse_legacy_cellomics_header(&data)?
        } else {
            return Err(BioFormatsError::UnsupportedFormat(
                "Cellomics header is too short to determine image dimensions".to_string(),
            ));
        };

        let bytes_per_pixel = (bpp / 8) as u64;
        let plane_bytes = (w as u64)
            .checked_mul(h as u64)
            .and_then(|n| n.checked_mul(bytes_per_pixel))
            .ok_or_else(|| BioFormatsError::Format("Cellomics plane size overflows".to_string()))?;
        let expected = pixel_offset
            .checked_add(plane_bytes.checked_mul(image_count as u64).ok_or_else(|| {
                BioFormatsError::Format("Cellomics total pixel size overflows".to_string())
            })?)
            .ok_or_else(|| BioFormatsError::Format("Cellomics file size overflows".to_string()))?;
        if (data.len() as u64) < expected {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "Cellomics pixel payload is shorter than declared image: got {} bytes, expected at least {expected}",
                data.len()
            )));
        }

        self.path = Some(path.to_path_buf());
        self.pixel_offset = pixel_offset;
        self.meta = Some(ImageMetadata {
            size_x: w,
            size_y: h,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type,
            bits_per_pixel: bpp,
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
        });
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.pixel_offset = 52;
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
        let bytes_per_pixel = (meta.bits_per_pixel / 8) as usize;
        let n_bytes = meta.size_x as usize * meta.size_y as usize * bytes_per_pixel;
        let path = self.path.clone().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = std::fs::File::open(&path).map_err(|e| BioFormatsError::Io(e))?;
        let plane_offset = self
            .pixel_offset
            .checked_add(
                (plane_index as u64)
                    .checked_mul(n_bytes as u64)
                    .ok_or_else(|| {
                        BioFormatsError::Format("Cellomics plane offset overflows".to_string())
                    })?,
            )
            .ok_or_else(|| {
                BioFormatsError::Format("Cellomics plane offset overflows".to_string())
            })?;
        f.seek(SeekFrom::Start(plane_offset))
            .map_err(|e| BioFormatsError::Io(e))?;
        let mut buf = vec![0u8; n_bytes];
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
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let full = self.open_bytes(plane_index)?;
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        crop_full_plane("Cellomics", &full, meta, 1, x, y, w, h)
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
// Group B — Extension-only placeholder readers
// ===========================================================================

// ---------------------------------------------------------------------------
// 7. Minolta Digital Camera RAW — TIFF delegate
// ---------------------------------------------------------------------------
/// Minolta Digital Camera RAW reader (`.mrw`).
///
/// Minolta RAW files have a TIFF structure inside; delegates to `TiffReader`.
/// If the proprietary header prevents parsing, `set_id` will propagate the error.
pub struct MrwReader {
    inner: crate::tiff::TiffReader,
}

impl MrwReader {
    pub fn new() -> Self {
        MrwReader {
            inner: crate::tiff::TiffReader::new(),
        }
    }
}

impl Default for MrwReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for MrwReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("mrw"))
    }
    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }
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

// ---------------------------------------------------------------------------
// 8. Yokogawa CV7000/8000 HCS — XML index + TIFF images
// ---------------------------------------------------------------------------
/// Yokogawa CV7000/8000 HCS reader (`.wpi`, `.mrf`).
///
/// Yokogawa high-content screening systems store data as XML index files
/// referencing TIFF tile images. Attempts to locate and open accompanying
/// TIFF files via TiffReader.
pub struct YokogawaReader {
    inner: crate::tiff::TiffReader,
}

impl YokogawaReader {
    pub fn new() -> Self {
        YokogawaReader {
            inner: crate::tiff::TiffReader::new(),
        }
    }
}

impl Default for YokogawaReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for YokogawaReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("wpi") | Some("mrf"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        // Try to find a companion TIFF in the same directory
        if let Some(parent) = path.parent() {
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            for ext in &["tif", "tiff"] {
                let tiff_path = parent.join(format!("{}.{}", stem, ext));
                if tiff_path.exists() {
                    return self.inner.set_id(&tiff_path);
                }
            }
        }
        // Fall back to trying to open the file itself as TIFF
        self.inner.set_id(path).map_err(|_| {
            BioFormatsError::UnsupportedFormat(
                "Yokogawa CV7000/8000: could not find companion TIFF images for this index file"
                    .to_string(),
            )
        })
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

// ---------------------------------------------------------------------------
// 9. Leica single-image LOF
// ---------------------------------------------------------------------------
/// Leica single-image LOF reader (`.lof`).
///
/// Leica LOF is a proprietary binary format used by Leica Application Suite.
/// The internal structure is vendor-specific and undocumented.
pub struct LeicaLofReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl LeicaLofReader {
    pub fn new() -> Self {
        LeicaLofReader {
            path: None,
            meta: None,
        }
    }
}

impl Default for LeicaLofReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for LeicaLofReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("lof"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, _path: &Path) -> Result<()> {
        Err(BioFormatsError::UnsupportedFormat(
            "Leica LOF is a proprietary binary format from Leica Application Suite".to_string(),
        ))
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

    fn open_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "Leica LOF is a proprietary binary format from Leica Application Suite".to_string(),
        ))
    }

    fn open_bytes_region(
        &mut self,
        _plane_index: u32,
        _x: u32,
        _y: u32,
        _w: u32,
        _h: u32,
    ) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "Leica LOF is a proprietary binary format from Leica Application Suite".to_string(),
        ))
    }

    fn open_thumb_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "Leica LOF is a proprietary binary format from Leica Application Suite".to_string(),
        ))
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
// 10. Animated PNG — delegates to PngReader
// ---------------------------------------------------------------------------
/// Animated PNG reader (`.apng`).
///
/// Tries to open the file as a regular PNG via `PngReader` (reads the first
/// frame). Full APNG animation decoding is not supported.
pub struct ApngReader {
    inner: crate::formats::png::PngReader,
}

impl ApngReader {
    pub fn new() -> Self {
        ApngReader {
            inner: crate::formats::png::PngReader::new(),
        }
    }
}

impl Default for ApngReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for ApngReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("apng"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // PNG magic: 89 50 4E 47 0D 0A 1A 0A
        header.len() >= 8 && header[..8] == [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.inner.set_id(path).map_err(|err| match err {
            BioFormatsError::UnsupportedFormat(_) => err,
            _ => BioFormatsError::UnsupportedFormat(
                "APNG file could not be opened as PNG (animated PNG may require dedicated parser)"
                    .to_string(),
            ),
        })
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
// 11. POV-Ray density grid (DF3)
// ---------------------------------------------------------------------------
/// POV-Ray density grid reader (`.pov`, `.df3`).
///
/// DF3 format: 6-byte header (3x uint16 BE: x, y, z dimensions) followed
/// by raw uint8 voxel data.
pub struct PovRayReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    pixel_data: Option<Vec<u8>>,
}

impl PovRayReader {
    pub fn new() -> Self {
        PovRayReader {
            path: None,
            meta: None,
            pixel_data: None,
        }
    }
}

impl Default for PovRayReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for PovRayReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("pov") | Some("df3"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        if data.len() < 6 {
            return Err(BioFormatsError::Format(
                "DF3 file too short (need at least 6-byte header)".to_string(),
            ));
        }

        let size_x = u16::from_be_bytes([data[0], data[1]]) as u32;
        let size_y = u16::from_be_bytes([data[2], data[3]]) as u32;
        let size_z = u16::from_be_bytes([data[4], data[5]]) as u32;

        if size_x == 0 || size_y == 0 || size_z == 0 {
            return Err(BioFormatsError::Format(
                "DF3 header contains zero dimensions".to_string(),
            ));
        }

        let plane_bytes = (size_x as usize)
            .checked_mul(size_y as usize)
            .ok_or_else(|| BioFormatsError::Format("DF3 plane byte count overflows".into()))?;
        let expected_pixels = plane_bytes
            .checked_mul(size_z as usize)
            .ok_or_else(|| BioFormatsError::Format("DF3 voxel byte count overflows".into()))?;
        if data.len() - 6 != expected_pixels {
            return Err(BioFormatsError::Format(format!(
                "DF3 pixel payload has {} bytes, expected {}",
                data.len() - 6,
                expected_pixels
            )));
        }

        let pixel_data = data[6..].to_vec();
        let image_count = size_z.max(1);

        self.path = Some(path.to_path_buf());
        self.pixel_data = Some(pixel_data);
        self.meta = Some(ImageMetadata {
            size_x,
            size_y,
            size_z,
            size_c: 1,
            size_t: 1,
            pixel_type: PixelType::Uint8,
            bits_per_pixel: 8,
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
        });
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.pixel_data = None;
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
        let pixels = self
            .pixel_data
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?;
        let plane_bytes = meta.size_x as usize * meta.size_y as usize;
        let offset = plane_index as usize * plane_bytes;
        let end = offset
            .checked_add(plane_bytes)
            .ok_or_else(|| BioFormatsError::Format("DF3 plane offset overflows".into()))?;
        Ok(pixels[offset..end].to_vec())
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        if x.checked_add(w).is_none_or(|end| end > meta.size_x)
            || y.checked_add(h).is_none_or(|end| end > meta.size_y)
        {
            return Err(BioFormatsError::InvalidData(format!(
                "DF3 region x={x} y={y} width={w} height={h} exceeds image {}x{}",
                meta.size_x, meta.size_y
            )));
        }

        let pixels = self
            .pixel_data
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?;
        let row_bytes = meta.size_x as usize;
        let plane_bytes = row_bytes
            .checked_mul(meta.size_y as usize)
            .ok_or_else(|| BioFormatsError::Format("DF3 plane byte count overflows".into()))?;
        let plane_offset = (plane_index as usize)
            .checked_mul(plane_bytes)
            .ok_or_else(|| BioFormatsError::Format("DF3 plane offset overflows".into()))?;
        let mut out = Vec::with_capacity(w as usize * h as usize);
        for row in y..y + h {
            let offset = plane_offset + row as usize * row_bytes + x as usize;
            out.extend_from_slice(&pixels[offset..offset + w as usize]);
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
// 12. NAF format
// ---------------------------------------------------------------------------
/// NAF format reader (`.naf`).
///
/// NAF is a proprietary format with undocumented structure.
pub struct NafReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl NafReader {
    pub fn new() -> Self {
        NafReader {
            path: None,
            meta: None,
        }
    }
}

impl Default for NafReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for NafReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("naf"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, _path: &Path) -> Result<()> {
        Err(BioFormatsError::UnsupportedFormat(
            "NAF is a proprietary format with undocumented structure".to_string(),
        ))
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

    fn open_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "NAF is a proprietary format with undocumented structure".to_string(),
        ))
    }

    fn open_bytes_region(
        &mut self,
        _plane_index: u32,
        _x: u32,
        _y: u32,
        _w: u32,
        _h: u32,
    ) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "NAF is a proprietary format with undocumented structure".to_string(),
        ))
    }

    fn open_thumb_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "NAF is a proprietary format with undocumented structure".to_string(),
        ))
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
// 13. Burleigh piezo/SPM
// ---------------------------------------------------------------------------
/// Burleigh piezo/SPM reader (`.img`).
///
/// NOTE: `.img` is a very generic extension shared by many formats.
/// Burleigh SPM images have an undocumented proprietary structure.
/// This reader is a last-resort extension fallback.
pub struct BurleighReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl BurleighReader {
    pub fn new() -> Self {
        BurleighReader {
            path: None,
            meta: None,
        }
    }
}

impl Default for BurleighReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for BurleighReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("img"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, _path: &Path) -> Result<()> {
        Err(BioFormatsError::UnsupportedFormat(
            "Burleigh SPM .img format is proprietary; .img extension is too generic for reliable detection".to_string()
        ))
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

    fn open_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "Burleigh SPM .img format is proprietary; .img extension is too generic for reliable detection".to_string()
        ))
    }

    fn open_bytes_region(
        &mut self,
        _plane_index: u32,
        _x: u32,
        _y: u32,
        _w: u32,
        _h: u32,
    ) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "Burleigh SPM .img format is proprietary; .img extension is too generic for reliable detection".to_string()
        ))
    }

    fn open_thumb_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "Burleigh SPM .img format is proprietary; .img extension is too generic for reliable detection".to_string()
        ))
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
