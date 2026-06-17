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

// ---------------------------------------------------------------------------
// Pyramid TIFF (Faas-format) — faithful port of Java
// loci.formats.in.PyramidTiffReader (extends BaseTiffReader).
// ---------------------------------------------------------------------------
/// Pyramid TIFF (`.tif` / `.tiff`).
///
/// Faithful port of Java `loci.formats.in.PyramidTiffReader`. The pyramid is
/// stored as the main IFD chain (one top-level IFD per resolution level, each
/// successively smaller). Java's `initStandardMetadata()` collapses those IFDs
/// into a single series whose `resolutionCount` equals the number of IFDs; we
/// mirror that by regrouping the inner `TiffReader`'s per-IFD series into one
/// multi-resolution series.
///
/// Detection mirrors Java `isThisType(RandomAccessInputStream)`: parse the
/// first IFD's SOFTWARE tag and require it contains the substring `Faas`, so
/// this reader only claims genuine Faas pyramid TIFFs (distinct from Aperio
/// `SvsReader` above and the crate's generic SubIFD-pyramid `TiffReader`).
pub struct PyramidTiffReader {
    inner: crate::tiff::TiffReader,
}

impl PyramidTiffReader {
    pub fn new() -> Self {
        PyramidTiffReader {
            inner: crate::tiff::TiffReader::new(),
        }
    }

    /// Mirror Java `PyramidTiffReader.isThisType(RandomAccessInputStream)`:
    /// parse the first IFD, read its SOFTWARE tag, and require it contains the
    /// substring `Faas`. Operates on whatever header bytes are available; if the
    /// SOFTWARE value lies beyond the supplied window the parse fails gracefully
    /// and detection returns `false`.
    fn is_this_type_from_bytes(header: &[u8]) -> bool {
        let cursor = std::io::Cursor::new(header);
        let mut parser = match crate::tiff::parser::TiffParser::new(cursor) {
            Ok(p) => p,
            Err(_) => return false,
        };
        let offset = parser.first_ifd_offset;
        let ifd = match parser.read_ifd(offset) {
            Ok((ifd, _)) => ifd,
            Err(_) => return false,
        };
        match ifd
            .get(crate::tiff::ifd::tag::SOFTWARE)
            .and_then(|v| v.as_str())
        {
            Some(software) => software.contains("Faas"),
            None => false,
        }
    }

    /// Mirror Java `PyramidTiffReader.initStandardMetadata()`: repopulate the
    /// core metadata so the `ifds.size()` top-level IFDs become a single series
    /// with `resolutionCount == ifds.size()` (series 0, level 0 = full
    /// resolution; the remaining IFDs are successively smaller sub-resolutions).
    ///
    /// The inner `TiffReader` already parses each differently-sized top-level
    /// IFD into its own series (`build_series` groups by dimensions). We collapse
    /// those into one multi-resolution series by moving the trailing series'
    /// IFD indices into `sub_resolutions` of the first.
    fn init_standard_metadata(&mut self) {
        let series = self.inner.series_list();
        if series.len() <= 1 {
            // Single IFD (or empty): nothing to regroup; resolutionCount stays 1.
            return;
        }

        // Each pre-existing series maps to one pyramid resolution level, in
        // order (Java reads ifds.get(s) for s in 0..seriesCount).
        let mut base = series[0].clone();
        let sub_resolutions: Vec<Vec<usize>> =
            series[1..].iter().map(|s| s.ifd_indices.clone()).collect();
        base.metadata.resolution_count = 1 + sub_resolutions.len() as u32;
        base.sub_resolutions = sub_resolutions;
        // Java sets ms.thumbnail = (s > 0); the full-resolution series 0 is not a
        // thumbnail, matching the inner reader's default for series 0.
        base.metadata.thumbnail = false;

        self.inner.replace_series(vec![base]);
    }
}

impl Default for PyramidTiffReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for PyramidTiffReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        // Java: suffixSufficient = false, suffixNecessary = false; the .tif/.tiff
        // suffixes are advisory. Real detection is the SOFTWARE-tag byte check.
        matches!(ext.as_deref(), Some("tif") | Some("tiff"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        Self::is_this_type_from_bytes(header)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.inner.close()?;
        self.inner.set_id(path)?;
        self.init_standard_metadata();
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
        self.inner.open_bytes(plane_index)
    }
    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        self.inner.open_bytes_region(plane_index, x, y, w, h)
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

    /// Mirror Java `PyramidTiffReader.initMetadataStore()`: name each OME image
    /// "Series 1", "Series 2", …
    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        use crate::common::ome_metadata::{OmeChannel, OmeImage, OmeMetadata};
        let series = self.inner.series_list();
        if series.is_empty() {
            return None;
        }
        let images = series
            .iter()
            .enumerate()
            .map(|(i, s)| OmeImage {
                name: Some(format!("Series {}", i + 1)),
                channels: vec![OmeChannel {
                    samples_per_pixel: s.metadata.size_c.max(1),
                    ..Default::default()
                }],
                ..Default::default()
            })
            .collect();
        Some(OmeMetadata {
            images,
            ..Default::default()
        })
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

#[cfg(test)]
mod pyramid_tiff_tests {
    use super::PyramidTiffReader;
    use crate::common::reader::FormatReader;
    use crate::tiff::ifd::tag;

    fn push_u16(d: &mut Vec<u8>, v: u16) {
        d.extend_from_slice(&v.to_le_bytes());
    }
    fn push_u32(d: &mut Vec<u8>, v: u32) {
        d.extend_from_slice(&v.to_le_bytes());
    }
    fn push_short(d: &mut Vec<u8>, tag: u16, value: u16) {
        push_u16(d, tag);
        push_u16(d, 3); // SHORT
        push_u32(d, 1);
        push_u16(d, value);
        push_u16(d, 0);
    }
    fn push_long(d: &mut Vec<u8>, tag: u16, value: u32) {
        push_u16(d, tag);
        push_u16(d, 4); // LONG
        push_u32(d, 1);
        push_u32(d, value);
    }
    fn push_ascii_at_offset(d: &mut Vec<u8>, tag: u16, s: &str, value_offset: u32) {
        // ASCII value (count includes the NUL terminator) stored out-of-line at
        // `value_offset`. The caller is responsible for writing the bytes there.
        push_u16(d, tag);
        push_u16(d, 2); // ASCII
        push_u32(d, (s.len() + 1) as u32);
        push_u32(d, value_offset);
    }

    /// Build a classic little-endian TIFF whose main IFD chain encodes a pyramid:
    /// one top-level IFD per resolution level (4x4 full res, then 2x2). The first
    /// IFD carries SOFTWARE = `software`. Single 8-bit grayscale strip per level.
    fn build_pyramid_tiff(software: &str) -> Vec<u8> {
        // Layout: [header 8][IFD0][IFD1][SOFTWARE str][pixels0][pixels1]
        // Each IFD has 9 entries -> 2 + 9*12 + 4 = 114 bytes.
        let dims = [4u32, 2u32];
        let ifd_size = 2 + 9 * 12 + 4;
        let ifd0_off: u32 = 8;
        let ifd1_off: u32 = ifd0_off + ifd_size as u32;
        let sw_off: u32 = ifd1_off + ifd_size as u32;
        let sw_len = (software.len() + 1) as u32; // includes NUL
        let px0_off: u32 = sw_off + sw_len;
        let px0_len = dims[0] * dims[0];
        let px1_off: u32 = px0_off + px0_len;

        let mut d: Vec<u8> = Vec::new();
        d.extend_from_slice(b"II");
        push_u16(&mut d, 42);
        push_u32(&mut d, ifd0_off);

        // -- IFD0 (full resolution, carries SOFTWARE) --
        push_u16(&mut d, 9); // entry count (tags must be ascending)
        push_long(&mut d, tag::IMAGE_WIDTH, dims[0]);
        push_long(&mut d, tag::IMAGE_LENGTH, dims[0]);
        push_short(&mut d, tag::BITS_PER_SAMPLE, 8);
        push_short(&mut d, tag::COMPRESSION, 1); // none
        push_short(&mut d, tag::PHOTOMETRIC_INTERPRETATION, 1); // black is zero
        push_long(&mut d, tag::STRIP_OFFSETS, px0_off);
        push_long(&mut d, tag::ROWS_PER_STRIP, dims[0]);
        push_long(&mut d, tag::STRIP_BYTE_COUNTS, px0_len);
        push_ascii_at_offset(&mut d, tag::SOFTWARE, software, sw_off); // tag 305
        push_u32(&mut d, ifd1_off); // next IFD

        // -- IFD1 (half resolution) --
        push_u16(&mut d, 9);
        push_long(&mut d, tag::IMAGE_WIDTH, dims[1]);
        push_long(&mut d, tag::IMAGE_LENGTH, dims[1]);
        push_short(&mut d, tag::BITS_PER_SAMPLE, 8);
        push_short(&mut d, tag::COMPRESSION, 1);
        push_short(&mut d, tag::PHOTOMETRIC_INTERPRETATION, 1);
        push_long(&mut d, tag::STRIP_OFFSETS, px1_off);
        push_long(&mut d, tag::ROWS_PER_STRIP, dims[1]);
        push_long(&mut d, tag::STRIP_BYTE_COUNTS, dims[1] * dims[1]);
        push_ascii_at_offset(&mut d, tag::SOFTWARE, software, sw_off);
        push_u32(&mut d, 0); // end of chain

        // -- out-of-line SOFTWARE value (NUL-terminated) --
        d.extend_from_slice(software.as_bytes());
        d.push(0);

        // -- pixel data --
        d.extend(std::iter::repeat(0xABu8).take(px0_len as usize));
        d.extend(std::iter::repeat(0xCDu8).take((dims[1] * dims[1]) as usize));
        d
    }

    #[test]
    fn detects_faas_software_tag() {
        let faas = build_pyramid_tiff("Faas");
        let reader = PyramidTiffReader::new();
        assert!(
            reader.is_this_type_by_bytes(&faas),
            "Faas SOFTWARE tag must be claimed"
        );
    }

    #[test]
    fn rejects_non_faas_tiff() {
        let other = build_pyramid_tiff("ACME");
        let reader = PyramidTiffReader::new();
        assert!(
            !reader.is_this_type_by_bytes(&other),
            "non-Faas SOFTWARE tag must NOT be claimed"
        );
    }

    #[test]
    fn maps_ifd_chain_to_resolution_levels() {
        let faas = build_pyramid_tiff("Faas");
        let path = std::env::temp_dir().join(format!("pyramid_faas_{}.tif", std::process::id()));
        std::fs::write(&path, &faas).unwrap();

        let mut reader = PyramidTiffReader::new();
        reader.set_id(&path).unwrap();

        // The two top-level IFDs collapse into a single multi-resolution series.
        assert_eq!(reader.series_count(), 1, "expected one pyramid series");
        assert_eq!(
            reader.resolution_count(),
            2,
            "expected two resolution levels (full + half)"
        );

        // Level 0 = full resolution (4x4).
        reader.set_resolution(0).unwrap();
        assert_eq!(reader.metadata().size_x, 4);
        assert_eq!(reader.metadata().size_y, 4);

        // Level 1 = half resolution (2x2).
        reader.set_resolution(1).unwrap();
        assert_eq!(reader.metadata().size_x, 2);
        assert_eq!(reader.metadata().size_y, 2);

        reader.close().unwrap();
        let _ = std::fs::remove_file(&path);
    }
}
