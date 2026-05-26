//! Scanning Probe Microscopy (SPM) and related format readers.
//!
//! Includes a real binary reader for PicoQuant TCSPC data and
//! extension-only placeholder readers for various SPM/AFM platforms.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;

// ---------------------------------------------------------------------------
// Macro: extension-only placeholder reader (512x512 uint16)
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
// Binary reader — PicoQuant TCSPC / FLIM
// ===========================================================================

/// PicoQuant PTU/PQRES time-correlated single-photon counting format.
///
/// Magic: first 6 bytes == `PQTTTR`. Image dimensions parsed from text header.
pub struct PicoQuantReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl PicoQuantReader {
    pub fn new() -> Self {
        PicoQuantReader {
            path: None,
            meta: None,
        }
    }
}

impl Default for PicoQuantReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for PicoQuantReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("ptu") | Some("pqres"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.len() >= 6 && &header[0..6] == b"PQTTTR"
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;

        // Read first 4096 bytes as lossy string for header parsing
        let header_bytes = &data[..data.len().min(4096)];
        let text = String::from_utf8_lossy(header_bytes).into_owned();

        let mut width: u32 = 64;
        let mut height: u32 = 64;
        let mut size_z: u32 = 1;

        for line in text.lines() {
            if let Some(val) = line.strip_prefix("ImgHdr_Pixels=") {
                if let Ok(n) = val.trim().parse::<u32>() {
                    width = n;
                }
            } else if let Some(val) = line.strip_prefix("ImgHdr_Lines=") {
                if let Ok(n) = val.trim().parse::<u32>() {
                    height = n;
                }
            } else if let Some(val) = line.strip_prefix("ImgHdr_Frame=") {
                if let Ok(n) = val.trim().parse::<u32>() {
                    size_z = n;
                }
            }
        }

        let _ = (width, height, size_z);
        Err(BioFormatsError::UnsupportedFormat(
            "PicoQuant TCSPC event stream decoding to image planes is not implemented".into(),
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
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        Err(BioFormatsError::UnsupportedFormat(
            "PicoQuant TCSPC event stream decoding to image planes is not implemented".into(),
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
            "PicoQuant TCSPC event stream decoding to image planes is not implemented".into(),
        ))
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
// Helper: compute square dimensions from file size assuming uint16
// ===========================================================================

/// Given a file size and a data offset, compute square dimensions assuming
/// uint16 (2 bytes per pixel). Returns (width, height).
fn unsupported_raw_spm(format_name: &str) -> BioFormatsError {
    BioFormatsError::UnsupportedFormat(format!(
        "{format_name} binary layout is not implemented; refusing heuristic dimensions"
    ))
}

// ===========================================================================
// Real binary reader — RHK Technology SPM
// ===========================================================================

/// RHK Technology SPM reader (`.sm2`, `.sm3`, `.sm4`).
///
/// Attempts to parse a text header looking for dimension info. Falls back
/// to raw uint16 square heuristic.
pub struct RhkReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    data_offset: u64,
}

impl RhkReader {
    pub fn new() -> Self {
        RhkReader {
            path: None,
            meta: None,
            data_offset: 0,
        }
    }
}

impl Default for RhkReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for RhkReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("sm2") | Some("sm3") | Some("sm4"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let file_size = std::fs::metadata(path).map_err(BioFormatsError::Io)?.len();

        // Read first 1024 bytes and try to parse as text header
        let read_len = (file_size as usize).min(1024);
        let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
        let mut hdr_buf = vec![0u8; read_len];
        f.read_exact(&mut hdr_buf).map_err(BioFormatsError::Io)?;

        let text = String::from_utf8_lossy(&hdr_buf);

        let mut width: u32 = 0;
        let mut height: u32 = 0;
        let mut data_off: u64 = 0;

        // RHK SM2/SM3 may have text header lines with key=value pairs
        for line in text.lines() {
            let line_lower = line.to_ascii_lowercase();
            if line_lower.contains("x_size")
                || line_lower.contains("xsize")
                || line_lower.contains("x size")
                || line_lower.contains("columns")
            {
                if let Some(val) = line.split_whitespace().last() {
                    if let Ok(w) = val.parse::<u32>() {
                        width = w;
                    }
                }
            } else if line_lower.contains("y_size")
                || line_lower.contains("ysize")
                || line_lower.contains("y size")
                || line_lower.contains("rows")
            {
                if let Some(val) = line.split_whitespace().last() {
                    if let Ok(h) = val.parse::<u32>() {
                        height = h;
                    }
                }
            } else if line_lower.contains("data offset") || line_lower.contains("header size") {
                if let Some(val) = line.split_whitespace().last() {
                    if let Ok(off) = val.parse::<u64>() {
                        data_off = off;
                    }
                }
            }
        }

        if width == 0 || height == 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "RHK SPM header missing image dimensions".into(),
            ));
        }
        let expected = data_off
            .checked_add(
                (width as u64)
                    .saturating_mul(height as u64)
                    .saturating_mul(2),
            )
            .ok_or_else(|| BioFormatsError::Format("RHK SPM plane size overflows".into()))?;
        if expected > file_size {
            return Err(BioFormatsError::UnsupportedFormat(
                "RHK SPM pixel payload is shorter than declared dimensions".into(),
            ));
        }

        self.data_offset = data_off;
        self.path = Some(path.to_path_buf());
        self.meta = Some(ImageMetadata {
            size_x: width,
            size_y: height,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type: PixelType::Uint16,
            bits_per_pixel: 16,
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
        f.seek(SeekFrom::Start(self.data_offset))
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
        crop_full_plane("RHK SPM", &full, &meta, 1, _x, _y, w, h)
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
// Raw binary reader — Quesant AFM
// ===========================================================================

/// Quesant AFM reader (`.afm`).
///
/// Binary header then raw data. Falls back to raw uint16 square heuristic.
pub struct QuesantReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    data_offset: u64,
}

impl QuesantReader {
    pub fn new() -> Self {
        QuesantReader {
            path: None,
            meta: None,
            data_offset: 0,
        }
    }
}

impl Default for QuesantReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for QuesantReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        // Note: .afm is also used by VeecoReader (Nanoscope). Quesant AFM
        // files lack the NANOSCOPE header, so this reader is a fallback.
        matches!(ext.as_deref(), Some("afm"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let _ = path;
        Err(unsupported_raw_spm("Quesant AFM"))
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
        Err(unsupported_raw_spm("Quesant AFM"))
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
        Err(unsupported_raw_spm("Quesant AFM"))
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
// ZIP+TIFF reader — JPK Instruments AFM
// ===========================================================================

/// JPK Instruments AFM reader (`.jpk`).
///
/// JPK files are ZIP archives containing TIFF images. Opens the ZIP, finds
/// the first TIFF, extracts it to a temp file, and delegates to TiffReader.
pub struct JpkReader {
    extracted_path: Option<PathBuf>,
    inner: crate::tiff::TiffReader,
    meta: Option<ImageMetadata>,
    is_tiff: bool,
}

impl JpkReader {
    pub fn new() -> Self {
        JpkReader {
            extracted_path: None,
            inner: crate::tiff::TiffReader::new(),
            meta: None,
            is_tiff: false,
        }
    }
}

impl Default for JpkReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for JpkReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("jpk"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let file = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
        let mut archive = zip::ZipArchive::new(file)
            .map_err(|e| BioFormatsError::Format(format!("JPK ZIP open error: {e}")))?;

        // Find the first TIFF entry
        let mut found_name: Option<String> = None;
        for i in 0..archive.len() {
            if let Ok(entry) = archive.by_index(i) {
                let name = entry.name().to_string();
                if !entry.is_dir() {
                    let lower = name.to_ascii_lowercase();
                    if lower.ends_with(".tif") || lower.ends_with(".tiff") {
                        found_name = Some(name);
                        break;
                    }
                }
            }
        }

        let Some(name) = found_name else {
            return Err(BioFormatsError::UnsupportedFormat(
                "JPK ZIP archive does not contain a TIFF image entry".to_string(),
            ));
        };

        // Extract to temp file
        let safe_name = std::path::Path::new(&name)
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("extracted.tif"))
            .to_string_lossy()
            .to_string();
        let unique = format!("bioformats_jpk_{}_{}", std::process::id(), safe_name);
        let temp_path = std::env::temp_dir().join(unique);

        {
            let mut entry = archive
                .by_name(&name)
                .map_err(|e| BioFormatsError::Format(format!("JPK ZIP entry error: {e}")))?;
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf).map_err(BioFormatsError::Io)?;
            std::fs::write(&temp_path, &buf).map_err(BioFormatsError::Io)?;
        }

        self.extracted_path = Some(temp_path.clone());
        self.inner.set_id(&temp_path)?;
        self.meta = Some(self.inner.metadata().clone());
        self.is_tiff = true;

        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        if self.is_tiff {
            let _ = self.inner.close();
        }
        if let Some(p) = self.extracted_path.take() {
            let _ = std::fs::remove_file(p);
        }
        self.meta = None;
        self.is_tiff = false;
        Ok(())
    }

    fn series_count(&self) -> usize {
        if self.is_tiff {
            self.inner.series_count()
        } else {
            1
        }
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.is_tiff {
            self.inner.set_series(s)
        } else if s != 0 {
            Err(BioFormatsError::SeriesOutOfRange(s))
        } else {
            Ok(())
        }
    }

    fn series(&self) -> usize {
        if self.is_tiff {
            self.inner.series()
        } else {
            0
        }
    }

    fn metadata(&self) -> &ImageMetadata {
        if self.is_tiff {
            self.inner.metadata()
        } else {
            self.meta.as_ref().expect("set_id not called")
        }
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        if self.is_tiff {
            return self.inner.open_bytes(plane_index);
        }
        let _ = plane_index;
        Err(BioFormatsError::UnsupportedFormat(
            "JPK ZIP archive does not contain delegated TIFF data".to_string(),
        ))
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        if self.is_tiff {
            return self.inner.open_bytes_region(plane_index, x, y, w, h);
        }
        let _ = (plane_index, x, y, w, h);
        Err(BioFormatsError::UnsupportedFormat(
            "JPK ZIP archive does not contain delegated TIFF data".to_string(),
        ))
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        if self.is_tiff {
            return self.inner.open_thumb_bytes(plane_index);
        }
        let _ = plane_index;
        Err(BioFormatsError::UnsupportedFormat(
            "JPK ZIP archive does not contain delegated TIFF data".to_string(),
        ))
    }

    fn resolution_count(&self) -> usize {
        if self.is_tiff {
            self.inner.resolution_count()
        } else {
            1
        }
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if self.is_tiff {
            self.inner.set_resolution(level)
        } else if level != 0 {
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
// Raw binary reader — WaTom SPM
// ===========================================================================

/// WA Technology TOP reader (`.wat`, plus legacy aliases).
///
/// Java Bio-Formats uses a 4864-byte little-endian header followed by raw
/// signed 16-bit pixels.
pub struct WatopReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl WatopReader {
    const HEADER_SIZE: usize = 4864;
    const MAGIC: &'static [u8] = b"0TOPSystem W.A.Technology";

    pub fn new() -> Self {
        WatopReader {
            path: None,
            meta: None,
        }
    }

    fn read_i32_le(data: &[u8], offset: usize, label: &str) -> Result<i32> {
        let bytes = data.get(offset..offset + 4).ok_or_else(|| {
            BioFormatsError::UnsupportedFormat(format!("WA Technology TOP header missing {label}"))
        })?;
        Ok(i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }
}

impl Default for WatopReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for WatopReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(
            ext.as_deref(),
            Some("wat") | Some("wap") | Some("opo") | Some("opz") | Some("opt")
        )
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.starts_with(Self::MAGIC)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        if data.len() < Self::HEADER_SIZE {
            return Err(BioFormatsError::UnsupportedFormat(
                "WA Technology TOP file is shorter than the 4864-byte header".into(),
            ));
        }
        if !data.starts_with(Self::MAGIC) {
            return Err(BioFormatsError::UnsupportedFormat(
                "WA Technology TOP file is missing 0TOPSystem W.A.Technology magic".into(),
            ));
        }

        let width = Self::read_i32_le(&data, 259, "width")?;
        let height = Self::read_i32_le(&data, 263, "height")?;
        if width <= 0 || height <= 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "WA Technology TOP header contains invalid image dimensions".into(),
            ));
        }
        let width = width as u32;
        let height = height as u32;
        let expected = (Self::HEADER_SIZE as u64)
            .checked_add(width as u64 * height as u64 * 2)
            .ok_or_else(|| BioFormatsError::Format("WA Technology TOP size overflows".into()))?;
        let file_len = data.len() as u64;
        if file_len < expected {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "WA Technology TOP pixel payload is shorter than declared dimensions {width}x{height}"
            )));
        }

        let comment_bytes = data.get(49..82).unwrap_or(&[]);
        let comment = String::from_utf8_lossy(comment_bytes)
            .trim_end_matches('\0')
            .trim()
            .to_string();
        let mut series_metadata = HashMap::new();
        if !comment.is_empty() {
            series_metadata.insert(
                "Comment".to_string(),
                crate::common::metadata::MetadataValue::String(comment),
            );
        }
        if let Ok(x_size) = Self::read_i32_le(&data, 247, "x size") {
            series_metadata.insert(
                "X size (in um)".to_string(),
                crate::common::metadata::MetadataValue::Float(x_size as f64 / 100.0),
            );
        }
        if let Ok(y_size) = Self::read_i32_le(&data, 251, "y size") {
            series_metadata.insert(
                "Y size (in um)".to_string(),
                crate::common::metadata::MetadataValue::Float(y_size as f64 / 100.0),
            );
        }
        if let Ok(z_size) = Self::read_i32_le(&data, 255, "z size") {
            series_metadata.insert(
                "Z size (in um)".to_string(),
                crate::common::metadata::MetadataValue::Float(z_size as f64 / 100.0),
            );
        }

        self.path = Some(path.to_path_buf());
        self.meta = Some(ImageMetadata {
            size_x: width,
            size_y: height,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type: PixelType::Int16,
            bits_per_pixel: 16,
            image_count: 1,
            dimension_order: DimensionOrder::XYZCT,
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
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(Self::HEADER_SIZE as u64))
            .map_err(BioFormatsError::Io)?;
        let n_bytes = meta.size_x as usize * meta.size_y as usize * 2;
        let mut buf = vec![0; n_bytes];
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
        crop_full_plane("WA Technology TOP", &full, &meta, 1, _x, _y, w, h)
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
// Raw binary reader — VG SAM
// ===========================================================================

/// VG SAM reader (`.dti`, plus legacy `.vgsam` alias).
///
/// Java Bio-Formats uses `VGS` magic, big-endian dimensions at offsets
/// 348/352, bytes-per-pixel at 360, and pixels at offset 368.
pub struct VgSamReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl VgSamReader {
    const PIXEL_OFFSET: usize = 368;
    const MAGIC: &'static [u8] = b"VGS";

    pub fn new() -> Self {
        VgSamReader {
            path: None,
            meta: None,
        }
    }

    fn read_i32_be(data: &[u8], offset: usize, label: &str) -> Result<i32> {
        let bytes = data.get(offset..offset + 4).ok_or_else(|| {
            BioFormatsError::UnsupportedFormat(format!("VG SAM header missing {label}"))
        })?;
        Ok(i32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn pixel_type_from_bpp(bytes_per_pixel: i32) -> Result<(PixelType, u8)> {
        match bytes_per_pixel {
            1 => Ok((PixelType::Uint8, 8)),
            2 => Ok((PixelType::Uint16, 16)),
            4 => Ok((PixelType::Float32, 32)),
            _ => Err(BioFormatsError::UnsupportedFormat(format!(
                "VG SAM unsupported bytes per pixel: {bytes_per_pixel}"
            ))),
        }
    }
}

impl Default for VgSamReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for VgSamReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("dti") | Some("vgsam"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.starts_with(Self::MAGIC)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        if data.len() < Self::PIXEL_OFFSET {
            return Err(BioFormatsError::UnsupportedFormat(
                "VG SAM file is shorter than the 368-byte header".into(),
            ));
        }
        if !data.starts_with(Self::MAGIC) {
            return Err(BioFormatsError::UnsupportedFormat(
                "VG SAM file is missing VGS magic".into(),
            ));
        }
        let width = Self::read_i32_be(&data, 348, "width")?;
        let height = Self::read_i32_be(&data, 352, "height")?;
        let bytes_per_pixel = Self::read_i32_be(&data, 360, "bytes per pixel")?;
        if width <= 0 || height <= 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "VG SAM header contains invalid image dimensions".into(),
            ));
        }
        let (pixel_type, bits_per_pixel) = Self::pixel_type_from_bpp(bytes_per_pixel)?;
        let width = width as u32;
        let height = height as u32;
        let expected = (Self::PIXEL_OFFSET as u64)
            .checked_add(width as u64 * height as u64 * bytes_per_pixel as u64)
            .ok_or_else(|| BioFormatsError::Format("VG SAM size overflows".into()))?;
        if (data.len() as u64) < expected {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "VG SAM pixel payload is shorter than declared dimensions {width}x{height}"
            )));
        }

        let mut series_metadata = HashMap::new();
        series_metadata.insert(
            "Bytes per pixel".into(),
            crate::common::metadata::MetadataValue::Int(bytes_per_pixel as i64),
        );
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
            is_little_endian: false,
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
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(Self::PIXEL_OFFSET as u64))
            .map_err(BioFormatsError::Io)?;
        let mut buf =
            vec![
                0u8;
                meta.size_x as usize * meta.size_y as usize * meta.pixel_type.bytes_per_sample()
            ];
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
        crop_full_plane("VG SAM", &full, &meta, 1, _x, _y, w, h)
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
// Raw binary reader — UBM Messtechnik
// ===========================================================================

/// UBM reader (`.pr3`, plus legacy `.ubm` alias).
///
/// Java Bio-Formats stores dimensions at offsets 44/48 in a 128-byte
/// little-endian header, followed by uint32 pixels with optional row padding.
pub struct UbmReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    padding_pixels: usize,
}

impl UbmReader {
    const HEADER_SIZE: usize = 128;

    pub fn new() -> Self {
        UbmReader {
            path: None,
            meta: None,
            padding_pixels: 0,
        }
    }

    fn read_i32_le(data: &[u8], offset: usize, label: &str) -> Result<i32> {
        let bytes = data.get(offset..offset + 4).ok_or_else(|| {
            BioFormatsError::UnsupportedFormat(format!("UBM header missing {label}"))
        })?;
        Ok(i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }
}

impl Default for UbmReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for UbmReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("pr3") | Some("ubm"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        if data.len() < Self::HEADER_SIZE {
            return Err(BioFormatsError::UnsupportedFormat(
                "UBM file is shorter than the 128-byte header".into(),
            ));
        }
        let width = Self::read_i32_le(&data, 44, "width")?;
        let height = Self::read_i32_le(&data, 48, "height")?;
        if width <= 0 || height <= 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "UBM header contains invalid image dimensions".into(),
            ));
        }
        let width = width as u32;
        let height = height as u32;
        let plane_bytes = width as u64 * height as u64 * 4;
        let min_len = Self::HEADER_SIZE as u64 + plane_bytes;
        if (data.len() as u64) < min_len {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "UBM pixel payload is shorter than declared dimensions {width}x{height}"
            )));
        }
        let extra = data.len() as u64 - min_len;
        let row_padding_bytes = extra
            .checked_div(height as u64)
            .ok_or_else(|| BioFormatsError::Format("UBM row padding overflows".into()))?;
        if row_padding_bytes % 4 != 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "UBM row padding is not aligned to uint32 pixels".into(),
            ));
        }
        let padding_pixels = (row_padding_bytes / 4) as usize;

        self.path = Some(path.to_path_buf());
        self.padding_pixels = padding_pixels;
        let mut series_metadata = HashMap::new();
        series_metadata.insert(
            "Padding pixels".to_string(),
            crate::common::metadata::MetadataValue::Int(padding_pixels as i64),
        );
        self.meta = Some(ImageMetadata {
            size_x: width,
            size_y: height,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type: PixelType::Uint32,
            bits_per_pixel: 32,
            image_count: 1,
            dimension_order: DimensionOrder::XYZCT,
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
        self.padding_pixels = 0;
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
        self.open_bytes_region(plane_index, 0, 0, meta.size_x, meta.size_y)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        _x: u32,
        _y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        crate::common::region::validate_region("UBM", meta.size_x, meta.size_y, _x, _y, w, h)?;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
        let row_stride = (meta.size_x as usize + self.padding_pixels)
            .checked_mul(4)
            .ok_or_else(|| BioFormatsError::Format("UBM row stride overflows".into()))?;
        let out_row = (w as usize)
            .checked_mul(4)
            .ok_or_else(|| BioFormatsError::Format("UBM output row size overflows".into()))?;
        let mut out = Vec::with_capacity(out_row * h as usize);
        for row in 0..h as usize {
            let source_row = _y as usize + row;
            let offset =
                Self::HEADER_SIZE as u64 + source_row as u64 * row_stride as u64 + _x as u64 * 4;
            f.seek(SeekFrom::Start(offset))
                .map_err(BioFormatsError::Io)?;
            let start = out.len();
            out.resize(start + out_row, 0);
            f.read_exact(&mut out[start..start + out_row])
                .map_err(BioFormatsError::Io)?;
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

// ===========================================================================
// Raw binary reader — Seiko SPM
// ===========================================================================

/// Seiko SPM reader (`.xqd`, `.xqf`).
///
/// Java Bio-Formats stores dimensions at offset 1402 in a 2944-byte
/// little-endian header, followed by raw uint16 pixels.
pub struct SeikoReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl SeikoReader {
    const HEADER_SIZE: usize = 2944;

    pub fn new() -> Self {
        SeikoReader {
            path: None,
            meta: None,
        }
    }

    fn read_u16_le(data: &[u8], offset: usize, label: &str) -> Result<u16> {
        let bytes = data.get(offset..offset + 2).ok_or_else(|| {
            BioFormatsError::UnsupportedFormat(format!("Seiko SPM header missing {label}"))
        })?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn read_f32_le(data: &[u8], offset: usize) -> Option<f32> {
        let bytes = data.get(offset..offset + 4)?;
        Some(f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }
}

impl Default for SeikoReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for SeikoReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("xqd") | Some("xqf"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        if data.len() < Self::HEADER_SIZE {
            return Err(BioFormatsError::UnsupportedFormat(
                "Seiko SPM file is shorter than the 2944-byte header".into(),
            ));
        }
        let width = Self::read_u16_le(&data, 1402, "width")? as u32;
        let height = Self::read_u16_le(&data, 1404, "height")? as u32;
        if width == 0 || height == 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "Seiko SPM header contains invalid image dimensions".into(),
            ));
        }
        let expected = (Self::HEADER_SIZE as u64)
            .checked_add(width as u64 * height as u64 * 2)
            .ok_or_else(|| BioFormatsError::Format("Seiko SPM size overflows".into()))?;
        if (data.len() as u64) < expected {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "Seiko SPM pixel payload is shorter than declared dimensions {width}x{height}"
            )));
        }

        let mut series_metadata = HashMap::new();
        let comment_bytes = &data[40..data.len().min(156)];
        let nul = comment_bytes
            .iter()
            .position(|b| *b == 0)
            .unwrap_or(comment_bytes.len());
        let comment = String::from_utf8_lossy(&comment_bytes[..nul])
            .trim()
            .to_string();
        if !comment.is_empty() {
            series_metadata.insert(
                "Comment".into(),
                crate::common::metadata::MetadataValue::String(comment),
            );
        }
        if let Some(x_size) = Self::read_f32_le(&data, 156) {
            series_metadata.insert(
                "X size".into(),
                crate::common::metadata::MetadataValue::Float(x_size as f64),
            );
        }
        if let Some(y_size) = Self::read_f32_le(&data, 164) {
            series_metadata.insert(
                "Y size".into(),
                crate::common::metadata::MetadataValue::Float(y_size as f64),
            );
        }

        self.path = Some(path.to_path_buf());
        self.meta = Some(ImageMetadata {
            size_x: width,
            size_y: height,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type: PixelType::Uint16,
            bits_per_pixel: 16,
            image_count: 1,
            dimension_order: DimensionOrder::XYZCT,
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
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(Self::HEADER_SIZE as u64))
            .map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; meta.size_x as usize * meta.size_y as usize * 2];
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
        crop_full_plane("Seiko SPM", &full, &meta, 1, _x, _y, w, h)
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
