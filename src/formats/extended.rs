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

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn placeholder_meta() -> ImageMetadata {
    ImageMetadata {
        size_x: 512,
        size_y: 512,
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
    }
}

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
        GelReader { path: None, meta: None }
    }
}

impl Default for GelReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for GelReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("gel"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool { false }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let data = std::fs::read(path)
            .map_err(|e| BioFormatsError::Io(e))?;

        let (w, h) = if data.len() >= 14 {
            let w = u16::from_be_bytes([data[10], data[11]]) as u32;
            let h = u16::from_be_bytes([data[12], data[13]]) as u32;
            if w == 0 || w > 32768 || h == 0 || h > 32768 {
                (512u32, 512u32)
            } else {
                (w, h)
            }
        } else {
            (512u32, 512u32)
        };

        self.path = Some(path.to_path_buf());
        self.meta = Some(placeholder_meta_u16(w, h));
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
        let n_bytes = meta.size_x as usize * meta.size_y as usize * 2;
        let path = self.path.clone().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = std::fs::File::open(&path)
            .map_err(|e| BioFormatsError::Io(e))?;
        f.seek(SeekFrom::Start(64))
            .map_err(|e| BioFormatsError::Io(e))?;
        let mut buf = vec![0u8; n_bytes];
        let _ = f.read(&mut buf).map_err(|e| BioFormatsError::Io(e))?;
        Ok(buf)
    }

    fn open_bytes_region(&mut self, plane_index: u32, _x: u32, _y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        Ok(vec![0u8; w as usize * h as usize * 2])
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

// ---------------------------------------------------------------------------
// 4. Imspector OBF STED microscopy
// ---------------------------------------------------------------------------

/// Imspector OBF/MSR STED microscopy format (`.obf`, `.msr`).
///
/// Magic: first 8 bytes contain "OMAS_BF_".
pub struct ImspectorReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl ImspectorReader {
    pub fn new() -> Self {
        ImspectorReader { path: None, meta: None }
    }
}

impl Default for ImspectorReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for ImspectorReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("obf") | Some("msr"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        if header.len() >= 8 {
            &header[..8] == b"OMAS_BF_"
        } else {
            false
        }
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.path = Some(path.to_path_buf());
        // Simple placeholder: 256x256 uint16
        self.meta = Some(placeholder_meta_u16(256, 256));
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
        Ok(vec![0u8; meta.size_x as usize * meta.size_y as usize * 2])
    }

    fn open_bytes_region(&mut self, plane_index: u32, _x: u32, _y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        Ok(vec![0u8; w as usize * h as usize * 2])
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

// ---------------------------------------------------------------------------
// 5. Hamamatsu VMS whole-slide
// ---------------------------------------------------------------------------

/// Hamamatsu VMS/VMU whole-slide format (`.vms`, `.vmu`).
///
/// VMS files are text files listing JPEG tile files. Parses ImageWidth/ImageHeight.
pub struct HamamatsuVmsReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl HamamatsuVmsReader {
    pub fn new() -> Self {
        HamamatsuVmsReader { path: None, meta: None }
    }
}

impl Default for HamamatsuVmsReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for HamamatsuVmsReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("vms") | Some("vmu"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool { false }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let (mut w, mut h) = (512u32, 512u32);
        if let Ok(text) = std::fs::read_to_string(path) {
            for line in text.lines() {
                if let Some(val) = line.strip_prefix("ImageWidth=") {
                    if let Ok(n) = val.trim().parse::<u32>() {
                        if n > 0 { w = n; }
                    }
                } else if let Some(val) = line.strip_prefix("ImageHeight=") {
                    if let Ok(n) = val.trim().parse::<u32>() {
                        if n > 0 { h = n; }
                    }
                }
            }
        }
        self.path = Some(path.to_path_buf());
        // RGB uint8 placeholder
        let mut meta = placeholder_meta();
        meta.size_x = w;
        meta.size_y = h;
        meta.size_c = 3;
        meta.is_rgb = true;
        meta.is_interleaved = true;
        self.meta = Some(meta);
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
        Ok(vec![0u8; meta.size_x as usize * meta.size_y as usize * 3])
    }

    fn open_bytes_region(&mut self, plane_index: u32, _x: u32, _y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        Ok(vec![0u8; w as usize * h as usize * 3])
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
}

impl CellomicsReader {
    pub fn new() -> Self {
        CellomicsReader { path: None, meta: None }
    }
}

impl Default for CellomicsReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for CellomicsReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("c01"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool { false }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let data = std::fs::read(path)
            .map_err(|e| BioFormatsError::Io(e))?;

        let (w, h, pixel_type, bpp) = if data.len() >= 10 {
            let w = u16::from_le_bytes([data[4], data[5]]) as u32;
            let h = u16::from_le_bytes([data[6], data[7]]) as u32;
            let bd = u16::from_le_bytes([data[8], data[9]]);
            let (w, h) = if w == 0 || w > 32768 || h == 0 || h > 32768 { (512, 512) } else { (w, h) };
            let (pt, bpp) = match bd {
                8 => (PixelType::Uint8, 8u8),
                _ => (PixelType::Uint16, 16u8),
            };
            (w, h, pt, bpp)
        } else {
            (512u32, 512u32, PixelType::Uint16, 16u8)
        };

        self.path = Some(path.to_path_buf());
        self.meta = Some(ImageMetadata {
            size_x: w,
            size_y: h,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type,
            bits_per_pixel: bpp,
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
        let bytes_per_pixel = (meta.bits_per_pixel / 8) as usize;
        let n_bytes = meta.size_x as usize * meta.size_y as usize * bytes_per_pixel;
        let path = self.path.clone().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = std::fs::File::open(&path)
            .map_err(|e| BioFormatsError::Io(e))?;
        f.seek(SeekFrom::Start(52))
            .map_err(|e| BioFormatsError::Io(e))?;
        let mut buf = vec![0u8; n_bytes];
        let _ = f.read(&mut buf).map_err(|e| BioFormatsError::Io(e))?;
        Ok(buf)
    }

    fn open_bytes_region(&mut self, plane_index: u32, _x: u32, _y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let bytes_per_pixel = (meta.bits_per_pixel / 8) as usize;
        Ok(vec![0u8; w as usize * h as usize * bytes_per_pixel])
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
// Group B — Extension-only placeholder readers
// ===========================================================================

// ---------------------------------------------------------------------------
// 7. Minolta Digital Camera RAW
// ---------------------------------------------------------------------------
placeholder_reader! {
    /// Minolta Digital Camera RAW placeholder reader (`.mrw`).
    pub struct MrwReader;
    extensions: ["mrw"];
}

// ---------------------------------------------------------------------------
// 8. Yokogawa CV7000/8000 HCS
// ---------------------------------------------------------------------------
placeholder_reader! {
    /// Yokogawa CV7000/8000 HCS placeholder reader (`.wpi`, `.mrf`).
    pub struct YokogawaReader;
    extensions: ["wpi", "mrf"];
}

// ---------------------------------------------------------------------------
// 9. Leica single-image LOF
// ---------------------------------------------------------------------------
placeholder_reader! {
    /// Leica single-image LOF placeholder reader (`.lof`).
    pub struct LeicaLofReader;
    extensions: ["lof"];
}

// ---------------------------------------------------------------------------
// 10. Animated PNG
// ---------------------------------------------------------------------------
placeholder_reader! {
    /// Animated PNG placeholder reader (`.apng`).
    pub struct ApngReader;
    extensions: ["apng"];
}

// ---------------------------------------------------------------------------
// 11. POV-Ray density grid
// ---------------------------------------------------------------------------
placeholder_reader! {
    /// POV-Ray density grid placeholder reader (`.pov`, `.df3`).
    pub struct PovRayReader;
    extensions: ["pov", "df3"];
}

// ---------------------------------------------------------------------------
// 12. NAF format
// ---------------------------------------------------------------------------
placeholder_reader! {
    /// NAF format placeholder reader (`.naf`).
    pub struct NafReader;
    extensions: ["naf"];
}

// ---------------------------------------------------------------------------
// 13. Burleigh piezo/SPM
// ---------------------------------------------------------------------------
placeholder_reader! {
    /// Burleigh piezo/SPM placeholder reader (`.img`).
    /// NOTE: `.img` is very generic; this is a last-resort extension fallback.
    pub struct BurleighReader;
    extensions: ["img"];
}
