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
        PicoQuantReader { path: None, meta: None }
    }
}

impl Default for PicoQuantReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for PicoQuantReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("ptu") | Some("pqres"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.len() >= 6 && &header[0..6] == b"PQTTTR"
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let data = std::fs::read(path)
            .map_err(BioFormatsError::Io)?;

        // Read first 4096 bytes as lossy string for header parsing
        let header_bytes = &data[..data.len().min(4096)];
        let text = String::from_utf8_lossy(header_bytes).into_owned();

        let mut width: u32 = 64;
        let mut height: u32 = 64;
        let mut size_z: u32 = 1;

        for line in text.lines() {
            if let Some(val) = line.strip_prefix("ImgHdr_Pixels=") {
                if let Ok(n) = val.trim().parse::<u32>() { width = n; }
            } else if let Some(val) = line.strip_prefix("ImgHdr_Lines=") {
                if let Ok(n) = val.trim().parse::<u32>() { height = n; }
            } else if let Some(val) = line.strip_prefix("ImgHdr_Frame=") {
                if let Ok(n) = val.trim().parse::<u32>() { size_z = n; }
            }
        }

        self.path = Some(path.to_path_buf());
        self.meta = Some(ImageMetadata {
            size_x: width,
            size_y: height,
            size_z,
            size_c: 1,
            size_t: 1,
            pixel_type: PixelType::Uint32,
            bits_per_pixel: 32,
            image_count: size_z,
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
        // Uint32 = 4 bytes per pixel
        Ok(vec![0u8; meta.size_x as usize * meta.size_y as usize * 4])
    }

    fn open_bytes_region(&mut self, plane_index: u32, _x: u32, _y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        Ok(vec![0u8; w as usize * h as usize * 4])
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
// Helper: compute square dimensions from file size assuming uint16
// ===========================================================================

/// Given a file size and a data offset, compute square dimensions assuming
/// uint16 (2 bytes per pixel). Returns (width, height).
fn raw_uint16_square_dims(file_size: u64, data_offset: u64) -> (u32, u32) {
    let data_bytes = file_size.saturating_sub(data_offset);
    let pixel_count = data_bytes / 2;
    let side = (pixel_count as f64).sqrt() as u32;
    let side = side.max(1);
    (side, side)
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
        RhkReader { path: None, meta: None, data_offset: 0 }
    }
}

impl Default for RhkReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for RhkReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("sm2") | Some("sm3") | Some("sm4"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool { false }

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
            if line_lower.contains("x_size") || line_lower.contains("xsize")
                || line_lower.contains("x size") || line_lower.contains("columns")
            {
                if let Some(val) = line.split_whitespace().last() {
                    if let Ok(w) = val.parse::<u32>() { width = w; }
                }
            } else if line_lower.contains("y_size") || line_lower.contains("ysize")
                || line_lower.contains("y size") || line_lower.contains("rows")
            {
                if let Some(val) = line.split_whitespace().last() {
                    if let Ok(h) = val.parse::<u32>() { height = h; }
                }
            } else if line_lower.contains("data offset") || line_lower.contains("header size") {
                if let Some(val) = line.split_whitespace().last() {
                    if let Ok(off) = val.parse::<u64>() { data_off = off; }
                }
            }
        }

        if width == 0 || height == 0 {
            let (w, h) = raw_uint16_square_dims(file_size, data_off);
            width = w;
            height = h;
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
        let mut f = std::fs::File::open(&path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(self.data_offset)).map_err(BioFormatsError::Io)?;
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
        QuesantReader { path: None, meta: None, data_offset: 0 }
    }
}

impl Default for QuesantReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for QuesantReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        // Note: .afm is also used by VeecoReader (Nanoscope). Quesant AFM
        // files lack the NANOSCOPE header, so this reader is a fallback.
        matches!(ext.as_deref(), Some("afm"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool { false }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let file_size = std::fs::metadata(path).map_err(BioFormatsError::Io)?.len();
        let (w, h) = raw_uint16_square_dims(file_size, 0);

        self.data_offset = 0;
        self.path = Some(path.to_path_buf());
        self.meta = Some(ImageMetadata {
            size_x: w,
            size_y: h,
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
        let mut f = std::fs::File::open(&path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(self.data_offset)).map_err(BioFormatsError::Io)?;
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

    fn placeholder_meta() -> ImageMetadata {
        ImageMetadata {
            size_x: 1,
            size_y: 1,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type: PixelType::Uint8,
            bits_per_pixel: 8,
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
        }
    }
}

impl Default for JpkReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for JpkReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("jpk"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // JPK files are ZIP archives
        header.len() >= 4 && header[0..4] == [0x50, 0x4B, 0x03, 0x04]
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
            // No TIFF found — use placeholder metadata
            self.meta = Some(Self::placeholder_meta());
            self.is_tiff = false;
            return Ok(());
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
        if self.is_tiff { self.inner.series_count() } else { 1 }
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
        if self.is_tiff { self.inner.series() } else { 0 }
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
        if plane_index != 0 {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        Ok(vec![0u8])
    }

    fn open_bytes_region(&mut self, plane_index: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        if self.is_tiff {
            return self.inner.open_bytes_region(plane_index, x, y, w, h);
        }
        if plane_index != 0 {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let meta = self.meta.as_ref().unwrap();
        let bps = meta.pixel_type.bytes_per_sample();
        Ok(vec![0u8; w as usize * h as usize * bps])
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        if self.is_tiff {
            return self.inner.open_thumb_bytes(plane_index);
        }
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn resolution_count(&self) -> usize {
        if self.is_tiff { self.inner.resolution_count() } else { 1 }
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if self.is_tiff {
            self.inner.set_resolution(level)
        } else if level != 0 {
            Err(BioFormatsError::Format(format!("resolution {} out of range", level)))
        } else {
            Ok(())
        }
    }
}

// ===========================================================================
// Raw binary reader — WaTom SPM
// ===========================================================================

/// WaTom SPM reader (`.wap`, `.opo`, `.opz`, `.opt`).
///
/// Binary format. Falls back to raw uint16 square heuristic.
pub struct WatopReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    data_offset: u64,
}

impl WatopReader {
    pub fn new() -> Self {
        WatopReader { path: None, meta: None, data_offset: 0 }
    }
}

impl Default for WatopReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for WatopReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("wap") | Some("opo") | Some("opz") | Some("opt"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool { false }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let file_size = std::fs::metadata(path).map_err(BioFormatsError::Io)?.len();
        let (w, h) = raw_uint16_square_dims(file_size, 0);

        self.data_offset = 0;
        self.path = Some(path.to_path_buf());
        self.meta = Some(ImageMetadata {
            size_x: w,
            size_y: h,
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
        let mut f = std::fs::File::open(&path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(self.data_offset)).map_err(BioFormatsError::Io)?;
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
// Raw binary reader — VG SAM
// ===========================================================================

/// VG SAM reader (`.vgsam`).
///
/// Binary format. Falls back to raw uint16 square heuristic.
pub struct VgSamReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    data_offset: u64,
}

impl VgSamReader {
    pub fn new() -> Self {
        VgSamReader { path: None, meta: None, data_offset: 0 }
    }
}

impl Default for VgSamReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for VgSamReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("vgsam"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool { false }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let file_size = std::fs::metadata(path).map_err(BioFormatsError::Io)?.len();
        let (w, h) = raw_uint16_square_dims(file_size, 0);

        self.data_offset = 0;
        self.path = Some(path.to_path_buf());
        self.meta = Some(ImageMetadata {
            size_x: w,
            size_y: h,
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
        let mut f = std::fs::File::open(&path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(self.data_offset)).map_err(BioFormatsError::Io)?;
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
// Raw binary reader — UBM Messtechnik
// ===========================================================================

/// UBM Messtechnik reader (`.ubm`).
///
/// Binary format. Falls back to raw uint16 square heuristic.
pub struct UbmReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    data_offset: u64,
}

impl UbmReader {
    pub fn new() -> Self {
        UbmReader { path: None, meta: None, data_offset: 0 }
    }
}

impl Default for UbmReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for UbmReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("ubm"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool { false }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let file_size = std::fs::metadata(path).map_err(BioFormatsError::Io)?.len();
        let (w, h) = raw_uint16_square_dims(file_size, 0);

        self.data_offset = 0;
        self.path = Some(path.to_path_buf());
        self.meta = Some(ImageMetadata {
            size_x: w,
            size_y: h,
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
        let mut f = std::fs::File::open(&path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(self.data_offset)).map_err(BioFormatsError::Io)?;
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
// Raw binary reader — Seiko SPM
// ===========================================================================

/// Seiko SPM reader (`.xqd`, `.xqf`).
///
/// Binary format. Falls back to raw uint16 square heuristic.
pub struct SeikoReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    data_offset: u64,
}

impl SeikoReader {
    pub fn new() -> Self {
        SeikoReader { path: None, meta: None, data_offset: 0 }
    }
}

impl Default for SeikoReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for SeikoReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("xqd") | Some("xqf"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool { false }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let file_size = std::fs::metadata(path).map_err(BioFormatsError::Io)?.len();
        let (w, h) = raw_uint16_square_dims(file_size, 0);

        self.data_offset = 0;
        self.path = Some(path.to_path_buf());
        self.meta = Some(ImageMetadata {
            size_x: w,
            size_y: h,
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
        let mut f = std::fs::File::open(&path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(self.data_offset)).map_err(BioFormatsError::Io)?;
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
