//! Additional FLIM, flow cytometry, and miscellaneous imaging format readers.
//!
//! Includes FlowSightReader with basic binary header inspection and many
//! extension-only placeholder readers.

use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::ImageMetadata;
use crate::common::reader::FormatReader;

// ---------------------------------------------------------------------------
// Macros
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

macro_rules! placeholder_reader_u16_small {
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
// 1. Amnis FlowSight (.cif)
// ---------------------------------------------------------------------------
/// Amnis FlowSight CIF format (`.cif`).
///
/// Returns a 64x64 uint16 placeholder; full decoding not implemented.
pub struct FlowSightReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl FlowSightReader {
    pub fn new() -> Self {
        FlowSightReader { path: None, meta: None }
    }
}

impl Default for FlowSightReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for FlowSightReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        matches!(
            path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref(),
            Some("cif")
        )
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool { false }

    fn set_id(&mut self, _path: &Path) -> Result<()> {
        Err(BioFormatsError::UnsupportedFormat(
            "FlowSight CIF format reading is not yet implemented".to_string()
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
            "FlowSight CIF format reading is not yet implemented".to_string()
        ))
    }

    fn open_bytes_region(&mut self, _plane_index: u32, _x: u32, _y: u32, _w: u32, _h: u32) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "FlowSight CIF format reading is not yet implemented".to_string()
        ))
    }

    fn open_thumb_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "FlowSight CIF format reading is not yet implemented".to_string()
        ))
    }
}

// ---------------------------------------------------------------------------
// 2. Amnis/Luminex IM3 — 64x64 uint16 placeholder
// ---------------------------------------------------------------------------
placeholder_reader_u16_small! {
    /// Amnis/Luminex IM3 format placeholder reader (`.im3`).
    pub struct Im3Reader;
    extensions: ["im3"];
    magic_bytes: false;
}

// ---------------------------------------------------------------------------
// 3. 3i SlideBook 7 — 64x64 uint16 placeholder
// ---------------------------------------------------------------------------
placeholder_reader_u16_small! {
    /// 3i SlideBook 7 format placeholder reader (`.sld`).
    pub struct SlideBook7Reader;
    extensions: ["sld"];
    magic_bytes: false;
}

// ---------------------------------------------------------------------------
// 4. NDPI Set — TIFF delegate
// ---------------------------------------------------------------------------
/// NDPI Set format reader (`.ndpis`).
///
/// Delegates to `TiffReader` since NDPI files reference TIFF data.
pub struct NdpisReader {
    inner: crate::tiff::TiffReader,
}

impl NdpisReader {
    pub fn new() -> Self {
        NdpisReader { inner: crate::tiff::TiffReader::new() }
    }
}

impl Default for NdpisReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for NdpisReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("ndpis"))
    }
    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool { false }
    fn set_id(&mut self, path: &Path) -> Result<()> { self.inner.set_id(path) }
    fn close(&mut self) -> Result<()> { self.inner.close() }
    fn series_count(&self) -> usize { self.inner.series_count() }
    fn set_series(&mut self, s: usize) -> Result<()> { self.inner.set_series(s) }
    fn series(&self) -> usize { self.inner.series() }
    fn metadata(&self) -> &ImageMetadata { self.inner.metadata() }
    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> { self.inner.open_bytes(p) }
    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> { self.inner.open_bytes_region(p, x, y, w, h) }
    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> { self.inner.open_thumb_bytes(p) }
    fn resolution_count(&self) -> usize { self.inner.resolution_count() }
    fn set_resolution(&mut self, level: usize) -> Result<()> { self.inner.set_resolution(level) }
}

// ---------------------------------------------------------------------------
// 5. iVision IPM
// ---------------------------------------------------------------------------
/// iVision format reader (`.ipm`).
///
/// iVision is a proprietary format from BioVision Technologies with
/// undocumented binary structure.
pub struct IvisionReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl IvisionReader {
    pub fn new() -> Self {
        IvisionReader { path: None, meta: None }
    }
}

impl Default for IvisionReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for IvisionReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("ipm"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool { false }

    fn set_id(&mut self, _path: &Path) -> Result<()> {
        Err(BioFormatsError::UnsupportedFormat(
            "iVision IPM is a proprietary format from BioVision Technologies".to_string()
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
            "iVision IPM is a proprietary format from BioVision Technologies".to_string()
        ))
    }

    fn open_bytes_region(&mut self, _plane_index: u32, _x: u32, _y: u32, _w: u32, _h: u32) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "iVision IPM is a proprietary format from BioVision Technologies".to_string()
        ))
    }

    fn open_thumb_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "iVision IPM is a proprietary format from BioVision Technologies".to_string()
        ))
    }
}

// ---------------------------------------------------------------------------
// 6. Aperio AFI — TIFF delegate
// ---------------------------------------------------------------------------
/// Aperio AFI fluorescence format reader (`.afi`).
///
/// AFI files use TIFF data; delegates to `TiffReader`.
pub struct AfiFluorescenceReader {
    inner: crate::tiff::TiffReader,
}

impl AfiFluorescenceReader {
    pub fn new() -> Self {
        AfiFluorescenceReader { inner: crate::tiff::TiffReader::new() }
    }
}

impl Default for AfiFluorescenceReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for AfiFluorescenceReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("afi"))
    }
    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool { false }
    fn set_id(&mut self, path: &Path) -> Result<()> { self.inner.set_id(path) }
    fn close(&mut self) -> Result<()> { self.inner.close() }
    fn series_count(&self) -> usize { self.inner.series_count() }
    fn set_series(&mut self, s: usize) -> Result<()> { self.inner.set_series(s) }
    fn series(&self) -> usize { self.inner.series() }
    fn metadata(&self) -> &ImageMetadata { self.inner.metadata() }
    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> { self.inner.open_bytes(p) }
    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> { self.inner.open_bytes_region(p, x, y, w, h) }
    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> { self.inner.open_thumb_bytes(p) }
    fn resolution_count(&self) -> usize { self.inner.resolution_count() }
    fn set_resolution(&mut self, level: usize) -> Result<()> { self.inner.set_resolution(level) }
}

// ---------------------------------------------------------------------------
// 7. Imaris TIFF — TIFF delegate
// ---------------------------------------------------------------------------
/// Imaris TIFF format reader (`.ims`).
///
/// Imaris TIFF files are valid TIFFs; delegates to `TiffReader`.
pub struct ImarisTiffReader {
    inner: crate::tiff::TiffReader,
}

impl ImarisTiffReader {
    pub fn new() -> Self {
        ImarisTiffReader { inner: crate::tiff::TiffReader::new() }
    }
}

impl Default for ImarisTiffReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for ImarisTiffReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("ims"))
    }
    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool { false }
    fn set_id(&mut self, path: &Path) -> Result<()> { self.inner.set_id(path) }
    fn close(&mut self) -> Result<()> { self.inner.close() }
    fn series_count(&self) -> usize { self.inner.series_count() }
    fn set_series(&mut self, s: usize) -> Result<()> { self.inner.set_series(s) }
    fn series(&self) -> usize { self.inner.series() }
    fn metadata(&self) -> &ImageMetadata { self.inner.metadata() }
    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> { self.inner.open_bytes(p) }
    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> { self.inner.open_bytes_region(p, x, y, w, h) }
    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> { self.inner.open_thumb_bytes(p) }
    fn resolution_count(&self) -> usize { self.inner.resolution_count() }
    fn set_resolution(&mut self, level: usize) -> Result<()> { self.inner.set_resolution(level) }
}

// ---------------------------------------------------------------------------
// 8. Leica XLEF — TIFF delegate
// ---------------------------------------------------------------------------
/// Leica XLEF format reader (`.xlef`).
///
/// XLEF files contain embedded TIFF data; delegates to `TiffReader`.
pub struct XlefReader {
    inner: crate::tiff::TiffReader,
}

impl XlefReader {
    pub fn new() -> Self {
        XlefReader { inner: crate::tiff::TiffReader::new() }
    }
}

impl Default for XlefReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for XlefReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("xlef"))
    }
    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool { false }
    fn set_id(&mut self, path: &Path) -> Result<()> { self.inner.set_id(path) }
    fn close(&mut self) -> Result<()> { self.inner.close() }
    fn series_count(&self) -> usize { self.inner.series_count() }
    fn set_series(&mut self, s: usize) -> Result<()> { self.inner.set_series(s) }
    fn series(&self) -> usize { self.inner.series() }
    fn metadata(&self) -> &ImageMetadata { self.inner.metadata() }
    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> { self.inner.open_bytes(p) }
    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> { self.inner.open_bytes_region(p, x, y, w, h) }
    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> { self.inner.open_thumb_bytes(p) }
    fn resolution_count(&self) -> usize { self.inner.resolution_count() }
    fn set_resolution(&mut self, level: usize) -> Result<()> { self.inner.set_resolution(level) }
}

// ---------------------------------------------------------------------------
// 9. Olympus OIR
// ---------------------------------------------------------------------------
/// Olympus OIR format reader (`.oir`).
///
/// Olympus OIR format requires OLE2 container parsing with proprietary
/// internal structure specific to Olympus FluoView software.
pub struct OirReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl OirReader {
    pub fn new() -> Self {
        OirReader { path: None, meta: None }
    }
}

impl Default for OirReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for OirReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("oir"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool { false }

    fn set_id(&mut self, _path: &Path) -> Result<()> {
        Err(BioFormatsError::UnsupportedFormat(
            "Olympus OIR format requires OLE2 container parsing with proprietary Olympus FluoView structure".to_string()
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
            "Olympus OIR format requires OLE2 container parsing".to_string()
        ))
    }

    fn open_bytes_region(&mut self, _plane_index: u32, _x: u32, _y: u32, _w: u32, _h: u32) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "Olympus OIR format requires OLE2 container parsing".to_string()
        ))
    }

    fn open_thumb_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "Olympus OIR format requires OLE2 container parsing".to_string()
        ))
    }
}

// ---------------------------------------------------------------------------
// 10. Olympus cellSens VSI — TIFF-based delegate
// ---------------------------------------------------------------------------
/// Olympus cellSens VSI format reader (`.vsi`).
///
/// VSI files are TIFF-based with ETS companion files. Delegates to TiffReader
/// for the base TIFF structure.
pub struct CellSensReader {
    inner: crate::tiff::TiffReader,
}

impl CellSensReader {
    pub fn new() -> Self {
        CellSensReader { inner: crate::tiff::TiffReader::new() }
    }
}

impl Default for CellSensReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for CellSensReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("vsi"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool { false }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.inner.set_id(path).map_err(|_| BioFormatsError::UnsupportedFormat(
            "Olympus cellSens VSI: could not parse as TIFF (may require ETS companion files)".to_string()
        ))
    }

    fn close(&mut self) -> Result<()> { self.inner.close() }
    fn series_count(&self) -> usize { self.inner.series_count() }
    fn set_series(&mut self, s: usize) -> Result<()> { self.inner.set_series(s) }
    fn series(&self) -> usize { self.inner.series() }
    fn metadata(&self) -> &ImageMetadata { self.inner.metadata() }
    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> { self.inner.open_bytes(p) }
    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        self.inner.open_bytes_region(p, x, y, w, h)
    }
    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> { self.inner.open_thumb_bytes(p) }
}

// ---------------------------------------------------------------------------
// 11. Volocity clipping ACFF
// ---------------------------------------------------------------------------
/// Volocity clipping format reader (`.acff`).
///
/// Volocity clipping files use OLE2/Compound Document format which requires
/// a dedicated OLE2 container parser.
pub struct VolocityClippingReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl VolocityClippingReader {
    pub fn new() -> Self {
        VolocityClippingReader { path: None, meta: None }
    }
}

impl Default for VolocityClippingReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for VolocityClippingReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("acff"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool { false }

    fn set_id(&mut self, _path: &Path) -> Result<()> {
        Err(BioFormatsError::UnsupportedFormat(
            "Volocity clipping format requires OLE2/Compound Document container parsing".to_string()
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
            "Volocity clipping format requires OLE2/Compound Document container parsing".to_string()
        ))
    }

    fn open_bytes_region(&mut self, _plane_index: u32, _x: u32, _y: u32, _w: u32, _h: u32) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "Volocity clipping format requires OLE2/Compound Document container parsing".to_string()
        ))
    }

    fn open_thumb_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "Volocity clipping format requires OLE2/Compound Document container parsing".to_string()
        ))
    }
}

// ---------------------------------------------------------------------------
// 12. Bruker MicroCT — TIFF delegate
// ---------------------------------------------------------------------------
/// Bruker MicroCT format reader (`.ctf`).
///
/// MicroCT files use TIFF data; delegates to `TiffReader`.
pub struct MicroCtReader {
    inner: crate::tiff::TiffReader,
}

impl MicroCtReader {
    pub fn new() -> Self {
        MicroCtReader { inner: crate::tiff::TiffReader::new() }
    }
}

impl Default for MicroCtReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for MicroCtReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("ctf"))
    }
    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool { false }
    fn set_id(&mut self, path: &Path) -> Result<()> { self.inner.set_id(path) }
    fn close(&mut self) -> Result<()> { self.inner.close() }
    fn series_count(&self) -> usize { self.inner.series_count() }
    fn set_series(&mut self, s: usize) -> Result<()> { self.inner.set_series(s) }
    fn series(&self) -> usize { self.inner.series() }
    fn metadata(&self) -> &ImageMetadata { self.inner.metadata() }
    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> { self.inner.open_bytes(p) }
    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> { self.inner.open_bytes_region(p, x, y, w, h) }
    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> { self.inner.open_thumb_bytes(p) }
    fn resolution_count(&self) -> usize { self.inner.resolution_count() }
    fn set_resolution(&mut self, level: usize) -> Result<()> { self.inner.set_resolution(level) }
}

// ---------------------------------------------------------------------------
// 13. Bio-Rad SCN confocal — TIFF delegate
// ---------------------------------------------------------------------------
/// Bio-Rad SCN confocal format reader (`.scn`).
///
/// Bio-Rad SCN confocal files are TIFF-based; delegates to `TiffReader`.
pub struct BioRadScnReader {
    inner: crate::tiff::TiffReader,
}

impl BioRadScnReader {
    pub fn new() -> Self {
        BioRadScnReader { inner: crate::tiff::TiffReader::new() }
    }
}

impl Default for BioRadScnReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for BioRadScnReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("scn"))
    }
    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool { false }
    fn set_id(&mut self, path: &Path) -> Result<()> { self.inner.set_id(path) }
    fn close(&mut self) -> Result<()> { self.inner.close() }
    fn series_count(&self) -> usize { self.inner.series_count() }
    fn set_series(&mut self, s: usize) -> Result<()> { self.inner.set_series(s) }
    fn series(&self) -> usize { self.inner.series() }
    fn metadata(&self) -> &ImageMetadata { self.inner.metadata() }
    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> { self.inner.open_bytes(p) }
    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> { self.inner.open_bytes_region(p, x, y, w, h) }
    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> { self.inner.open_thumb_bytes(p) }
    fn resolution_count(&self) -> usize { self.inner.resolution_count() }
    fn set_resolution(&mut self, level: usize) -> Result<()> { self.inner.set_resolution(level) }
}

// ---------------------------------------------------------------------------
// 14. 3i SlideBook TIFF export — TIFF delegate
// ---------------------------------------------------------------------------
/// 3i SlideBook TIFF export format reader (`.tif`).
///
/// SlideBook TIFF exports are valid TIFFs; delegates to `TiffReader`.
pub struct SlidebookTiffReader {
    inner: crate::tiff::TiffReader,
}

impl SlidebookTiffReader {
    pub fn new() -> Self {
        SlidebookTiffReader { inner: crate::tiff::TiffReader::new() }
    }
}

impl Default for SlidebookTiffReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for SlidebookTiffReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("tif"))
    }
    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool { false }
    fn set_id(&mut self, path: &Path) -> Result<()> { self.inner.set_id(path) }
    fn close(&mut self) -> Result<()> { self.inner.close() }
    fn series_count(&self) -> usize { self.inner.series_count() }
    fn set_series(&mut self, s: usize) -> Result<()> { self.inner.set_series(s) }
    fn series(&self) -> usize { self.inner.series() }
    fn metadata(&self) -> &ImageMetadata { self.inner.metadata() }
    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> { self.inner.open_bytes(p) }
    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> { self.inner.open_bytes_region(p, x, y, w, h) }
    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> { self.inner.open_thumb_bytes(p) }
    fn resolution_count(&self) -> usize { self.inner.resolution_count() }
    fn set_resolution(&mut self, level: usize) -> Result<()> { self.inner.set_resolution(level) }
}
