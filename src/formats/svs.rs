//! Whole-slide TIFF-based format reader.
//!
//! Wraps TiffReader and enriches metadata with vendor-specific information:
//! - **Aperio SVS** (.svs) — parses `|key=value` pairs from ImageDescription
//!   for magnification, microns-per-pixel, date, etc.
//! - Also supports: Ventana BIF, Hamamatsu NDPI, Leica SCN, Olympus VSI, AFI.

use std::path::Path;

use crate::common::error::Result;
use crate::common::metadata::{ImageMetadata, MetadataValue};
use crate::common::reader::FormatReader;

pub struct WholeSlideTiffReader {
    inner: crate::tiff::TiffReader,
}

impl WholeSlideTiffReader {
    pub fn new() -> Self {
        WholeSlideTiffReader { inner: crate::tiff::TiffReader::new() }
    }

    /// Parse Aperio SVS ImageDescription metadata.
    /// Format: "Aperio ...|key=value|key=value|..."
    fn parse_aperio_metadata(&mut self) {
        let desc = {
            let series = self.inner.series_list();
            if series.is_empty() { return; }
            series[0].metadata.series_metadata.get("ImageDescription")
                .and_then(|v| if let MetadataValue::String(s) = v { Some(s.clone()) } else { None })
        };
        let Some(desc) = desc else { return };
        if !desc.starts_with("Aperio") { return; }

        // Parse |key=value pairs
        let mut vendor_meta = std::collections::HashMap::new();
        for part in desc.split('|').skip(1) {
            if let Some((key, val)) = part.split_once('=') {
                let key = key.trim().to_string();
                let val = val.trim().to_string();
                vendor_meta.insert(key, MetadataValue::String(val));
            }
        }

        // Also try to extract microns-per-pixel and magnification as OME-like metadata
        let mpp = vendor_meta.get("MPP")
            .and_then(|v| if let MetadataValue::String(s) = v { s.parse::<f64>().ok() } else { None });
        let mag = vendor_meta.get("AppMag")
            .and_then(|v| if let MetadataValue::String(s) = v { s.parse::<f64>().ok() } else { None });

        let series = self.inner.series_list_mut();
        if let Some(s) = series.first_mut() {
            // Store vendor metadata
            for (k, v) in vendor_meta {
                s.metadata.series_metadata.insert(format!("aperio.{}", k), v);
            }
            // Store magnification
            if let Some(m) = mag {
                s.metadata.series_metadata.insert(
                    "objective.magnification".into(),
                    MetadataValue::Float(m),
                );
            }
            if let Some(m) = mpp {
                s.metadata.series_metadata.insert(
                    "pixel.size.um".into(),
                    MetadataValue::Float(m),
                );
            }
        }
    }
}

impl Default for WholeSlideTiffReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for WholeSlideTiffReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension().and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("svs") | Some("bif") | Some("ndpi") | Some("scn") | Some("vsi") | Some("afi"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.inner.set_id(path)?;
        self.parse_aperio_metadata();
        Ok(())
    }

    fn close(&mut self) -> Result<()> { self.inner.close() }
    fn series_count(&self) -> usize { self.inner.series_count() }
    fn set_series(&mut self, series: usize) -> Result<()> { self.inner.set_series(series) }
    fn series(&self) -> usize { self.inner.series() }
    fn metadata(&self) -> &ImageMetadata { self.inner.metadata() }
    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> { self.inner.open_bytes(plane_index) }
    fn open_bytes_region(&mut self, plane_index: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        self.inner.open_bytes_region(plane_index, x, y, w, h)
    }
    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> { self.inner.open_thumb_bytes(plane_index) }
    fn resolution_count(&self) -> usize { self.inner.resolution_count() }
    fn set_resolution(&mut self, level: usize) -> Result<()> { self.inner.set_resolution(level) }
    fn resolution(&self) -> usize { self.inner.resolution() }
}
