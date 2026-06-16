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

pub struct SvsReader {
    inner: crate::tiff::TiffReader,
}

impl SvsReader {
    pub fn new() -> Self {
        SvsReader {
            inner: crate::tiff::TiffReader::new(),
        }
    }

    /// Parse Aperio SVS ImageDescription metadata.
    /// Format: "Aperio ...|key=value|key=value|..."
    fn parse_aperio_metadata(&mut self) {
        let desc = {
            let series = self.inner.series_list();
            if series.is_empty() {
                return;
            }
            series[0]
                .metadata
                .series_metadata
                .get("ImageDescription")
                .and_then(|v| {
                    if let MetadataValue::String(s) = v {
                        Some(s.clone())
                    } else {
                        None
                    }
                })
        };
        let Some(desc) = desc else { return };
        if !desc.starts_with("Aperio") {
            return;
        }

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
        let mpp = vendor_meta.get("MPP").and_then(|v| {
            if let MetadataValue::String(s) = v {
                s.parse::<f64>().ok()
            } else {
                None
            }
        });
        let mag = vendor_meta.get("AppMag").and_then(|v| {
            if let MetadataValue::String(s) = v {
                s.parse::<f64>().ok()
            } else {
                None
            }
        });

        let series = self.inner.series_list_mut();
        if let Some(s) = series.first_mut() {
            // Store vendor metadata
            for (k, v) in vendor_meta {
                s.metadata
                    .series_metadata
                    .insert(format!("aperio.{}", k), v);
            }
            // Store magnification
            if let Some(m) = mag {
                s.metadata
                    .series_metadata
                    .insert("objective.magnification".into(), MetadataValue::Float(m));
            }
            if let Some(m) = mpp {
                s.metadata
                    .series_metadata
                    .insert("pixel.size.um".into(), MetadataValue::Float(m));
            }
        }
    }
}

impl Default for SvsReader {
    fn default() -> Self {
        Self::new()
    }
}

impl SvsReader {
    /// Convert a chunky/interleaved RGB(A) buffer (`c0c1c2 c0c1c2 …`) into the
    /// channel-separated layout (`c0c0… c1c1… c2c2…`) that Java's SVSReader
    /// returns (`isInterleaved() == false`). No-op unless the current series is
    /// RGB and flagged non-interleaved, and the buffer length matches
    /// `w * h * channels * bytesPerSample`.
    fn separate_channels(&self, buf: Vec<u8>, w: u32, h: u32) -> Vec<u8> {
        let m = self.inner.metadata();
        if !m.is_rgb || m.is_interleaved {
            return buf;
        }
        let channels = m.size_c as usize;
        if channels < 2 {
            return buf;
        }
        let bps = (m.bits_per_pixel as usize + 7) / 8;
        let bps = bps.max(1);
        let pixels = w as usize * h as usize;
        let expected = pixels * channels * bps;
        if pixels == 0 || buf.len() != expected {
            return buf;
        }
        let mut out = vec![0u8; expected];
        let plane = pixels * bps;
        for i in 0..pixels {
            for c in 0..channels {
                let src = (i * channels + c) * bps;
                let dst = c * plane + i * bps;
                out[dst..dst + bps].copy_from_slice(&buf[src..src + bps]);
            }
        }
        out
    }
}

impl FormatReader for SvsReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(
            ext.as_deref(),
            Some("svs") | Some("bif") | Some("ndpi") | Some("scn") | Some("vsi") | Some("afi")
        )
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.inner.close()?;
        self.inner.set_id(path)?;
        // Aperio SVS stores its pyramid as the main IFD chain (differently
        // sized IFDs). TiffReader splits these into separate series by default;
        // regroup them into one multi-resolution series + label/macro series,
        // mirroring SVSReader.java.
        let is_svs = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("svs"))
            .unwrap_or(false);
        if is_svs {
            self.inner.regroup_as_svs_pyramid()?;
        }
        self.parse_aperio_metadata();
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.inner.close()
    }
    fn series_count(&self) -> usize {
        self.inner.series_count()
    }
    fn set_series(&mut self, series: usize) -> Result<()> {
        self.inner.set_series(series)
    }
    fn series(&self) -> usize {
        self.inner.series()
    }
    fn metadata(&self) -> &ImageMetadata {
        self.inner.metadata()
    }
    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let (w, h) = {
            let m = self.inner.metadata();
            (m.size_x, m.size_y)
        };
        let buf = self.inner.open_bytes(plane_index)?;
        Ok(self.separate_channels(buf, w, h))
    }
    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let buf = self.inner.open_bytes_region(plane_index, x, y, w, h)?;
        Ok(self.separate_channels(buf, w, h))
    }
    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        self.inner.open_thumb_bytes(plane_index)
    }
    fn resolution_count(&self) -> usize {
        self.inner.resolution_count()
    }
    fn set_resolution(&mut self, level: usize) -> Result<()> {
        self.inner.set_resolution(level)
    }
    fn resolution(&self) -> usize {
        self.inner.resolution()
    }

    /// One OME image per series, named "Series 1", "Series 2", … (Java
    /// SVSReader.initMetadataStore). The full-resolution image (series 0) carries
    /// the Aperio `MPP` micrometre/pixel as PhysicalSizeX/Y; label/macro images
    /// have no calibration. Each image has a single RGB channel
    /// (SamplesPerPixel = channel count).
    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        use crate::common::ome_metadata::{OmeChannel, OmeImage, OmeMetadata};
        let series = self.inner.series_list();
        if series.is_empty() {
            return None;
        }
        // Aperio MPP (micrometres per pixel) lives on the first series.
        let mpp = series.first().and_then(|s| {
            s.metadata
                .series_metadata
                .get("pixel.size.um")
                .and_then(|v| match v {
                    MetadataValue::Float(f) => Some(*f),
                    MetadataValue::String(s) => s.parse::<f64>().ok(),
                    _ => None,
                })
        });
        let images = series
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let m = &s.metadata;
                let (px, py) = if i == 0 { (mpp, mpp) } else { (None, None) };
                OmeImage {
                    name: Some(format!("Series {}", i + 1)),
                    physical_size_x: px,
                    physical_size_y: py,
                    channels: vec![OmeChannel {
                        samples_per_pixel: m.size_c.max(1),
                        ..Default::default()
                    }],
                    ..Default::default()
                }
            })
            .collect();
        Some(OmeMetadata {
            images,
            ..Default::default()
        })
    }
}
