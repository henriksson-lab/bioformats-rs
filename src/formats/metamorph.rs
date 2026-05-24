//! MetaMorph STK format reader (cell biology / live-cell imaging).
//!
//! STK files are TIFF files with Universal Imaging Corporation (UIC) proprietary
//! tags that describe the Z-stack and time-lapse structure:
//!   UIC1Tag = 33628 — per-plane metadata (z-distance, wavelength, etc.)
//!   UIC2Tag = 33629 — z-distances
//!   UIC3Tag = 33630 — wavelengths
//!   UIC4Tag = 33631 — string metadata
//!
//! The number of planes is encoded in UIC1Tag's rational numerator.

use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::reader::FormatReader;
use crate::tiff::ifd::Ifd;
use crate::tiff::ifd::IfdValue;
use crate::tiff::parser::TiffParser;
use crate::tiff::TiffReader;

const UIC1_TAG: u16 = 33628;
#[allow(dead_code)]
const UIC2_TAG: u16 = 33629;
#[allow(dead_code)]
const UIC3_TAG: u16 = 33630;
#[allow(dead_code)]
const UIC4_TAG: u16 = 33631;

/// Read the plane count from UIC1Tag.
/// UIC1Tag is stored as a RATIONAL (numerator/denominator) with:
///   numerator = number of planes
///   denominator = offset into extended UIC data block (we ignore this)
fn read_uic_plane_count(path: &Path) -> Result<Option<u32>> {
    let f = File::open(path).map_err(BioFormatsError::Io)?;
    let buf = BufReader::new(f);
    let mut parser = TiffParser::new(buf)?;
    let (ifd, _) = parser.read_ifd(parser.first_ifd_offset)?;

    // UIC1Tag is stored as a Rational (pair of u32 values)
    let count = match ifd.get(UIC1_TAG) {
        Some(IfdValue::Rational(v)) if !v.is_empty() => Some(v[0].0),
        Some(IfdValue::Long(v)) if !v.is_empty() => Some(v[0]),
        _ => None,
    };
    Ok(count)
}

fn read_metamorph_original_metadata(path: &Path) -> Result<HashMap<String, MetadataValue>> {
    let f = File::open(path).map_err(BioFormatsError::Io)?;
    let buf = BufReader::new(f);
    let mut parser = TiffParser::new(buf)?;
    let (ifd, _) = parser.read_ifd(parser.first_ifd_offset)?;
    Ok(parse_uic4_metadata(&ifd))
}

fn parse_uic4_metadata(ifd: &Ifd) -> HashMap<String, MetadataValue> {
    let mut out = HashMap::new();
    let Some(raw) = ifd.get(UIC4_TAG).and_then(ifd_value_text) else {
        return out;
    };
    let raw = raw.trim_matches(char::from(0)).trim().to_string();
    if raw.is_empty() {
        return out;
    }

    out.insert(
        "metamorph.uic4.raw".into(),
        MetadataValue::String(raw.clone()),
    );
    for entry in raw
        .split(['\0', '\r', '\n', ';'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if let Some((key, value)) = entry.split_once('=') {
            let key = key.trim();
            let value = value.trim();
            if !key.is_empty() {
                out.insert(
                    format!("metamorph.uic4.{key}"),
                    MetadataValue::String(value.to_string()),
                );
            }
        }
    }
    out
}

fn ifd_value_text(value: &IfdValue) -> Option<String> {
    match value {
        IfdValue::Ascii(s) => Some(s.clone()),
        IfdValue::Byte(v) | IfdValue::Undefined(v) => Some(String::from_utf8_lossy(v).into_owned()),
        _ => None,
    }
}

// ── Reader ────────────────────────────────────────────────────────────────────

pub struct MetamorphReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    inner: TiffReader,
}

impl MetamorphReader {
    pub fn new() -> Self {
        MetamorphReader {
            path: None,
            meta: None,
            inner: TiffReader::new(),
        }
    }
}

impl Default for MetamorphReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for MetamorphReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("stk"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        // STK is a TIFF; we rely on extension detection
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        // Try to read plane count from UIC1Tag
        let uic_planes = read_uic_plane_count(path).unwrap_or(None);

        // Open with inner TIFF reader
        self.inner.set_id(path)?;

        // Select the series with the largest image dimensions
        let n_series = self.inner.series_count();
        let mut best_series = 0usize;
        let mut best_pixels = 0u64;
        for s in 0..n_series {
            let _ = self.inner.set_series(s);
            let m = self.inner.metadata();
            let px = m.size_x as u64 * m.size_y as u64;
            if px > best_pixels {
                best_pixels = px;
                best_series = s;
            }
        }
        let _ = self.inner.set_series(best_series);
        let tiff_meta = self.inner.metadata().clone();

        // If UIC1Tag has a plane count, use it; otherwise use TIFF IFD count
        let image_count = uic_planes.unwrap_or(tiff_meta.image_count).max(1);

        let mut meta_map: HashMap<String, MetadataValue> = tiff_meta.series_metadata.clone();
        meta_map.insert(
            "format".into(),
            MetadataValue::String("MetaMorph STK".into()),
        );
        meta_map.extend(read_metamorph_original_metadata(path).unwrap_or_default());
        if let Some(n) = uic_planes {
            meta_map.insert("uic_plane_count".into(), MetadataValue::Int(n as i64));
        }

        let meta = ImageMetadata {
            size_z: image_count,
            size_c: tiff_meta.size_c,
            size_t: 1,
            image_count,
            dimension_order: DimensionOrder::XYZCT,
            series_metadata: meta_map,
            ..tiff_meta
        };

        self.meta = Some(meta);
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        let _ = self.inner.close();
        Ok(())
    }

    fn series_count(&self) -> usize {
        1
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if s != 0 {
            Err(BioFormatsError::SeriesOutOfRange(s))
        } else {
            Ok(())
        }
    }

    fn series(&self) -> usize {
        0
    }

    fn metadata(&self) -> &ImageMetadata {
        self.meta.as_ref().expect("set_id not called")
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let count = self.meta.as_ref().map(|m| m.image_count).unwrap_or(0);
        if plane_index >= count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let inner_count = self.inner.metadata().image_count;
        let inner_idx = if inner_count > 0 {
            plane_index.min(inner_count - 1)
        } else {
            0
        };
        self.inner.open_bytes(inner_idx)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let count = self.meta.as_ref().map(|m| m.image_count).unwrap_or(0);
        if plane_index >= count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let inner_count = self.inner.metadata().image_count;
        let inner_idx = if inner_count > 0 {
            plane_index.min(inner_count - 1)
        } else {
            0
        };
        self.inner.open_bytes_region(inner_idx, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = ((meta.size_x - tw) / 2, (meta.size_y - th) / 2);
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tiff::ifd::{Ifd, IfdValue};

    fn metadata_str<'a>(
        metadata: &'a HashMap<String, MetadataValue>,
        key: &str,
    ) -> Option<&'a str> {
        match metadata.get(key) {
            Some(MetadataValue::String(s)) => Some(s.as_str()),
            _ => None,
        }
    }

    #[test]
    fn metamorph_uic4_metadata_preserves_raw_and_key_values() {
        let mut ifd = Ifd::default();
        ifd.entries.insert(
            UIC4_TAG,
            IfdValue::Ascii("Exposure=12.5\r\nBinning = 2x2\0Comment=live cells".into()),
        );

        let metadata = parse_uic4_metadata(&ifd);

        assert_eq!(
            metadata_str(&metadata, "metamorph.uic4.Exposure"),
            Some("12.5")
        );
        assert_eq!(
            metadata_str(&metadata, "metamorph.uic4.Binning"),
            Some("2x2")
        );
        assert_eq!(
            metadata_str(&metadata, "metamorph.uic4.Comment"),
            Some("live cells")
        );
        assert!(metadata_str(&metadata, "metamorph.uic4.raw")
            .is_some_and(|raw| raw.contains("Exposure=12.5")));
    }

    #[test]
    fn metamorph_uic4_metadata_accepts_undefined_bytes() {
        let mut ifd = Ifd::default();
        ifd.entries.insert(
            UIC4_TAG,
            IfdValue::Undefined(b"Objective=40x;Wavelength=488".to_vec()),
        );

        let metadata = parse_uic4_metadata(&ifd);

        assert_eq!(
            metadata_str(&metadata, "metamorph.uic4.Objective"),
            Some("40x")
        );
        assert_eq!(
            metadata_str(&metadata, "metamorph.uic4.Wavelength"),
            Some("488")
        );
    }
}
