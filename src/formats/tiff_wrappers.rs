//! Thin TIFF-wrapper readers for formats that are TIFF-based but identified
//! only by file extension (no distinct magic bytes beyond TIFF itself).
//!
//! All readers delegate all pixel / metadata work to `crate::tiff::TiffReader`.

use std::path::Path;

use crate::common::error::Result;
use crate::common::metadata::ImageMetadata;
use crate::common::reader::FormatReader;

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
// 3. Ventana/Roche BIF whole-slide — enriched reader
// ---------------------------------------------------------------------------
/// Ventana/Roche BIF whole-slide image (TIFF-based, `.bif`).
///
/// Parses XML metadata from ImageDescription looking for `<iScan>` elements
/// to extract magnification and scanner parameters.
pub struct VentanaReader {
    inner: crate::tiff::TiffReader,
}

impl VentanaReader {
    pub fn new() -> Self {
        VentanaReader { inner: crate::tiff::TiffReader::new() }
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

        let mut vendor = std::collections::HashMap::new();

        // Ventana stores XML with <iScan> root or similar elements
        if desc.contains("<iScan") || desc.contains("<ventana") || desc.contains("<Ventana") {
            // Extract magnification: look for Magnification="..." or <Magnification>...</Magnification>
            let lower = desc.to_ascii_lowercase();
            if let Some(pos) = lower.find("magnification") {
                let rest = &desc[pos..];
                // Try attribute form: magnification="20"
                if let Some(eq) = rest.find('=') {
                    let val_start = &rest[eq + 1..];
                    let val = val_start.trim_start_matches(|c: char| c == '"' || c == '\'' || c.is_whitespace());
                    let end = val.find(|c: char| c == '"' || c == '\'' || c == '<' || c == '/' || c.is_whitespace())
                        .unwrap_or(val.len());
                    if let Ok(mag) = val[..end].parse::<f64>() {
                        vendor.insert("ventana.magnification".to_string(),
                            crate::common::metadata::MetadataValue::Float(mag));
                    }
                }
            }
        }

        // Also try generic key=value or simple XML element extraction
        for line in desc.lines() {
            let line = line.trim();
            if let Some((key, val)) = line.split_once('=') {
                let key = key.trim().trim_start_matches('<');
                let val = val.trim().trim_matches('"').trim_matches('\'');
                if !key.is_empty() && !val.is_empty() && !vendor.contains_key(&format!("ventana.{}", key.to_ascii_lowercase())) {
                    vendor.insert(
                        format!("ventana.{}", key.to_ascii_lowercase()),
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

impl Default for VentanaReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for VentanaReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("bif"))
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
// 4. Nikon NIS-Elements TIFF — enriched reader
// ---------------------------------------------------------------------------
/// Nikon NIS-Elements annotated TIFF (`.tiff`).
///
/// Parses XML metadata from ImageDescription looking for `<variant>` elements
/// to extract channel info and acquisition parameters.
pub struct NikonElementsTiffReader {
    inner: crate::tiff::TiffReader,
}

impl NikonElementsTiffReader {
    pub fn new() -> Self {
        NikonElementsTiffReader { inner: crate::tiff::TiffReader::new() }
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

        let mut vendor = std::collections::HashMap::new();

        // Nikon NIS-Elements XML uses <variant> elements
        if desc.contains("<variant") || desc.contains("NIS-Elements") || desc.contains("Nikon") {
            // Count channel references
            let channel_count = desc.matches("<Channel").count()
                .max(desc.matches("<channel").count());
            if channel_count > 0 {
                vendor.insert("nikon.channel_count".to_string(),
                    crate::common::metadata::MetadataValue::Int(channel_count as i64));
            }

            // Extract runtype or variant name attributes: name="value"
            // Look for key attributes in <variant> tags
            let lower = desc.to_ascii_lowercase();
            for tag_name in &["runtype", "objectivename", "magnification", "numericaperture"] {
                if let Some(pos) = lower.find(*tag_name) {
                    let rest = &desc[pos..];
                    if let Some(eq) = rest.find('=') {
                        let val_start = &rest[eq + 1..];
                        let val = val_start.trim_start_matches(|c: char| c == '"' || c == '\'' || c.is_whitespace());
                        let end = val.find(|c: char| c == '"' || c == '\'' || c == '<' || c == '/' || c.is_whitespace())
                            .unwrap_or(val.len());
                        if !val[..end].is_empty() {
                            let key = format!("nikon.{}", tag_name);
                            if let Ok(f) = val[..end].parse::<f64>() {
                                vendor.insert(key,
                                    crate::common::metadata::MetadataValue::Float(f));
                            } else {
                                vendor.insert(key,
                                    crate::common::metadata::MetadataValue::String(val[..end].to_string()));
                            }
                        }
                    }
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

impl Default for NikonElementsTiffReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for NikonElementsTiffReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("tiff"))
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
// 5. FEI-annotated TIFF — enriched reader
// ---------------------------------------------------------------------------
/// FEI/ThermoFisher annotated TIFF (`.tiff`).
///
/// Parses ImageDescription for key=value pairs commonly found in FEI
/// electron microscope images (e.g. HV, beam current, pixel size).
pub struct FeiTiffReader {
    inner: crate::tiff::TiffReader,
}

impl FeiTiffReader {
    pub fn new() -> Self {
        FeiTiffReader { inner: crate::tiff::TiffReader::new() }
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

        let mut vendor = std::collections::HashMap::new();

        // FEI images use key=value lines, often with section headers like [User], [Beam], [Scan]
        for line in desc.lines() {
            let line = line.trim();
            if let Some((key, val)) = line.split_once('=') {
                let key = key.trim();
                let val = val.trim();
                if !key.is_empty() && !key.starts_with('[') && !val.is_empty() {
                    let sanitized_key = key.to_ascii_lowercase().replace(' ', "_");
                    if let Ok(f) = val.parse::<f64>() {
                        vendor.insert(
                            format!("fei.{}", sanitized_key),
                            crate::common::metadata::MetadataValue::Float(f),
                        );
                    } else {
                        vendor.insert(
                            format!("fei.{}", sanitized_key),
                            crate::common::metadata::MetadataValue::String(val.to_string()),
                        );
                    }
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

impl Default for FeiTiffReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for FeiTiffReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("tiff"))
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
// 6. Olympus SIS TIFF — enriched reader
// ---------------------------------------------------------------------------
/// Olympus SIS TIFF (`.tif`).
///
/// Parses ImageDescription for pixel calibration and acquisition metadata
/// stored by Olympus SIS software.
pub struct OlympusSisTiffReader {
    inner: crate::tiff::TiffReader,
}

impl OlympusSisTiffReader {
    pub fn new() -> Self {
        OlympusSisTiffReader { inner: crate::tiff::TiffReader::new() }
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

        let mut vendor = std::collections::HashMap::new();

        // Olympus SIS uses key=value or key:value lines for calibration
        for line in desc.lines() {
            let line = line.trim();
            // Try key=value first, then key: value
            let pair = line.split_once('=')
                .or_else(|| line.split_once(':'));
            if let Some((key, val)) = pair {
                let key = key.trim();
                let val = val.trim();
                if key.is_empty() || val.is_empty() || key.starts_with('[') || key.starts_with('<') {
                    continue;
                }
                let sanitized_key = key.to_ascii_lowercase().replace(' ', "_");
                if let Ok(f) = val.parse::<f64>() {
                    vendor.insert(
                        format!("olympus_sis.{}", sanitized_key),
                        crate::common::metadata::MetadataValue::Float(f),
                    );
                } else {
                    vendor.insert(
                        format!("olympus_sis.{}", sanitized_key),
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

impl Default for OlympusSisTiffReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for OlympusSisTiffReader {
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
// 7. Improvision/Volocity annotated TIFF — enriched reader
// ---------------------------------------------------------------------------
/// Improvision/Volocity annotated TIFF (`.tif`).
///
/// Parses ImageDescription for structured metadata stored by
/// Improvision/PerkinElmer Volocity software.
pub struct ImprovisionTiffReader {
    inner: crate::tiff::TiffReader,
}

impl ImprovisionTiffReader {
    pub fn new() -> Self {
        ImprovisionTiffReader { inner: crate::tiff::TiffReader::new() }
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

        let mut vendor = std::collections::HashMap::new();

        // Improvision/Volocity uses key=value or key: value lines
        for line in desc.lines() {
            let line = line.trim();
            let pair = line.split_once('=')
                .or_else(|| line.split_once(':'));
            if let Some((key, val)) = pair {
                let key = key.trim();
                let val = val.trim();
                if key.is_empty() || val.is_empty() || key.starts_with('[') || key.starts_with('<') {
                    continue;
                }
                let sanitized_key = key.to_ascii_lowercase().replace(' ', "_");
                if let Ok(f) = val.parse::<f64>() {
                    vendor.insert(
                        format!("improvision.{}", sanitized_key),
                        crate::common::metadata::MetadataValue::Float(f),
                    );
                } else {
                    vendor.insert(
                        format!("improvision.{}", sanitized_key),
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

impl Default for ImprovisionTiffReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for ImprovisionTiffReader {
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
// 8. Zeiss ApoTome TIFF — enriched reader
// ---------------------------------------------------------------------------
/// Zeiss ApoTome TIFF (`.tif`).
///
/// Parses XML metadata from ImageDescription looking for `<Zeiss>` or
/// ApoTome acquisition parameters.
pub struct ZeissApotomeTiffReader {
    inner: crate::tiff::TiffReader,
}

impl ZeissApotomeTiffReader {
    pub fn new() -> Self {
        ZeissApotomeTiffReader { inner: crate::tiff::TiffReader::new() }
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

        let mut vendor = std::collections::HashMap::new();

        // Zeiss ApoTome may store XML with <Zeiss> or <ApoTome> elements
        if desc.contains("<Zeiss") || desc.contains("<zeiss") || desc.contains("<ApoTome") || desc.contains("AxioVision") {
            let lower = desc.to_ascii_lowercase();
            // Extract common Zeiss attributes
            for tag_name in &["objectivemagnification", "objectivename", "exposuretime", "numericalaperture", "scalex", "scaley"] {
                if let Some(pos) = lower.find(*tag_name) {
                    let rest = &desc[pos..];
                    // Try attribute form: key="value" or element <key>value</key>
                    if let Some(eq) = rest.find('=') {
                        let val_start = &rest[eq + 1..];
                        let val = val_start.trim_start_matches(|c: char| c == '"' || c == '\'' || c.is_whitespace());
                        let end = val.find(|c: char| c == '"' || c == '\'' || c == '<' || c == '/' || c.is_whitespace())
                            .unwrap_or(val.len());
                        if !val[..end].is_empty() {
                            let key = format!("zeiss.{}", tag_name);
                            if let Ok(f) = val[..end].parse::<f64>() {
                                vendor.insert(key,
                                    crate::common::metadata::MetadataValue::Float(f));
                            } else {
                                vendor.insert(key,
                                    crate::common::metadata::MetadataValue::String(val[..end].to_string()));
                            }
                        }
                    }
                }
            }
        }

        // Also parse key=value lines for non-XML descriptions
        for line in desc.lines() {
            let line = line.trim();
            if let Some((key, val)) = line.split_once('=') {
                let key = key.trim();
                let val = val.trim().trim_matches('"');
                if !key.is_empty() && !val.is_empty() && !key.starts_with('[') && !key.starts_with('<') {
                    let sanitized_key = key.to_ascii_lowercase().replace(' ', "_");
                    if !vendor.contains_key(&format!("zeiss.{}", sanitized_key)) {
                        if let Ok(f) = val.parse::<f64>() {
                            vendor.insert(
                                format!("zeiss.{}", sanitized_key),
                                crate::common::metadata::MetadataValue::Float(f),
                            );
                        } else {
                            vendor.insert(
                                format!("zeiss.{}", sanitized_key),
                                crate::common::metadata::MetadataValue::String(val.to_string()),
                            );
                        }
                    }
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

impl Default for ZeissApotomeTiffReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for ZeissApotomeTiffReader {
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
// 10. Molecular Devices plate TIFF — enriched reader
// ---------------------------------------------------------------------------
/// Molecular Devices MetaXpress plate TIFF (`.tif`).
///
/// Parses ImageDescription for plate/well info and acquisition parameters
/// stored by Molecular Devices MetaXpress software.
pub struct MolecularDevicesTiffReader {
    inner: crate::tiff::TiffReader,
}

impl MolecularDevicesTiffReader {
    pub fn new() -> Self {
        MolecularDevicesTiffReader { inner: crate::tiff::TiffReader::new() }
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

        let mut vendor = std::collections::HashMap::new();

        // Molecular Devices may use XML or key=value pairs
        // Look for plate/well identifiers and acquisition parameters
        if desc.contains("<MetaXpress") || desc.contains("Molecular Devices") || desc.contains("<PlateID") {
            let lower = desc.to_ascii_lowercase();
            for tag_name in &["plateid", "wellid", "siteid", "wavelength", "exposuretime", "objectivemagnification"] {
                if let Some(pos) = lower.find(*tag_name) {
                    let rest = &desc[pos..];
                    if let Some(eq) = rest.find('=') {
                        let val_start = &rest[eq + 1..];
                        let val = val_start.trim_start_matches(|c: char| c == '"' || c == '\'' || c.is_whitespace());
                        let end = val.find(|c: char| c == '"' || c == '\'' || c == '<' || c == '/' || c.is_whitespace())
                            .unwrap_or(val.len());
                        if !val[..end].is_empty() {
                            let key = format!("moldev.{}", tag_name);
                            if let Ok(f) = val[..end].parse::<f64>() {
                                vendor.insert(key,
                                    crate::common::metadata::MetadataValue::Float(f));
                            } else {
                                vendor.insert(key,
                                    crate::common::metadata::MetadataValue::String(val[..end].to_string()));
                            }
                        }
                    }
                }
            }
        }

        // Also parse generic key=value lines
        for line in desc.lines() {
            let line = line.trim();
            if let Some((key, val)) = line.split_once('=') {
                let key = key.trim();
                let val = val.trim().trim_matches('"');
                if !key.is_empty() && !val.is_empty() && !key.starts_with('[') && !key.starts_with('<') {
                    let sanitized_key = key.to_ascii_lowercase().replace(' ', "_");
                    if !vendor.contains_key(&format!("moldev.{}", sanitized_key)) {
                        if let Ok(f) = val.parse::<f64>() {
                            vendor.insert(
                                format!("moldev.{}", sanitized_key),
                                crate::common::metadata::MetadataValue::Float(f),
                            );
                        } else {
                            vendor.insert(
                                format!("moldev.{}", sanitized_key),
                                crate::common::metadata::MetadataValue::String(val.to_string()),
                            );
                        }
                    }
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

impl Default for MolecularDevicesTiffReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for MolecularDevicesTiffReader {
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
