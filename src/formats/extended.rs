//! Extended format readers for Bio-Formats Rust.
//!
//! Group A: TIFF-based wrappers (DNG, QPTIFF, GEL).
//! Group B: Binary readers with structure (Imspector OBF, Hamamatsu VMS, Cellomics).
//! Group C: Extension-only placeholder readers (MRW, Yokogawa, etc.).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata};
use crate::common::ome_metadata::{
    create_lsid, OmeImage, OmeMetadata, OmePlate, OmeWell, OmeWellSample,
};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

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

/// Amersham Biosciences / Molecular Dynamics GEL format (`.gel`).
///
/// Ported from the upstream Java `GelReader`, which extends `BaseTiffReader`:
/// a GEL file is a TIFF carrying private Molecular Dynamics tags. The data
/// format tag (`MD_FILETAG` = 33445) is either LINEAR (128, plain TIFF) or
/// SQUARE_ROOT (2). For SQUARE_ROOT, the stored unsigned-short samples must be
/// squared and multiplied by the `MD_SCALE_PIXEL` (33446) rational scale, and
/// the pixel type becomes 32-bit float. Image count equals the number of IFDs
/// and is reported as the T dimension.
const MD_FILETAG: u16 = 33445;
const MD_SCALE_PIXEL: u16 = 33446;
const GEL_SQUARE_ROOT: u64 = 2;

pub struct GelReader {
    inner: crate::tiff::TiffReader,
    meta: Option<ImageMetadata>,
    /// True when the data format is SQUARE_ROOT (pixels need squaring/scaling).
    square_root: bool,
    /// MD_SCALE_PIXEL rational as f64 (defaults to 1.0).
    scale: f64,
}

impl GelReader {
    pub fn new() -> Self {
        GelReader {
            inner: crate::tiff::TiffReader::new(),
            meta: None,
            square_root: false,
            scale: 1.0,
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
        // A genuine GEL is a TIFF whose first IFD contains MD_FILETAG, which we
        // cannot test from a raw header slice alone, so defer to set_id/by_name.
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.inner.set_id(path)?;

        // Inspect the first IFD for the private Molecular Dynamics tags.
        let first = self.inner.ifd(0).ok_or_else(|| {
            BioFormatsError::UnsupportedFormat("GEL TIFF has no IFDs".into())
        })?;
        if first.get(MD_FILETAG).is_none() {
            // Not a Molecular Dynamics GEL TIFF.
            return Err(BioFormatsError::UnsupportedFormat(
                "GEL TIFF is missing the Molecular Dynamics MD_FILETAG (33445) tag".into(),
            ));
        }
        let fmt = first
            .get(MD_FILETAG)
            .and_then(|v| v.as_u64())
            .unwrap_or(128);
        self.square_root = fmt == GEL_SQUARE_ROOT;
        self.scale = first
            .get(MD_SCALE_PIXEL)
            .and_then(|v| match v {
                crate::tiff::ifd::IfdValue::Rational(r) if !r.is_empty() => {
                    let (num, den) = r[0];
                    if den == 0 {
                        Some(1.0)
                    } else {
                        Some(num as f64 / den as f64)
                    }
                }
                _ => None,
            })
            .unwrap_or(1.0);

        // imageCount == number of IFDs; reported as the T dimension (Java
        // GelReader.initMetadata sets sizeT = imageCount, sizeZ/sizeC = 1).
        let mut ifds = 0u32;
        while self.inner.ifd(ifds as usize).is_some() {
            ifds += 1;
        }
        let ifds = ifds.max(1);
        let base = self.inner.metadata();
        let mut meta = base.clone();
        meta.size_z = 1;
        meta.size_c = 1;
        meta.size_t = ifds;
        meta.image_count = ifds;
        meta.dimension_order = DimensionOrder::XYZCT;
        if self.square_root {
            meta.pixel_type = PixelType::Float32;
            meta.bits_per_pixel = 32;
        }
        self.meta = Some(meta);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.meta = None;
        self.square_root = false;
        self.scale = 1.0;
        self.inner.close()
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
        let little_endian = meta.is_little_endian;

        if !self.square_root {
            // LINEAR: plain TIFF pixels.
            return self.inner.open_bytes(plane_index);
        }

        // SQUARE_ROOT: the TIFF holds unsigned-short samples that must be
        // squared and multiplied by the scale, then emitted as 32-bit floats.
        // We read the raw shorts directly (the TIFF reports a 16-bit type for
        // these IFDs) rather than letting any float interpretation occur.
        let raw = self.inner.open_bytes(plane_index)?;
        let n = raw.len() / 2;
        let mut out = vec![0u8; n * 4];
        for i in 0..n {
            let value = if little_endian {
                u16::from_le_bytes([raw[i * 2], raw[i * 2 + 1]])
            } else {
                u16::from_be_bytes([raw[i * 2], raw[i * 2 + 1]])
            } as u64;
            let pixel = (value * value) as f64 * self.scale;
            let bits = (pixel as f32).to_bits();
            let bytes = if little_endian {
                bits.to_le_bytes()
            } else {
                bits.to_be_bytes()
            };
            out[i * 4..i * 4 + 4].copy_from_slice(&bytes);
        }
        Ok(out)
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

/// Cellomics C01 format (`.c01` / `.dib`).
///
/// Ported from the upstream Java `CellomicsReader`. A `.c01` file is a
/// zlib-compressed payload: the first 4 bytes are the C01 magic, and the
/// remainder is a zlib (Deflate-with-header) stream. The decompressed payload
/// is a DIB-style bitmap: at offset 4 the 32-bit width and height (LE), then
/// 16-bit plane count and bit depth, a 32-bit compression code, and pixel data
/// starting at offset 52. A `.dib` file is the same layout but not compressed.
pub struct CellomicsReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    pixel_offset: u64,
    /// Decoded (decompressed for .c01, raw for .dib) file bytes.
    data: Vec<u8>,
}

impl CellomicsReader {
    pub fn new() -> Self {
        CellomicsReader {
            path: None,
            meta: None,
            pixel_offset: 52,
            data: Vec::new(),
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
        let raw = std::fs::read(path).map_err(|e| BioFormatsError::Io(e))?;
        // .c01 files are zlib-compressed after a 4-byte magic; .dib are raw.
        let is_c01 = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("c01"))
            .unwrap_or(false);
        let data = if is_c01 {
            if raw.len() < 4 {
                return Err(BioFormatsError::UnsupportedFormat(
                    "Cellomics C01 file is too short to contain a magic number".into(),
                ));
            }
            crate::common::codec::decompress_deflate(&raw[4..]).map_err(|_| {
                BioFormatsError::UnsupportedFormat(
                    "Cellomics C01 zlib payload could not be decompressed".into(),
                )
            })?
        } else {
            raw
        };

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
        self.data = data;
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
        self.data = Vec::new();
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
        // Pixel data lives in the decoded (decompressed for .c01) buffer.
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
            })? as usize;
        if plane_offset + n_bytes > self.data.len() {
            return Err(BioFormatsError::InvalidData(
                "Cellomics plane extends beyond decoded payload".to_string(),
            ));
        }
        Ok(self.data[plane_offset..plane_offset + n_bytes].to_vec())
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
/// Minolta MRW (Minolta RAW) reader (`.mrw`).
///
/// Ported from the upstream Java `MRWReader`. An MRW file is **not** a TIFF; it
/// is a block-structured binary file. After a 4-byte magic ("\0MRM"), a 32-bit
/// big-endian length gives the offset to the Bayer pixel data (`length + 8`).
/// Between the magic and the data, named 4-character blocks describe the image:
///   - `PRD`: sensor and output dimensions, the per-sample bit depth and the
///     Bayer pattern.
///   - `WBG`: white-balance gains.
///   - `TTW`: an embedded TIFF block of EXIF-style metadata.
///
/// The pixel data is a single-channel Bayer mosaic that the Java reader
/// demosaics (via `ImageTools.interpolate`) into an interleaved RGB UINT16
/// big-endian plane. The demosaic is a substantial RAW operation and is **not**
/// reproduced here: this port faithfully parses the header so the format is
/// recognised and its metadata is correct, but pixel reads return an
/// UnsupportedFormat error.
pub struct MrwReader {
    meta: Option<ImageMetadata>,
    sensor_width: u32,
    sensor_height: u32,
    bayer_pattern: u8,
    data_size: u8,
    pixel_offset: u64,
    wbg: [f32; 4],
}

impl MrwReader {
    pub fn new() -> Self {
        MrwReader {
            meta: None,
            sensor_width: 0,
            sensor_height: 0,
            bayer_pattern: 0,
            data_size: 0,
            pixel_offset: 0,
            wbg: [1.0; 4],
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
    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // First 4 bytes end with the "MRM" magic string.
        header.len() >= 4 && &header[1..4] == b"MRM"
    }
    fn set_id(&mut self, path: &Path) -> Result<()> {
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        if data.len() < 8 || &data[1..4] != b"MRM" {
            return Err(BioFormatsError::UnsupportedFormat(
                "MRW: missing 'MRM' magic string".into(),
            ));
        }
        // Big-endian throughout. offset = readInt(@4) + 8.
        let be_i32 = |o: usize| -> i64 {
            i32::from_be_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]]) as i64
        };
        let be_i16 = |o: usize| -> i16 { i16::from_be_bytes([data[o], data[o + 1]]) };

        let offset = (be_i32(4) + 8) as u64;
        let mut size_x = 0u32;
        let mut size_y = 0u32;
        let mut sensor_w = 0u32;
        let mut sensor_h = 0u32;
        let mut data_size = 0u8;
        let mut bayer = 0u8;
        let mut wbg = [1.0f32; 4];

        let mut fp = 8usize;
        while (fp as u64) < offset && fp + 8 <= data.len() {
            let block_name = &data[fp..fp + 4];
            let len = i32::from_be_bytes([data[fp + 4], data[fp + 5], data[fp + 6], data[fp + 7]])
                .max(0) as usize;
            let body = fp + 8;
            if block_name.ends_with(b"PRD") {
                // skip 8, sensorHeight(short), sensorWidth(short),
                // sizeY(short), sizeX(short), dataSize(byte), skip 1,
                // storageMethod(byte), skip 4, bayerPattern(byte)
                if body + 17 <= data.len() {
                    sensor_h = be_i16(body + 8) as u16 as u32;
                    sensor_w = be_i16(body + 10) as u16 as u32;
                    size_y = be_i16(body + 12) as u16 as u32;
                    size_x = be_i16(body + 14) as u16 as u32;
                    data_size = data[body + 16];
                    // body+17 skip, body+18 storageMethod, body+19..23 skip,
                    // body+23 bayerPattern
                    if body + 24 <= data.len() {
                        bayer = data[body + 23];
                    }
                }
            } else if block_name.ends_with(b"WBG") {
                // 4-byte scale array, then 4 big-endian shorts: coeff/(64<<scale)
                if body + 12 <= data.len() {
                    let scale = &data[body..body + 4];
                    for i in 0..4 {
                        let coeff = be_i16(body + 4 + i * 2) as f32;
                        wbg[i] = coeff / ((64u32 << scale[i]) as f32);
                    }
                }
            }
            // TTW block (embedded TIFF metadata) is parsed by Java for global
            // metadata only; not required for pixel layout.
            fp = body + len;
        }

        if size_x == 0 || size_y == 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "MRW: PRD block did not yield image dimensions".into(),
            ));
        }

        self.sensor_width = sensor_w;
        self.sensor_height = sensor_h;
        self.bayer_pattern = bayer;
        self.data_size = data_size;
        self.pixel_offset = offset;
        self.wbg = wbg;

        // Java: RGB UINT16, big-endian, interleaved, dimensionOrder XYCZT,
        // sizeC = 3, sizeZ = sizeT = 1, imageCount = 1, bitsPerPixel = dataSize.
        self.meta = Some(ImageMetadata {
            size_x,
            size_y,
            size_z: 1,
            size_c: 3,
            size_t: 1,
            pixel_type: PixelType::Uint16,
            bits_per_pixel: if data_size > 0 { data_size } else { 16 },
            image_count: 1,
            dimension_order: DimensionOrder::XYCZT,
            is_rgb: true,
            is_interleaved: true,
            is_indexed: false,
            is_little_endian: false,
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
        self.meta = None;
        self.sensor_width = 0;
        self.sensor_height = 0;
        self.bayer_pattern = 0;
        self.data_size = 0;
        self.pixel_offset = 0;
        self.wbg = [1.0; 4];
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
    fn open_bytes(&mut self, _p: u32) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "MRW: Bayer CFA demosaicing of raw sensor data is not implemented".into(),
        ))
    }
    fn open_bytes_region(&mut self, p: u32, _x: u32, _y: u32, _w: u32, _h: u32) -> Result<Vec<u8>> {
        self.open_bytes(p)
    }
    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        self.open_bytes(p)
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
// 8. Yokogawa CV7000/8000 HCS — XML index + TIFF images
// ---------------------------------------------------------------------------
/// Yokogawa CV7000/8000 HCS reader (`.wpi`).
///
/// Ported from the upstream Java `CV7000Reader`. A CV7000 acquisition is a
/// directory of single-plane TIFFs indexed by three XML files:
///   - `*.wpi`         — the entry point; describes the well plate (rows/cols).
///   - `MeasurementData.mlf`   — one `bts:MeasurementRecord` per acquired image,
///     giving its well row/column, field, Z, channel, timepoint and the TIFF
///     filename.
///   - `MeasurementDetail.mrf` — the acquired channels (pixel sizes etc.).
///
/// Each (well, field) combination becomes a series; planes within a series are
/// addressed in XYCZT order. `open_bytes` delegates to the per-plane TIFF.
pub struct YokogawaReader {
    inner: crate::tiff::TiffReader,
    tiff_loaded: bool,
    series: Vec<ImageMetadata>,
    current_series: usize,
    /// For each series, plane_index -> TIFF file (None for missing planes).
    plane_files: Vec<Vec<Option<PathBuf>>>,
    plate: YokogawaPlate,
    /// For each series: (well_ordinal, field) for OME mapping.
    series_well_field: Vec<(usize, usize)>,
    /// Populated wells in raster order, each (row, col).
    wells: Vec<(u32, u32)>,
    fields: usize,
    /// Physical pixel size (X, Y) in micrometres from the first channel, if any.
    physical_size_x: Option<f64>,
    physical_size_y: Option<f64>,
}

#[derive(Default, Clone)]
struct YokogawaPlate {
    name: Option<String>,
    rows: u32,
    columns: u32,
}

#[derive(Clone)]
struct YokogawaPlane {
    row: u32,
    column: u32,
    field: u32,
    z: i32,
    channel: i32,
    timepoint: i32,
    action_index: i32,
    timeline_index: i32,
    file: Option<PathBuf>,
}

#[derive(Clone, Default)]
struct YokogawaChannel {
    index: i32,
    action_index: i32,
    timeline_index: i32,
    x_size: Option<f64>,
    y_size: Option<f64>,
}

impl YokogawaReader {
    pub fn new() -> Self {
        YokogawaReader {
            inner: crate::tiff::TiffReader::new(),
            tiff_loaded: false,
            series: Vec::new(),
            current_series: 0,
            plane_files: Vec::new(),
            plate: YokogawaPlate::default(),
            series_well_field: Vec::new(),
            wells: Vec::new(),
            fields: 1,
            physical_size_x: None,
            physical_size_y: None,
        }
    }
}

impl Default for YokogawaReader {
    fn default() -> Self {
        Self::new()
    }
}

/// Read every `name="value"` style attribute from the start tag and return the
/// requested one. `bts:` prefixes are preserved by quick_xml.
fn yk_attr(e: &quick_xml::events::BytesStart, name: &str) -> Option<String> {
    for a in e.attributes().flatten() {
        if a.key.as_ref() == name.as_bytes() {
            return Some(String::from_utf8_lossy(&a.value).to_string());
        }
    }
    None
}

fn yk_attr_int(e: &quick_xml::events::BytesStart, name: &str) -> Option<i64> {
    yk_attr(e, name).and_then(|s| s.trim().parse::<i64>().ok())
}

fn yk_attr_f64(e: &quick_xml::events::BytesStart, name: &str) -> Option<f64> {
    yk_attr(e, name).and_then(|s| s.trim().parse::<f64>().ok())
}

/// Read a file and strip a stray trailing '>' (mirrors readSanitizedXML).
fn yk_read_sanitized(path: &Path) -> Result<String> {
    let mut s = std::fs::read_to_string(path).map_err(BioFormatsError::Io)?;
    let trimmed = s.trim_end();
    if trimmed.ends_with(">>") {
        s = trimmed[..trimmed.len() - 1].to_string();
    } else {
        s = trimmed.to_string();
    }
    Ok(s)
}

fn yk_parse_wpi(xml: &str) -> YokogawaPlate {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(false);
    let mut plate = YokogawaPlate::default();
    loop {
        match reader.read_event() {
            Ok(Event::Eof) | Err(_) => break,
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                if e.name().as_ref() == b"bts:WellPlate" {
                    plate.name = yk_attr(e, "bts:Name");
                    plate.rows = yk_attr_int(e, "bts:Rows").unwrap_or(0) as u32;
                    plate.columns = yk_attr_int(e, "bts:Columns").unwrap_or(0) as u32;
                }
            }
            _ => {}
        }
    }
    plate
}

fn yk_parse_mlf(xml: &str, parent: &Path) -> Vec<YokogawaPlane> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(false);
    let mut planes: Vec<YokogawaPlane> = Vec::new();
    let mut current_text = String::new();
    let mut in_img_record = false;
    loop {
        match reader.read_event() {
            Ok(Event::Eof) | Err(_) => break,
            Ok(Event::Start(ref e)) => {
                if e.name().as_ref() == b"bts:MeasurementRecord" {
                    current_text.clear();
                    let bts_type = yk_attr(e, "bts:Type").unwrap_or_default();
                    if bts_type == "IMG" {
                        in_img_record = true;
                        // attributes are 1-based in the file; convert to 0-based.
                        let p = YokogawaPlane {
                            row: (yk_attr_int(e, "bts:Row").unwrap_or(1) - 1).max(0) as u32,
                            column: (yk_attr_int(e, "bts:Column").unwrap_or(1) - 1).max(0) as u32,
                            field: (yk_attr_int(e, "bts:FieldIndex").unwrap_or(1) - 1).max(0) as u32,
                            z: (yk_attr_int(e, "bts:ZIndex").unwrap_or(1) - 1) as i32,
                            channel: (yk_attr_int(e, "bts:Ch").unwrap_or(1) - 1) as i32,
                            timepoint: (yk_attr_int(e, "bts:TimePoint").unwrap_or(1) - 1) as i32,
                            action_index: (yk_attr_int(e, "bts:ActionIndex").unwrap_or(1) - 1)
                                as i32,
                            timeline_index: (yk_attr_int(e, "bts:TimelineIndex").unwrap_or(1) - 1)
                                as i32,
                            file: None,
                        };
                        planes.push(p);
                    } else {
                        in_img_record = false;
                    }
                }
            }
            Ok(Event::Text(t)) => {
                if in_img_record {
                    current_text.push_str(&t.unescape().unwrap_or_default());
                }
            }
            Ok(Event::End(ref e)) => {
                if e.name().as_ref() == b"bts:MeasurementRecord" && in_img_record {
                    let value = current_text.trim();
                    if !value.is_empty() {
                        let img = parent.join(value);
                        if img.exists() {
                            if let Some(last) = planes.last_mut() {
                                last.file = Some(img);
                            }
                        }
                    }
                    in_img_record = false;
                    current_text.clear();
                }
            }
            _ => {}
        }
    }
    planes
}

fn yk_parse_mrf(xml: &str) -> Vec<YokogawaChannel> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(false);
    let mut channels: Vec<YokogawaChannel> = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Eof) | Err(_) => break,
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                if e.name().as_ref() == b"bts:MeasurementChannel" {
                    channels.push(YokogawaChannel {
                        index: (yk_attr_int(e, "bts:Ch").unwrap_or(1) - 1) as i32,
                        action_index: 0,
                        timeline_index: 0,
                        x_size: yk_attr_f64(e, "bts:HorizontalPixelDimension"),
                        y_size: yk_attr_f64(e, "bts:VerticalPixelDimension"),
                    });
                }
            }
            _ => {}
        }
    }
    channels
}

impl YokogawaReader {
    fn build(&mut self, wpi_path: &Path) -> Result<()> {
        let parent = wpi_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));

        let plate = yk_parse_wpi(&yk_read_sanitized(wpi_path)?);

        // MeasurementData.mlf is required.
        let mlf_path = parent.join("MeasurementData.mlf");
        if !mlf_path.exists() {
            return Err(BioFormatsError::UnsupportedFormat(
                "Yokogawa CV7000: missing MeasurementData.mlf index file".into(),
            ));
        }
        let planes = yk_parse_mlf(&yk_read_sanitized(&mlf_path)?, &parent);

        // MeasurementDetail.mrf is optional (channels / pixel sizes).
        let mrf_path = parent.join("MeasurementDetail.mrf");
        let channels = if mrf_path.exists() {
            yk_parse_mrf(&yk_read_sanitized(&mrf_path)?)
        } else {
            Vec::new()
        };

        let plate_columns = plate.columns.max(1);

        // Determine acquired wells, fields, channels and per-well Z/T ranges.
        use std::collections::{HashMap, HashSet};
        let mut unique_wells: HashSet<u32> = HashSet::new();
        let mut unique_channels: HashSet<i32> = HashSet::new();
        let mut fields = 0usize;
        // per-well min/max Z and T
        let mut zmin: HashMap<u32, i32> = HashMap::new();
        let mut zmax: HashMap<u32, i32> = HashMap::new();
        let mut tmin: HashMap<u32, i32> = HashMap::new();
        let mut tmax: HashMap<u32, i32> = HashMap::new();
        let mut first_file: Option<PathBuf> = None;

        for p in &planes {
            if p.file.is_none() {
                continue;
            }
            if first_file.is_none() {
                first_file = p.file.clone();
            }
            let well_number = p.row * plate_columns + p.column;
            unique_wells.insert(well_number);
            unique_channels.insert(p.channel);
            if (p.field as usize) + 1 > fields {
                fields = p.field as usize + 1;
            }
            *zmin.entry(well_number).or_insert(i32::MAX) =
                (*zmin.get(&well_number).unwrap_or(&i32::MAX)).min(p.z);
            *zmax.entry(well_number).or_insert(i32::MIN) =
                (*zmax.get(&well_number).unwrap_or(&i32::MIN)).max(p.z);
            *tmin.entry(well_number).or_insert(i32::MAX) =
                (*tmin.get(&well_number).unwrap_or(&i32::MAX)).min(p.timepoint);
            *tmax.entry(well_number).or_insert(i32::MIN) =
                (*tmax.get(&well_number).unwrap_or(&i32::MIN)).max(p.timepoint);
        }
        let fields = fields.max(1);

        let first_file = first_file.ok_or_else(|| {
            BioFormatsError::UnsupportedFormat(
                "Yokogawa CV7000: MeasurementData.mlf referenced no existing image files".into(),
            )
        })?;

        // Probe the first TIFF for pixel parameters.
        let mut probe = crate::tiff::TiffReader::new();
        probe.set_id(&first_file)?;
        let pm = probe.metadata().clone();
        let _ = probe.close();
        let tiff_c = pm.size_c.max(1);

        // Sorted unique wells and channels.
        let mut wells: Vec<u32> = unique_wells.into_iter().collect();
        wells.sort_unstable();
        let mut channel_indexes: Vec<i32> = unique_channels.into_iter().collect();
        channel_indexes.sort_unstable();
        let n_channels = channel_indexes.len().max(1) as u32;

        let real_wells = wells.len();
        let series_count = real_wells * fields;

        // Build per-series metadata and plane-file lookup.
        let mut series = Vec::with_capacity(series_count);
        let mut plane_files: Vec<Vec<Option<PathBuf>>> = Vec::with_capacity(series_count);
        let mut series_well_field = Vec::with_capacity(series_count);
        for s in 0..series_count {
            let well_ordinal = s / fields;
            let field = s % fields;
            let well_number = wells[well_ordinal];
            let size_z = (zmax.get(&well_number).copied().unwrap_or(0)
                - zmin.get(&well_number).copied().unwrap_or(0)
                + 1)
            .max(1) as u32;
            let size_t = (tmax.get(&well_number).copied().unwrap_or(0)
                - tmin.get(&well_number).copied().unwrap_or(0)
                + 1)
            .max(1) as u32;
            let size_c = tiff_c * n_channels;
            let planes_per_series = (size_z * size_t * n_channels) as usize;

            let mut meta = pm.clone();
            meta.size_z = size_z;
            meta.size_t = size_t;
            meta.size_c = size_c;
            meta.image_count = size_z * size_t * n_channels;
            meta.dimension_order = DimensionOrder::XYCZT;
            meta.series_metadata
                .insert("format".into(), crate::common::metadata::MetadataValue::String("Yokogawa CV7000".into()));
            series.push(meta);
            plane_files.push(vec![None; planes_per_series]);
            series_well_field.push((well_ordinal, field));
        }

        // Map each plane record into (series, no) and record its file.
        for p in &planes {
            let Some(_) = p.file.as_ref() else { continue };
            let well_number = p.row * plate_columns + p.column;
            let Ok(well_ordinal) = wells.binary_search(&well_number) else {
                continue;
            };
            if (p.field as usize) >= fields {
                continue;
            }
            let series_index = well_ordinal * fields + p.field as usize;
            if series_index >= series_count {
                continue;
            }
            // channel index into the unique acquired channels
            let channel_index = channel_indexes
                .binary_search(&yk_channel_index(p, &channels))
                .unwrap_or(0);
            let m = &series[series_index];
            let plane_c = (m.size_c / tiff_c).max(1);
            let plane_z = m.size_z.max(1);
            let z = (p.z - zmin.get(&well_number).copied().unwrap_or(0)).max(0) as u32;
            let t = (p.timepoint - tmin.get(&well_number).copied().unwrap_or(0)).max(0) as u32;
            // positionToRaster([C, Z, T], [channel, z, t]) for XYCZT order:
            // index = channel + plane_c*(z + plane_z*t)
            let no = channel_index as u32 + plane_c * (z + plane_z * t);
            if let Some(slot) = plane_files
                .get_mut(series_index)
                .and_then(|v| v.get_mut(no as usize))
            {
                if slot.is_none() {
                    *slot = p.file.clone();
                }
            }
        }

        self.series = series;
        self.plane_files = plane_files;
        self.plate = plate;
        self.series_well_field = series_well_field;
        self.wells = wells.iter().map(|&w| (w / plate_columns, w % plate_columns)).collect();
        self.fields = fields;
        self.physical_size_x = channels.first().and_then(|c| c.x_size).filter(|&v| v > 0.0);
        self.physical_size_y = channels.first().and_then(|c| c.y_size).filter(|&v| v > 0.0);
        self.current_series = 0;
        Ok(())
    }
}

/// Compute the channel index of a plane within the list of acquired channels,
/// mirroring CV7000Reader.getChannelIndex (simplified: when channel metadata is
/// missing, fall back to the raw channel number).
fn yk_channel_index(p: &YokogawaPlane, channels: &[YokogawaChannel]) -> i32 {
    if channels.is_empty() {
        return p.channel;
    }
    let mut index = -1i32;
    for action in 0..=p.action_index {
        for ch in channels {
            if ch.timeline_index == p.timeline_index && ch.action_index == action {
                index += 1;
                if ch.index == p.channel && ch.action_index == p.action_index {
                    return index;
                }
            }
        }
    }
    if index < 0 {
        p.channel
    } else {
        index
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
        self.build(path)?;
        if self.series.is_empty() {
            return Err(BioFormatsError::UnsupportedFormat(
                "Yokogawa CV7000: no series could be assembled from the index files".into(),
            ));
        }
        self.tiff_loaded = false;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.series.clear();
        self.plane_files.clear();
        self.series_well_field.clear();
        self.wells.clear();
        self.plate = YokogawaPlate::default();
        self.fields = 1;
        self.physical_size_x = None;
        self.physical_size_y = None;
        self.current_series = 0;
        if self.tiff_loaded {
            let _ = self.inner.close();
            self.tiff_loaded = false;
        }
        Ok(())
    }
    fn series_count(&self) -> usize {
        self.series.len().max(1)
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if s >= self.series_count() {
            Err(BioFormatsError::SeriesOutOfRange(s))
        } else {
            self.current_series = s;
            Ok(())
        }
    }
    fn series(&self) -> usize {
        self.current_series
    }
    fn metadata(&self) -> &ImageMetadata {
        self.series
            .get(self.current_series)
            .expect("set_id not called")
    }
    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        let meta = self
            .series
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        if p >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(p));
        }
        let plane_bytes =
            meta.size_x as usize * meta.size_y as usize * meta.pixel_type.bytes_per_sample();
        let file = self
            .plane_files
            .get(self.current_series)
            .and_then(|v| v.get(p as usize))
            .cloned()
            .flatten();
        let Some(file) = file else {
            // Missing plane: return zero-filled buffer (Java fills with 0).
            return Ok(vec![0u8; plane_bytes]);
        };
        if self.tiff_loaded {
            let _ = self.inner.close();
        }
        self.inner.set_id(&file)?;
        self.tiff_loaded = true;
        self.inner.open_bytes(0)
    }
    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let full = self.open_bytes(p)?;
        let meta = self
            .series
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?
            .clone();
        crop_full_plane("Yokogawa CV7000", &full, &meta, 1, x, y, w, h)
    }
    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        let meta = self
            .series
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(p, tx, ty, tw, th)
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

    /// Build OME HCS metadata: one Plate with Wells/WellSamples mapping each
    /// (well, field) series to an Image. Mirrors CV7000Reader's MetadataStore.
    fn ome_metadata(&self) -> Option<OmeMetadata> {
        if self.series.is_empty() {
            return None;
        }
        let mut images = Vec::with_capacity(self.series.len());
        for (s, (well_ordinal, field)) in self.series_well_field.iter().enumerate() {
            let (row, col) = self.wells.get(*well_ordinal).copied().unwrap_or((0, 0));
            let name = format!(
                "Well {}{}, Field {}",
                yk_row_name(row),
                col + 1,
                field + 1
            );
            let _ = s;
            images.push(OmeImage {
                name: Some(name),
                physical_size_x: self.physical_size_x,
                physical_size_y: self.physical_size_y,
                ..Default::default()
            });
        }

        let mut wells = Vec::with_capacity(self.wells.len());
        for (well_ordinal, &(row, col)) in self.wells.iter().enumerate() {
            let mut well_samples = Vec::with_capacity(self.fields);
            for field in 0..self.fields {
                let series = well_ordinal * self.fields + field;
                if series >= self.series.len() {
                    continue;
                }
                well_samples.push(OmeWellSample {
                    id: Some(create_lsid("WellSample", &[0, well_ordinal, field])),
                    index: series as u32,
                    image_ref: Some(series),
                    position_x: None,
                    position_y: None,
                });
            }
            wells.push(OmeWell {
                id: Some(create_lsid("Well", &[0, well_ordinal])),
                row,
                column: col,
                well_samples,
            });
        }

        let plate = OmePlate {
            id: Some(create_lsid("Plate", &[0])),
            name: self.plate.name.clone(),
            rows: self.plate.rows,
            columns: self.plate.columns,
            wells,
        };

        Some(OmeMetadata {
            images,
            plates: vec![plate],
            ..Default::default()
        })
    }
}

/// Well row letter (0 -> "A", 25 -> "Z", 26 -> "AA", ...).
fn yk_row_name(row: u32) -> String {
    let mut n = row as i64;
    let mut s = String::new();
    loop {
        let rem = (n % 26) as u8;
        s.insert(0, (b'A' + rem) as char);
        n = n / 26 - 1;
        if n < 0 {
            break;
        }
    }
    s
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
