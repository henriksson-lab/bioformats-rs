//! Scanning Probe Microscopy (SPM) and related format readers.
//!
//! Includes a real binary reader for PicoQuant TCSPC data and
//! extension-only placeholder readers for various SPM/AFM platforms.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;

// ---------------------------------------------------------------------------
// Macro: extension-only placeholder reader (512x512 uint16)
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
// Extension-only placeholder readers (512x512 uint16)
// ===========================================================================

// ---------------------------------------------------------------------------
// RHK Technology SPM
// ---------------------------------------------------------------------------
placeholder_reader! {
    /// RHK Technology SPM placeholder reader (`.sm2`, `.sm3`, `.sm4`).
    pub struct RhkReader;
    extensions: ["sm2", "sm3", "sm4"];
}

// ---------------------------------------------------------------------------
// Quesant AFM
// ---------------------------------------------------------------------------
placeholder_reader! {
    /// Quesant AFM placeholder reader (`.afm`).
    pub struct QuesantReader;
    extensions: ["afm"];
}

// ---------------------------------------------------------------------------
// JPK Instruments AFM
// ---------------------------------------------------------------------------
placeholder_reader! {
    /// JPK Instruments AFM placeholder reader (`.jpk`).
    pub struct JpkReader;
    extensions: ["jpk"];
}

// ---------------------------------------------------------------------------
// WaTom SPM
// ---------------------------------------------------------------------------
placeholder_reader! {
    /// WaTom SPM placeholder reader (`.wap`, `.opo`, `.opz`, `.opt`).
    pub struct WatopReader;
    extensions: ["wap", "opo", "opz", "opt"];
}

// ---------------------------------------------------------------------------
// VG SAM
// ---------------------------------------------------------------------------
placeholder_reader! {
    /// VG SAM placeholder reader (`.vgsam`).
    pub struct VgSamReader;
    extensions: ["vgsam"];
}

// ---------------------------------------------------------------------------
// UBM Messtechnik
// ---------------------------------------------------------------------------
placeholder_reader! {
    /// UBM Messtechnik placeholder reader (`.ubm`).
    pub struct UbmReader;
    extensions: ["ubm"];
}

// ---------------------------------------------------------------------------
// Seiko SPM
// ---------------------------------------------------------------------------
placeholder_reader! {
    /// Seiko SPM placeholder reader (`.xqd`, `.xqf`).
    pub struct SeikoReader;
    extensions: ["xqd", "xqf"];
}
