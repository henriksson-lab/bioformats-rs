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
placeholder_reader! {
    /// Applied Precision format placeholder reader (`.apl`).
    pub struct AplReader;
    extensions: ["apl"];
    magic_bytes: false;
}

// ---------------------------------------------------------------------------
// 2. ARF format
// ---------------------------------------------------------------------------
placeholder_reader! {
    /// ARF format placeholder reader (`.arf`).
    pub struct ArfReader;
    extensions: ["arf"];
    magic_bytes: false;
}

// ---------------------------------------------------------------------------
// 3. I2I format
// ---------------------------------------------------------------------------
placeholder_reader! {
    /// I2I format placeholder reader (`.i2i`).
    pub struct I2iReader;
    extensions: ["i2i"];
    magic_bytes: false;
}

// ---------------------------------------------------------------------------
// 4. JDCE format
// ---------------------------------------------------------------------------
placeholder_reader! {
    /// JDCE format placeholder reader (`.jdce`).
    pub struct JdceReader;
    extensions: ["jdce"];
    magic_bytes: false;
}

// ---------------------------------------------------------------------------
// 5. JPX (JPEG 2000 Part 2)
// ---------------------------------------------------------------------------
placeholder_reader! {
    /// JPX (JPEG 2000 Part 2) format placeholder reader (`.jpx`).
    pub struct JpxReader;
    extensions: ["jpx"];
    magic_bytes: false;
}

// ---------------------------------------------------------------------------
// 6. Capture Pro Image (PCI)
// ---------------------------------------------------------------------------
placeholder_reader! {
    /// Capture Pro Image format placeholder reader (`.pci`).
    pub struct PciReader;
    extensions: ["pci"];
    magic_bytes: false;
}

// ---------------------------------------------------------------------------
// 7. PDS planetary format
// ---------------------------------------------------------------------------
placeholder_reader! {
    /// PDS planetary format placeholder reader (`.pds`).
    pub struct PdsReader;
    extensions: ["pds"];
    magic_bytes: false;
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
placeholder_reader! {
    /// HRDC GDF format placeholder reader (`.gdf`).
    pub struct HrdgdfReader;
    extensions: ["gdf"];
    magic_bytes: false;
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
placeholder_reader! {
    /// File pattern reader placeholder (`.pattern`).
    pub struct FilePatternReaderStub;
    extensions: ["pattern"];
    magic_bytes: false;
}

// ---------------------------------------------------------------------------
// 12. KLB (Keller Lab Block) format
// ---------------------------------------------------------------------------
placeholder_reader! {
    /// KLB (Keller Lab Block) format placeholder reader (`.klb`).
    pub struct KlbReader;
    extensions: ["klb"];
    magic_bytes: false;
}

// ---------------------------------------------------------------------------
// 13. OBF (Imspector OBF)
// ---------------------------------------------------------------------------
placeholder_reader! {
    /// OBF/MSR Imspector format placeholder reader (`.obf`).
    pub struct ObfReader;
    extensions: ["obf"];
    magic_bytes: false;
}
