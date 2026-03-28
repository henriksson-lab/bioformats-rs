//! Camera and RAW format readers — PCO, Bio-Rad GEL, Hamamatsu L2D, and more.
//!
//! Includes three binary readers with partial metadata parsing (PcoRawReader,
//! BioRadGelReader, L2dReader) and several extension-only placeholder readers.

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
fn placeholder_meta_u16() -> ImageMetadata {
    ImageMetadata {
        size_x: 512,
        size_y: 512,
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
    }
}

// ---------------------------------------------------------------------------
// Macro for extension-only placeholder readers (uint8, 512x512)
// ---------------------------------------------------------------------------
macro_rules! placeholder_reader {
    (
        $(#[$attr:meta])*
        pub struct $name:ident;
        extensions: [$($ext:literal),+];
        magic_bytes: false;
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
        }
    };
}

// ---------------------------------------------------------------------------
// Macro for TIFF wrapper readers
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
// 1. PCO B16 raw camera file
// ---------------------------------------------------------------------------
/// PCO camera raw B16 binary format (`.b16`).
///
/// Header is 216 bytes; width at offset 4 (u16 LE), height at offset 6 (u16 LE).
/// Pixel data starts at offset 216 as 16-bit little-endian grayscale values.
pub struct PcoRawReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl PcoRawReader {
    pub fn new() -> Self {
        PcoRawReader { path: None, meta: None }
    }
}

impl Default for PcoRawReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for PcoRawReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        matches!(
            path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref(),
            Some("b16")
        )
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool { false }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let mut f = std::fs::File::open(path)
            .map_err(|e| BioFormatsError::Io(e))?;
        let mut header = [0u8; 216];
        let n = f.read(&mut header).map_err(|e| BioFormatsError::Io(e))?;
        let (w, h) = if n >= 8 {
            let w = u16::from_le_bytes([header[4], header[5]]) as u32;
            let h = u16::from_le_bytes([header[6], header[7]]) as u32;
            if w == 0 || h == 0 { (512, 512) } else { (w, h) }
        } else {
            (512, 512)
        };
        self.path = Some(path.to_path_buf());
        self.meta = Some(ImageMetadata {
            size_x: w,
            size_y: h,
            pixel_type: PixelType::Uint16,
            bits_per_pixel: 16,
            is_little_endian: true,
            ..placeholder_meta_u16()
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
        let n_bytes = meta.size_x as usize * meta.size_y as usize * 2;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = std::fs::File::open(path).map_err(|e| BioFormatsError::Io(e))?;
        f.seek(SeekFrom::Start(216)).map_err(|e| BioFormatsError::Io(e))?;
        let mut buf = vec![0u8; n_bytes];
        f.read_exact(&mut buf).map_err(|e| BioFormatsError::Io(e))?;
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
}

// ---------------------------------------------------------------------------
// 2. Bio-Rad GEL phosphor imager (.1sc)
// ---------------------------------------------------------------------------
/// Bio-Rad GEL phosphor imager format (`.1sc`).
///
/// 76-byte header; width at offset 10 (u16 BE), height at offset 12 (u16 BE).
/// Pixel data at offset 76 as 16-bit big-endian values.
pub struct BioRadGelReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl BioRadGelReader {
    pub fn new() -> Self {
        BioRadGelReader { path: None, meta: None }
    }
}

impl Default for BioRadGelReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for BioRadGelReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        matches!(
            path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref(),
            Some("1sc")
        )
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool { false }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let mut f = std::fs::File::open(path).map_err(|e| BioFormatsError::Io(e))?;
        let mut header = [0u8; 76];
        let n = f.read(&mut header).map_err(|e| BioFormatsError::Io(e))?;
        let (w, h) = if n >= 14 {
            let w = u16::from_be_bytes([header[10], header[11]]) as u32;
            let h = u16::from_be_bytes([header[12], header[13]]) as u32;
            if w == 0 || h == 0 || w > 32768 || h > 32768 { (512, 512) } else { (w, h) }
        } else {
            (512, 512)
        };
        self.path = Some(path.to_path_buf());
        self.meta = Some(ImageMetadata {
            size_x: w,
            size_y: h,
            pixel_type: PixelType::Uint16,
            bits_per_pixel: 16,
            is_little_endian: false,
            ..placeholder_meta_u16()
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
        let n_bytes = meta.size_x as usize * meta.size_y as usize * 2;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = std::fs::File::open(path).map_err(|e| BioFormatsError::Io(e))?;
        f.seek(SeekFrom::Start(76)).map_err(|e| BioFormatsError::Io(e))?;
        let mut buf = vec![0u8; n_bytes];
        f.read_exact(&mut buf).map_err(|e| BioFormatsError::Io(e))?;
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
}

// ---------------------------------------------------------------------------
// 3. Hamamatsu L2D INI-style text metadata
// ---------------------------------------------------------------------------
/// Hamamatsu L2D format (`.l2d`).
///
/// INI-style text file with `ImageWidth=N` and `ImageHeight=N` keys.
/// Returns uint8 RGB placeholder metadata.
pub struct L2dReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl L2dReader {
    pub fn new() -> Self {
        L2dReader { path: None, meta: None }
    }
}

impl Default for L2dReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for L2dReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        matches!(
            path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref(),
            Some("l2d")
        )
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool { false }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        let mut w: u32 = 512;
        let mut h: u32 = 512;
        for line in text.lines() {
            let line = line.trim();
            if let Some(val) = line.strip_prefix("ImageWidth=") {
                if let Ok(v) = val.trim().parse::<u32>() {
                    if v > 0 { w = v; }
                }
            } else if let Some(val) = line.strip_prefix("ImageHeight=") {
                if let Ok(v) = val.trim().parse::<u32>() {
                    if v > 0 { h = v; }
                }
            }
        }
        self.path = Some(path.to_path_buf());
        self.meta = Some(ImageMetadata {
            size_x: w,
            size_y: h,
            size_z: 1,
            size_c: 3,
            size_t: 1,
            pixel_type: PixelType::Uint8,
            bits_per_pixel: 8,
            image_count: 1,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: true,
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
}

// ---------------------------------------------------------------------------
// 4. Canon RAW (CR2 / CRW / CR3) — extension-only placeholder
// ---------------------------------------------------------------------------
placeholder_reader! {
    /// Canon RAW format placeholder reader (`.cr2`, `.crw`, `.cr3`).
    pub struct CanonRawReader;
    extensions: ["cr2", "crw", "cr3"];
    magic_bytes: false;
}

// ---------------------------------------------------------------------------
// 5. Hasselblad Imacon — extension-only placeholder
// ---------------------------------------------------------------------------
placeholder_reader! {
    /// Hasselblad Imacon format placeholder reader (`.fff`).
    pub struct ImaconReader;
    extensions: ["fff"];
    magic_bytes: false;
}

// ---------------------------------------------------------------------------
// 6. Santa Barbara Instrument Group — extension-only placeholder
// ---------------------------------------------------------------------------
placeholder_reader! {
    /// Santa Barbara Instrument Group format placeholder reader (`.fts`).
    pub struct SbigReader;
    extensions: ["fts"];
    magic_bytes: false;
}

// ---------------------------------------------------------------------------
// 7. Image Pro Workspace — extension-only placeholder
// ---------------------------------------------------------------------------
placeholder_reader! {
    /// Image Pro Workspace format placeholder reader (`.ipw`).
    pub struct IpwReader;
    extensions: ["ipw"];
    magic_bytes: false;
}

// ---------------------------------------------------------------------------
// 8. Photoshop-annotated TIFF — TIFF wrapper
// ---------------------------------------------------------------------------
tiff_wrapper! {
    /// Photoshop-annotated TIFF format (`.tif`).
    pub struct PhotoshopTiffReader;
    extensions: ["tif"];
}
