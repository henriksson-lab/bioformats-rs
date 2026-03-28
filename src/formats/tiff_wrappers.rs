//! Thin TIFF-wrapper readers for formats that are TIFF-based but identified
//! only by file extension (no distinct magic bytes beyond TIFF itself).
//!
//! All readers delegate all pixel / metadata work to `crate::tiff::TiffReader`.

use std::path::Path;

use crate::common::error::Result;
use crate::common::metadata::ImageMetadata;
use crate::common::reader::FormatReader;

// ---------------------------------------------------------------------------
// Macro to generate a thin TIFF-wrapper reader.
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
// 1. Hamamatsu NDPI whole-slide — enriched reader
// ---------------------------------------------------------------------------
/// Hamamatsu NDPI whole-slide image (TIFF-based, `.ndpi`).
///
/// Enriches metadata with NDPI-specific vendor tags:
/// - Tag 65421: magnification (float)
/// - Tag 65422: x-offset (float)
/// - Tag 65423: y-offset (float)
/// - Tag 65441: z-offset (float)
/// - Tag 65442: source lens (ASCII)
/// - Tag 65449: NDPI JPEG quality (long)
pub struct NdpiReader {
    inner: crate::tiff::TiffReader,
}

impl NdpiReader {
    pub fn new() -> Self {
        NdpiReader { inner: crate::tiff::TiffReader::new() }
    }

    fn enrich_metadata(&mut self) {
        // Read vendor tags from the first IFD
        let vendor = {
            let ifd = match self.inner.ifd(0) {
                Some(ifd) => ifd,
                None => return,
            };
            let mut meta = std::collections::HashMap::new();
            // Tag 65421 = magnification (stored as FLOAT)
            if let Some(v) = ifd.get(65421) {
                if let Some(vals) = v.as_vec_f32() {
                    if let Some(&mag) = vals.first() {
                        meta.insert("ndpi.magnification".to_string(),
                            crate::common::metadata::MetadataValue::Float(mag as f64));
                    }
                }
            }
            // Tag 65422 = x offset (FLOAT)
            if let Some(v) = ifd.get(65422) {
                if let Some(vals) = v.as_vec_f32() {
                    if let Some(&x) = vals.first() {
                        meta.insert("ndpi.offset.x".to_string(),
                            crate::common::metadata::MetadataValue::Float(x as f64));
                    }
                }
            }
            // Tag 65423 = y offset (FLOAT)
            if let Some(v) = ifd.get(65423) {
                if let Some(vals) = v.as_vec_f32() {
                    if let Some(&y) = vals.first() {
                        meta.insert("ndpi.offset.y".to_string(),
                            crate::common::metadata::MetadataValue::Float(y as f64));
                    }
                }
            }
            // Tag 65442 = source lens (ASCII)
            if let Some(v) = ifd.get(65442) {
                if let Some(s) = v.as_str() {
                    meta.insert("ndpi.source_lens".to_string(),
                        crate::common::metadata::MetadataValue::String(s.to_string()));
                }
            }
            meta
        };

        if let Some(s) = self.inner.series_list_mut().first_mut() {
            for (k, v) in vendor {
                s.metadata.series_metadata.insert(k, v);
            }
        }
    }
}

impl Default for NdpiReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for NdpiReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("ndpi"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool { false }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.inner.set_id(path)?;
        self.enrich_metadata();
        Ok(())
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
    fn resolution_count(&self) -> usize { self.inner.resolution_count() }
    fn set_resolution(&mut self, level: usize) -> Result<()> { self.inner.set_resolution(level) }
}

// ---------------------------------------------------------------------------
// 2. Leica SCN whole-slide — enriched reader
// ---------------------------------------------------------------------------
/// Leica SCN whole-slide image (TIFF-based, `.scn`).
///
/// Parses Leica XML metadata from the ImageDescription tag to extract
/// magnification, pixel size, and scanner info.
pub struct LeicaScnReader {
    inner: crate::tiff::TiffReader,
}

impl LeicaScnReader {
    pub fn new() -> Self {
        LeicaScnReader { inner: crate::tiff::TiffReader::new() }
    }

    fn enrich_metadata(&mut self) {
        let desc = {
            let series = self.inner.series_list();
            if series.is_empty() { return; }
            series[0].metadata.series_metadata.get("ImageDescription")
                .and_then(|v| if let crate::common::metadata::MetadataValue::String(s) = v {
                    Some(s.clone())
                } else { None })
        };
        let Some(desc) = desc else { return };
        // Leica SCN stores XML with <scn ...> root element
        if !desc.contains("<scn") && !desc.contains("<SCN") { return; }

        let mut vendor = std::collections::HashMap::new();

        // Extract objectiveMagnification from XML
        let lower = desc.to_ascii_lowercase();
        if let Some(pos) = lower.find("objectivemagnification") {
            // Look for the value in nearby attribute or element text
            let rest = &desc[pos..];
            if let Some(eq) = rest.find('=') {
                let val_start = &rest[eq + 1..];
                let val = val_start.trim_start_matches(|c: char| c == '"' || c == '\'' || c.is_whitespace());
                let end = val.find(|c: char| c == '"' || c == '\'' || c == '<' || c == '/' || c.is_whitespace())
                    .unwrap_or(val.len());
                if let Ok(mag) = val[..end].parse::<f64>() {
                    vendor.insert("leica.objective_magnification".to_string(),
                        crate::common::metadata::MetadataValue::Float(mag));
                }
            }
        }

        let series = self.inner.series_list_mut();
        if let Some(s) = series.first_mut() {
            for (k, v) in vendor {
                s.metadata.series_metadata.insert(k, v);
            }
        }
    }
}

impl Default for LeicaScnReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for LeicaScnReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("scn"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool { false }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.inner.set_id(path)?;
        self.enrich_metadata();
        Ok(())
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
    fn resolution_count(&self) -> usize { self.inner.resolution_count() }
    fn set_resolution(&mut self, level: usize) -> Result<()> { self.inner.set_resolution(level) }
}

// ---------------------------------------------------------------------------
// 3. Ventana/Roche BIF whole-slide
// ---------------------------------------------------------------------------
tiff_wrapper! {
    /// Ventana/Roche BIF whole-slide image (TIFF-based, `.bif`).
    pub struct VentanaReader;
    extensions: ["bif"];
}

// ---------------------------------------------------------------------------
// 4. Nikon NIS-Elements TIFF (metadata embedded in TIFF description)
// ---------------------------------------------------------------------------
tiff_wrapper! {
    /// Nikon NIS-Elements annotated TIFF (`.tiff`).
    pub struct NikonElementsTiffReader;
    extensions: ["tiff"];
}

// ---------------------------------------------------------------------------
// 5. FEI-annotated TIFF (extension-only fallback for `.tiff`)
// ---------------------------------------------------------------------------
tiff_wrapper! {
    /// FEI-annotated TIFF (extension-only fallback, `.tiff`).
    pub struct FeiTiffReader;
    extensions: ["tiff"];
}

// ---------------------------------------------------------------------------
// 6. Olympus SIS TIFF metadata (`.tif`)
// ---------------------------------------------------------------------------
tiff_wrapper! {
    /// Olympus SIS TIFF (`.tif`).
    pub struct OlympusSisTiffReader;
    extensions: ["tif"];
}

// ---------------------------------------------------------------------------
// 7. Improvision/Volocity annotated TIFF (`.tif`)
// ---------------------------------------------------------------------------
tiff_wrapper! {
    /// Improvision/Volocity annotated TIFF (`.tif`).
    pub struct ImprovisionTiffReader;
    extensions: ["tif"];
}

// ---------------------------------------------------------------------------
// 8. Zeiss ApoTome TIFF (`.tif`)
// ---------------------------------------------------------------------------
tiff_wrapper! {
    /// Zeiss ApoTome TIFF (`.tif`).
    pub struct ZeissApotomeTiffReader;
    extensions: ["tif"];
}

// ---------------------------------------------------------------------------
// 9. Olympus Fluoview FV300 (`.tif`) — enriched reader
// ---------------------------------------------------------------------------
/// Olympus Fluoview FV300 TIFF (`.tif`).
///
/// Enriches metadata from the ImageDescription tag which may contain
/// Fluoview-specific key=value pairs like `[Acquisition Parameters]`.
pub struct FluoviewTiffReader {
    inner: crate::tiff::TiffReader,
}

impl FluoviewTiffReader {
    pub fn new() -> Self {
        FluoviewTiffReader { inner: crate::tiff::TiffReader::new() }
    }

    fn enrich_metadata(&mut self) {
        let desc = {
            let series = self.inner.series_list();
            if series.is_empty() { return; }
            series[0].metadata.series_metadata.get("ImageDescription")
                .and_then(|v| if let crate::common::metadata::MetadataValue::String(s) = v {
                    Some(s.clone())
                } else { None })
        };
        let Some(desc) = desc else { return };
        if !desc.contains("[Acquisition Parameters]") && !desc.contains("FluoView") { return; }

        let mut vendor = std::collections::HashMap::new();
        // Parse INI-style key=value pairs
        for line in desc.lines() {
            let line = line.trim();
            if let Some((key, val)) = line.split_once('=') {
                let key = key.trim();
                let val = val.trim();
                if !key.is_empty() && !key.starts_with('[') {
                    vendor.insert(
                        format!("fluoview.{}", key),
                        crate::common::metadata::MetadataValue::String(val.to_string()),
                    );
                }
            }
        }

        let series = self.inner.series_list_mut();
        if let Some(s) = series.first_mut() {
            for (k, v) in vendor {
                s.metadata.series_metadata.insert(k, v);
            }
        }
    }
}

impl Default for FluoviewTiffReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for FluoviewTiffReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("tif"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool { false }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.inner.set_id(path)?;
        self.enrich_metadata();
        Ok(())
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
    fn resolution_count(&self) -> usize { self.inner.resolution_count() }
    fn set_resolution(&mut self, level: usize) -> Result<()> { self.inner.set_resolution(level) }
}

// ---------------------------------------------------------------------------
// 10. Molecular Devices plate TIFF (`.tif`)
// ---------------------------------------------------------------------------
tiff_wrapper! {
    /// Molecular Devices plate TIFF (`.tif`).
    pub struct MolecularDevicesTiffReader;
    extensions: ["tif"];
}
