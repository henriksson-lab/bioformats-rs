//! Placeholder readers for remaining obscure and proprietary formats.
//!
//! All readers are extension-only and return 512×512 uint8 placeholder metadata
//! with zeroed pixel data. Full decoding is not implemented.

use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::ImageMetadata;
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
placeholder_reader! {
    /// Hiscan HIS format placeholder reader (`.his`).
    pub struct HisReader;
    extensions: ["his"];
    magic_bytes: false;
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
placeholder_reader! {
    /// Text/CSV image format placeholder reader (`.csv`).
    pub struct TextImageReader;
    extensions: ["csv"];
    magic_bytes: false;
}
