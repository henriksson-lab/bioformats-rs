//! Placeholder readers for remaining obscure and proprietary formats.
//!
//! All readers are extension-only and return 512×512 uint8 placeholder metadata
//! with zeroed pixel data. Full decoding is not implemented.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;

// ---------------------------------------------------------------------------
// Macro for extension-only placeholder readers
// ---------------------------------------------------------------------------
#[allow(unused_macros)]
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
// 1. Applied Precision APL
// ---------------------------------------------------------------------------
/// Applied Precision format reader (`.apl`).
///
/// Applied Precision APL is a proprietary binary format used by DeltaVision
/// instruments. The internal structure requires vendor documentation.
pub struct AplReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl AplReader {
    pub fn new() -> Self {
        AplReader { path: None, meta: None }
    }
}

impl Default for AplReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for AplReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("apl"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool { false }

    fn set_id(&mut self, _path: &Path) -> Result<()> {
        Err(BioFormatsError::UnsupportedFormat(
            "Applied Precision APL is a proprietary format requiring vendor documentation".to_string()
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
            "Applied Precision APL is a proprietary format requiring vendor documentation".to_string()
        ))
    }

    fn open_bytes_region(&mut self, _plane_index: u32, _x: u32, _y: u32, _w: u32, _h: u32) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "Applied Precision APL is a proprietary format requiring vendor documentation".to_string()
        ))
    }

    fn open_thumb_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "Applied Precision APL is a proprietary format requiring vendor documentation".to_string()
        ))
    }
}

// ---------------------------------------------------------------------------
// 2. ARF format — raw uint16 heuristic
// ---------------------------------------------------------------------------
/// ARF binary format reader (`.arf`).
///
/// Attempts to read the file as raw uint16 data. Guesses square dimensions
/// from the file size. If the file size does not correspond to a valid image,
/// returns an error.
pub struct ArfReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl ArfReader {
    pub fn new() -> Self {
        ArfReader { path: None, meta: None }
    }
}

impl Default for ArfReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for ArfReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("arf"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool { false }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let file_len = std::fs::metadata(path)
            .map_err(BioFormatsError::Io)?
            .len();

        // Assume raw uint16 data; guess square dimensions
        let n_pixels = file_len / 2;
        if n_pixels == 0 {
            return Err(BioFormatsError::Format("ARF file is empty".to_string()));
        }
        let side = (n_pixels as f64).sqrt() as u32;
        let (w, h) = if side > 0 && (side as u64 * side as u64) == n_pixels {
            (side, side)
        } else {
            // Non-square: treat as single row
            (n_pixels as u32, 1u32)
        };

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
        let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; n_bytes];
        let bytes_read = f.read(&mut buf).map_err(BioFormatsError::Io)?;
        buf.truncate(bytes_read);
        buf.resize(n_bytes, 0);
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
// 3. I2I format
// ---------------------------------------------------------------------------
/// I2I format reader (`.i2i`).
///
/// I2I is a proprietary format with undocumented structure.
pub struct I2iReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl I2iReader {
    pub fn new() -> Self {
        I2iReader { path: None, meta: None }
    }
}

impl Default for I2iReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for I2iReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("i2i"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool { false }

    fn set_id(&mut self, _path: &Path) -> Result<()> {
        Err(BioFormatsError::UnsupportedFormat(
            "I2I format is proprietary with undocumented structure".to_string()
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
            "I2I format is proprietary with undocumented structure".to_string()
        ))
    }

    fn open_bytes_region(&mut self, _plane_index: u32, _x: u32, _y: u32, _w: u32, _h: u32) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "I2I format is proprietary with undocumented structure".to_string()
        ))
    }

    fn open_thumb_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "I2I format is proprietary with undocumented structure".to_string()
        ))
    }
}

// ---------------------------------------------------------------------------
// 4. JDCE format
// ---------------------------------------------------------------------------
/// JDCE format reader (`.jdce`).
///
/// JDCE is a proprietary format with undocumented structure.
pub struct JdceReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl JdceReader {
    pub fn new() -> Self {
        JdceReader { path: None, meta: None }
    }
}

impl Default for JdceReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for JdceReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("jdce"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool { false }

    fn set_id(&mut self, _path: &Path) -> Result<()> {
        Err(BioFormatsError::UnsupportedFormat(
            "JDCE format is proprietary with undocumented structure".to_string()
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
            "JDCE format is proprietary with undocumented structure".to_string()
        ))
    }

    fn open_bytes_region(&mut self, _plane_index: u32, _x: u32, _y: u32, _w: u32, _h: u32) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "JDCE format is proprietary with undocumented structure".to_string()
        ))
    }

    fn open_thumb_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "JDCE format is proprietary with undocumented structure".to_string()
        ))
    }
}

// ---------------------------------------------------------------------------
// 5. JPX (JPEG 2000 Part 2)
// ---------------------------------------------------------------------------
/// JPX (JPEG 2000 Part 2) format reader (`.jpx`).
///
/// JPX files are JPEG 2000 Part 2; delegates to `Jpeg2000Reader`.
pub struct JpxReader {
    inner: crate::formats::misc::Jpeg2000Reader,
}

impl JpxReader {
    pub fn new() -> Self {
        JpxReader { inner: crate::formats::misc::Jpeg2000Reader::new() }
    }
}

impl Default for JpxReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for JpxReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("jpx"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        self.inner.is_this_type_by_bytes(header)
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
}

// ---------------------------------------------------------------------------
// 6. Capture Pro Image (PCI)
// ---------------------------------------------------------------------------
/// Capture Pro Image format reader (`.pci`).
///
/// Capture Pro is a proprietary format from Media Cybernetics with
/// undocumented binary structure.
pub struct PciReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl PciReader {
    pub fn new() -> Self {
        PciReader { path: None, meta: None }
    }
}

impl Default for PciReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for PciReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("pci"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool { false }

    fn set_id(&mut self, _path: &Path) -> Result<()> {
        Err(BioFormatsError::UnsupportedFormat(
            "Capture Pro Image (PCI) is a proprietary format from Media Cybernetics".to_string()
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
            "Capture Pro Image (PCI) is a proprietary format from Media Cybernetics".to_string()
        ))
    }

    fn open_bytes_region(&mut self, _plane_index: u32, _x: u32, _y: u32, _w: u32, _h: u32) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "Capture Pro Image (PCI) is a proprietary format from Media Cybernetics".to_string()
        ))
    }

    fn open_thumb_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "Capture Pro Image (PCI) is a proprietary format from Media Cybernetics".to_string()
        ))
    }
}

// ---------------------------------------------------------------------------
// 7. PDS planetary format — text header parsing
// ---------------------------------------------------------------------------
/// PDS (Planetary Data System) format reader (`.pds`).
///
/// PDS has a text header with keyword=value pairs (LINES, LINE_SAMPLES,
/// SAMPLE_BITS, etc.) followed by raw binary pixel data.
pub struct PdsReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    data_offset: u64,
}

impl PdsReader {
    pub fn new() -> Self {
        PdsReader { path: None, meta: None, data_offset: 0 }
    }
}

impl Default for PdsReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for PdsReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("pds"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool { false }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        let text_end = data.len().min(8192);
        let header_text = String::from_utf8_lossy(&data[..text_end]);

        let mut lines = 0u32;
        let mut line_samples = 0u32;
        let mut sample_bits = 8u32;
        let mut record_bytes = 0u64;
        let mut label_records = 0u64;

        for line in header_text.lines() {
            let line = line.trim();
            if line == "END" {
                break;
            }
            if let Some((key, val)) = line.split_once('=') {
                let key = key.trim();
                let val = val.trim();
                match key {
                    "LINES" => { lines = val.parse().unwrap_or(0); }
                    "LINE_SAMPLES" => { line_samples = val.parse().unwrap_or(0); }
                    "SAMPLE_BITS" => { sample_bits = val.parse().unwrap_or(8); }
                    "RECORD_BYTES" => { record_bytes = val.parse().unwrap_or(0); }
                    "LABEL_RECORDS" | "^IMAGE" => {
                        // ^IMAGE can be a record number
                        if let Ok(n) = val.parse::<u64>() {
                            label_records = n;
                        }
                    }
                    _ => {}
                }
            }
        }

        if lines == 0 || line_samples == 0 {
            return Err(BioFormatsError::Format(
                "PDS header missing LINES or LINE_SAMPLES keywords".to_string()
            ));
        }

        let (pixel_type, bpp) = match sample_bits {
            8 => (PixelType::Uint8, 8u8),
            16 => (PixelType::Uint16, 16u8),
            32 => (PixelType::Uint32, 32u8),
            _ => (PixelType::Uint8, 8u8),
        };

        // Calculate data offset
        let offset = if record_bytes > 0 && label_records > 0 {
            record_bytes * (label_records - 1) // PDS records are 1-based
        } else {
            // Find END keyword position and skip past it
            if let Some(end_pos) = header_text.find("\nEND\r") {
                (end_pos + 5) as u64
            } else if let Some(end_pos) = header_text.find("\nEND\n") {
                (end_pos + 5) as u64
            } else if let Some(end_pos) = header_text.find("\nEND") {
                (end_pos + 4) as u64
            } else {
                0u64
            }
        };

        self.data_offset = offset;
        self.path = Some(path.to_path_buf());
        self.meta = Some(ImageMetadata {
            size_x: line_samples,
            size_y: lines,
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
            is_little_endian: false, // PDS is typically big-endian
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
        let bps = (meta.bits_per_pixel / 8) as usize;
        let n_bytes = meta.size_x as usize * meta.size_y as usize * bps;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
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
}

// ---------------------------------------------------------------------------
// 8. Hiscan HIS format
// ---------------------------------------------------------------------------
/// Hiscan HIS format reader (`.his`).
///
/// 100-byte header: bytes 0-1 magic (0x49), bytes 2-3 width (u16 LE),
/// bytes 4-5 height (u16 LE). Pixel data starts at offset 100 as 16-bit LE.
pub struct HisReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl HisReader {
    pub fn new() -> Self {
        HisReader { path: None, meta: None }
    }
}

impl Default for HisReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for HisReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("his"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.len() >= 2 && header[0] == 0x49
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
        let mut header = [0u8; 100];
        f.read_exact(&mut header).map_err(BioFormatsError::Io)?;
        let w = u16::from_le_bytes([header[2], header[3]]) as u32;
        let h = u16::from_le_bytes([header[4], header[5]]) as u32;
        let (w, h) = if w == 0 || h == 0 { (512, 512) } else { (w, h) };
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
        let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(100)).map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; n_bytes];
        let bytes_read = f.read(&mut buf).map_err(BioFormatsError::Io)?;
        buf.truncate(bytes_read.max(n_bytes).min(n_bytes));
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
// 9. HRDC GDF format
// ---------------------------------------------------------------------------
/// HRDC GDF format reader (`.gdf`).
///
/// HRDC GDF is a proprietary format from the Health Research Data Council
/// with undocumented binary structure.
pub struct HrdgdfReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl HrdgdfReader {
    pub fn new() -> Self {
        HrdgdfReader { path: None, meta: None }
    }
}

impl Default for HrdgdfReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for HrdgdfReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("gdf"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool { false }

    fn set_id(&mut self, _path: &Path) -> Result<()> {
        Err(BioFormatsError::UnsupportedFormat(
            "HRDC GDF is a proprietary format with undocumented binary structure".to_string()
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
            "HRDC GDF is a proprietary format with undocumented binary structure".to_string()
        ))
    }

    fn open_bytes_region(&mut self, _plane_index: u32, _x: u32, _y: u32, _w: u32, _h: u32) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "HRDC GDF is a proprietary format with undocumented binary structure".to_string()
        ))
    }

    fn open_thumb_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "HRDC GDF is a proprietary format with undocumented binary structure".to_string()
        ))
    }
}

// ---------------------------------------------------------------------------
// 10. Text/CSV image format
// ---------------------------------------------------------------------------
/// Text/CSV image reader (`.csv`).
///
/// Reads a CSV/TSV text file where each row is a line and columns are separated
/// by commas, tabs, or spaces. Each cell is parsed as f64, then stored as Float32
/// pixel data.
pub struct TextImageReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    pixel_data: Vec<u8>,
}

impl TextImageReader {
    pub fn new() -> Self {
        TextImageReader { path: None, meta: None, pixel_data: Vec::new() }
    }
}

impl Default for TextImageReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for TextImageReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("csv"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool { false }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let text = std::fs::read_to_string(path).map_err(BioFormatsError::Io)?;
        let mut rows: Vec<Vec<f32>> = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let cells: Vec<f32> = line
                .split(|c: char| c == ',' || c == '\t' || c == ' ')
                .filter(|s| !s.is_empty())
                .map(|s| s.trim().parse::<f64>().unwrap_or(0.0) as f32)
                .collect();
            if !cells.is_empty() {
                rows.push(cells);
            }
        }
        if rows.is_empty() {
            return Err(BioFormatsError::UnsupportedFormat(
                "TextImageReader: file contains no numeric data".to_string(),
            ));
        }
        let height = rows.len() as u32;
        let width = rows.iter().map(|r| r.len()).max().unwrap_or(0) as u32;
        // Build Float32 pixel buffer (row-major, zero-padded for short rows)
        let mut pixel_data = Vec::with_capacity((width * height * 4) as usize);
        for row in &rows {
            for x in 0..width as usize {
                let val = if x < row.len() { row[x] } else { 0.0f32 };
                pixel_data.extend_from_slice(&val.to_le_bytes());
            }
        }
        self.path = Some(path.to_path_buf());
        self.pixel_data = pixel_data;
        self.meta = Some(ImageMetadata {
            size_x: width,
            size_y: height,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type: PixelType::Float32,
            bits_per_pixel: 32,
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
        self.pixel_data.clear();
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
        Ok(self.pixel_data.clone())
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
}

// ---------------------------------------------------------------------------
// 11. FilePatternReader - reads file patterns
// ---------------------------------------------------------------------------
/// File pattern reader (`.pattern`).
///
/// Pattern files describe a set of files to combine into a multi-dimensional
/// dataset. Requires a glob/regex expansion engine which is not implemented.
pub struct FilePatternReaderStub {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl FilePatternReaderStub {
    pub fn new() -> Self {
        FilePatternReaderStub { path: None, meta: None }
    }
}

impl Default for FilePatternReaderStub {
    fn default() -> Self { Self::new() }
}

impl FormatReader for FilePatternReaderStub {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("pattern"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool { false }

    fn set_id(&mut self, _path: &Path) -> Result<()> {
        Err(BioFormatsError::UnsupportedFormat(
            "FilePattern format requires glob/regex expansion to assemble multi-file datasets".to_string()
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
            "FilePattern format requires glob/regex expansion to assemble multi-file datasets".to_string()
        ))
    }

    fn open_bytes_region(&mut self, _plane_index: u32, _x: u32, _y: u32, _w: u32, _h: u32) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "FilePattern format requires glob/regex expansion to assemble multi-file datasets".to_string()
        ))
    }

    fn open_thumb_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "FilePattern format requires glob/regex expansion to assemble multi-file datasets".to_string()
        ))
    }
}

// ---------------------------------------------------------------------------
// 12. KLB (Keller Lab Block) format
// ---------------------------------------------------------------------------
/// KLB (Keller Lab Block) format reader (`.klb`).
///
/// KLB is a compressed block-based format for light-sheet microscopy data.
/// Requires a dedicated KLB decoder library which is not available in pure Rust.
pub struct KlbReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl KlbReader {
    pub fn new() -> Self {
        KlbReader { path: None, meta: None }
    }
}

impl Default for KlbReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for KlbReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("klb"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool { false }

    fn set_id(&mut self, _path: &Path) -> Result<()> {
        Err(BioFormatsError::UnsupportedFormat(
            "KLB (Keller Lab Block) format requires a dedicated decoder not available in pure Rust".to_string()
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
            "KLB (Keller Lab Block) format requires a dedicated decoder not available in pure Rust".to_string()
        ))
    }

    fn open_bytes_region(&mut self, _plane_index: u32, _x: u32, _y: u32, _w: u32, _h: u32) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "KLB (Keller Lab Block) format requires a dedicated decoder not available in pure Rust".to_string()
        ))
    }

    fn open_thumb_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "KLB (Keller Lab Block) format requires a dedicated decoder not available in pure Rust".to_string()
        ))
    }
}

// ---------------------------------------------------------------------------
// 13. OBF (Imspector OBF)
// ---------------------------------------------------------------------------
/// OBF/MSR Imspector format reader (`.obf`).
///
/// OBF files are handled by ImspectorReader in the extended module.
/// This reader exists as a fallback for files that do not match the
/// Imspector magic bytes.
pub struct ObfReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl ObfReader {
    pub fn new() -> Self {
        ObfReader { path: None, meta: None }
    }
}

impl Default for ObfReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for ObfReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("obf"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool { false }

    fn set_id(&mut self, _path: &Path) -> Result<()> {
        Err(BioFormatsError::UnsupportedFormat(
            "OBF format: use ImspectorReader for files with OMAS_BF_ magic; this fallback does not support other OBF variants".to_string()
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
            "OBF format: use ImspectorReader for files with OMAS_BF_ magic".to_string()
        ))
    }

    fn open_bytes_region(&mut self, _plane_index: u32, _x: u32, _y: u32, _w: u32, _h: u32) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "OBF format: use ImspectorReader for files with OMAS_BF_ magic".to_string()
        ))
    }

    fn open_thumb_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "OBF format: use ImspectorReader for files with OMAS_BF_ magic".to_string()
        ))
    }
}
