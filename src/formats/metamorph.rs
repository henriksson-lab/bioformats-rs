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

/// Dimension info derived from the UIC tags, mirroring Java MetamorphReader.
struct UicDims {
    /// Total plane count (UIC2 length / mmPlanes).
    image_count: u32,
    size_z: u32,
    size_c: u32,
}

/// Read the raw value/offset field of a given IFD tag by walking the IFD
/// entries directly. Needed for UIC2, whose on-disk layout (6 longs per plane)
/// does not match the declared TIFF count, so the generic IFD parser cannot
/// read it correctly.
fn read_tag_value_offset(data: &[u8], tag: u16) -> Option<(bool, u64, u32)> {
    if data.len() < 8 {
        return None;
    }
    let le = match &data[0..2] {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let rd_u16 = |off: usize| -> u16 {
        if le {
            u16::from_le_bytes([data[off], data[off + 1]])
        } else {
            u16::from_be_bytes([data[off], data[off + 1]])
        }
    };
    let rd_u32 = |off: usize| -> u32 {
        if le {
            u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
        } else {
            u32::from_be_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
        }
    };
    // Only classic TIFF (magic 42) is handled here; STK is always classic.
    if rd_u16(2) != 42 {
        return None;
    }
    let ifd_offset = rd_u32(4) as usize;
    if ifd_offset + 2 > data.len() {
        return None;
    }
    let n_entries = rd_u16(ifd_offset) as usize;
    let mut pos = ifd_offset + 2;
    for _ in 0..n_entries {
        if pos + 12 > data.len() {
            break;
        }
        let entry_tag = rd_u16(pos);
        let count = rd_u32(pos + 4);
        let value_or_offset = rd_u32(pos + 8) as u64;
        if entry_tag == tag {
            return Some((le, value_or_offset, count));
        }
        pos += 12;
    }
    None
}

/// Parse UIC2 (z-distances + timestamps) and UIC3 (wavelengths) to recover the
/// Z/C/T structure of a single-file STK, following Java MetamorphReader logic.
fn read_uic_dims(path: &Path, ifd: &Ifd, mm_planes: u32) -> Option<UicDims> {
    let data = std::fs::read(path).ok()?;

    // UIC2: 24 bytes per plane: z-distance (rational, 8B), date (4B), time (4B),
    // mod-date (4B), mod-time (4B). Count non-zero z-distances -> sizeZ.
    let (le, uic2_offset, _count) = read_tag_value_offset(&data, UIC2_TAG)?;
    let mut size_z = 0u32;
    let mut image_count = mm_planes.max(1);
    {
        let mut z_planes = 0u32;
        let mut off = uic2_offset as usize;
        let rd_u32 = |o: usize| -> u32 {
            if le {
                u32::from_le_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]])
            } else {
                u32::from_be_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]])
            }
        };
        for _ in 0..mm_planes {
            if off + 8 > data.len() {
                break;
            }
            let num = rd_u32(off);
            let den = rd_u32(off + 4);
            let z = if den != 0 { num as f64 / den as f64 } else { 0.0 };
            if z != 0.0 {
                size_z += 1;
            }
            z_planes += 1;
            off += 24;
        }
        if z_planes > 0 {
            image_count = z_planes;
        }
    }
    if size_z == 0 {
        size_z = 1;
    }

    // UIC3: one wavelength rational per plane. sizeC = number of unique values
    // (when the TIFF reports a single channel).
    let mut size_c = 1u32;
    if let Some(IfdValue::Rational(waves)) = ifd.get(UIC3_TAG) {
        let mut unique: Vec<f64> = Vec::new();
        for (n, d) in waves {
            let v = if *d != 0 { *n as f64 / *d as f64 } else { *n as f64 };
            if !unique.iter().any(|u| (*u - v).abs() < f64::EPSILON) {
                unique.push(v);
            }
        }
        if !unique.is_empty() {
            size_c = unique.len() as u32;
            // Java: if sizeC < imageCount && sizeC > (imageCount - sizeC) &&
            //       imageCount % sizeC != 0 -> sizeC = imageCount.
            if size_c < image_count
                && size_c > image_count.saturating_sub(size_c)
                && image_count % size_c != 0
            {
                size_c = image_count;
            }
        }
    }

    Some(UicDims {
        image_count,
        size_z,
        size_c,
    })
}

// ── Per-plane UIC metadata (UIC1/UIC2/UIC3), ported from Java MetamorphReader ──

/// Convert a Julian date int into a `dd/mm/yyyy` string (Java `decodeDate`).
fn decode_date(julian: i32) -> String {
    let z = julian as i64 + 1;
    let a = if z < 2_299_161 {
        z
    } else {
        let alpha = ((z as f64 - 1_867_216.25) / 36_524.25) as i64;
        z + 1 + alpha - alpha / 4
    };
    let b = if a > 1_721_423 { a + 1524 } else { a + 1158 };
    let c = ((b as f64 - 122.1) / 365.25) as i64;
    let d = (365.25 * c as f64) as i64;
    let e = ((b - d) as f64 / 30.6001) as i64;
    let day = b - d - (30.6001 * e as f64) as i64;
    let month = if (e as f64) < 13.5 { e - 1 } else { e - 13 };
    let year = if (month as f64) > 2.5 { c - 4716 } else { c - 4715 };
    format!("{:02}/{:02}/{}", day, month, year)
}

/// Convert a milliseconds-of-day int into `hh:mm:ss:SSS` (Java `decodeTime`).
fn decode_time(millis: i32) -> String {
    let millis = millis.max(0);
    let total_secs = millis / 1000;
    let ms = millis % 1000;
    let h = (total_secs / 3600) % 24;
    let m = (total_secs / 60) % 60;
    let s = total_secs % 60;
    format!("{:02}:{:02}:{:02}:{:03}", h, m, s, ms)
}

/// Format `i` with leading zeros to the width of `max`'s digit count.
fn int_format_max(i: u32, max: u32) -> String {
    let width = max.to_string().len();
    format!("{:0width$}", i, width = width)
}

/// Parse per-plane UIC2 (z-distance, creation date/time) and UIC3 (wavelength)
/// tables into a metadata map, mirroring Java `parseUIC2Tags` / UIC3 handling.
fn parse_uic_per_plane_metadata(
    data: &[u8],
    ifd: &Ifd,
    mm_planes: u32,
) -> HashMap<String, MetadataValue> {
    let mut out = HashMap::new();
    if mm_planes == 0 {
        return out;
    }

    if let Some((le, uic2_offset, _count)) = read_tag_value_offset(data, UIC2_TAG) {
        let rd_i32 = |o: usize| -> Option<i32> {
            if o + 4 > data.len() {
                return None;
            }
            Some(if le {
                i32::from_le_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]])
            } else {
                i32::from_be_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]])
            })
        };
        let mut off = uic2_offset as usize;
        for i in 0..mm_planes {
            // z-distance rational (8 bytes)
            let (Some(num), Some(den)) = (rd_i32(off), rd_i32(off + 4)) else {
                break;
            };
            let label = int_format_max(i, mm_planes);
            let z = if den != 0 { num as f64 / den as f64 } else { 0.0 };
            out.insert(
                format!("zDistance[{label}]"),
                MetadataValue::Float(z),
            );
            // creation date (4B) and time (4B)
            if let (Some(date_raw), Some(time_raw)) = (rd_i32(off + 8), rd_i32(off + 12)) {
                out.insert(
                    format!("creationDate[{label}]"),
                    MetadataValue::String(decode_date(date_raw)),
                );
                out.insert(
                    format!("creationTime[{label}]"),
                    MetadataValue::String(decode_time(time_raw)),
                );
            }
            // modification date/time (8B) skipped, as in Java.
            off += 24;
        }
    }

    // UIC3: one wavelength rational per plane.
    if let Some(IfdValue::Rational(waves)) = ifd.get(UIC3_TAG) {
        for (i, (n, d)) in waves.iter().enumerate() {
            let v = if *d != 0 { *n as f64 / *d as f64 } else { *n as f64 };
            let label = int_format_max(i as u32, mm_planes);
            out.insert(format!("Wavelength [{label}]"), MetadataValue::Float(v));
        }
    }

    out
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

impl MetamorphReader {
    /// Read a plane directly from the concatenated STK strip data. Used when the
    /// inner TIFF reader exposes a single IFD that actually contains all planes
    /// (Java rebuilds per-plane strip offsets; here we assume contiguous,
    /// uncompressed planes after the first strip offset).
    fn read_concatenated_plane(&self, plane_index: u32) -> Result<Vec<u8>> {
        use std::io::{Read, Seek, SeekFrom};
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let bps = meta.pixel_type.bytes_per_sample();
        let plane_bytes = meta.size_x as usize * meta.size_y as usize * bps;

        // Find the first strip offset of the first IFD.
        let f = File::open(path).map_err(BioFormatsError::Io)?;
        let buf = BufReader::new(f);
        let mut parser = TiffParser::new(buf)?;
        let (ifd, _) = parser.read_ifd(parser.first_ifd_offset)?;
        let base_offset = match ifd.get(crate::tiff::ifd::tag::STRIP_OFFSETS) {
            Some(IfdValue::Long(v)) if !v.is_empty() => v[0] as u64,
            Some(IfdValue::Short(v)) if !v.is_empty() => v[0] as u64,
            _ => {
                return Err(BioFormatsError::Format(
                    "MetaMorph STK: missing strip offsets for concatenated plane".into(),
                ))
            }
        };
        let offset = base_offset + plane_index as u64 * plane_bytes as u64;

        let mut file = File::open(path).map_err(BioFormatsError::Io)?;
        let len = file.metadata().map_err(BioFormatsError::Io)?.len();
        let mut out = vec![0u8; plane_bytes];
        if offset < len {
            file.seek(SeekFrom::Start(offset)).map_err(BioFormatsError::Io)?;
            let available = (len - offset).min(plane_bytes as u64) as usize;
            file.read_exact(&mut out[..available])
                .map_err(BioFormatsError::Io)?;
        }
        Ok(out)
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

        // mmPlanes: UIC1 plane count if present, else the TIFF IFD count.
        let mm_planes = uic_planes.unwrap_or(tiff_meta.image_count).max(1);

        // Parse UIC2/UIC3 for the Z/C/T structure (Java MetamorphReader).
        let uic_dims = {
            let f = File::open(path).ok();
            let parsed = f.and_then(|file| {
                let buf = BufReader::new(file);
                TiffParser::new(buf).ok().and_then(|mut parser| {
                    parser
                        .read_ifd(parser.first_ifd_offset)
                        .ok()
                        .and_then(|(ifd, _)| read_uic_dims(path, &ifd, mm_planes))
                })
            });
            parsed
        };

        let rgb_channels = if tiff_meta.is_rgb { 3 } else { 1 };
        let tiff_c = tiff_meta.size_c.max(1);

        let (image_count, mut size_z, uic_size_c) = match &uic_dims {
            Some(d) => (d.image_count.max(1), d.size_z.max(1), d.size_c.max(1)),
            None => (mm_planes, mm_planes, tiff_c),
        };
        // If the TIFF already reports more than one channel, respect it.
        let mut size_c = if tiff_c > 1 { tiff_c } else { uic_size_c };

        // sizeT = imageCount / (sizeZ * (sizeC / rgbChannels)), with Java's
        // reconciliation fallbacks.
        let effective_c = (size_c / rgb_channels).max(1);
        let mut size_t = (image_count / (size_z * effective_c).max(1)).max(1);
        if size_t * size_z * effective_c != image_count {
            size_t = 1;
            size_z = (image_count / effective_c).max(1);
        }

        // If '_t' is present in the file name and sizeT > 1, swap Z and T.
        let fname = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        if fname.contains("_t") && size_t > 1 {
            std::mem::swap(&mut size_z, &mut size_t);
        }
        if size_z == 0 {
            size_z = 1;
        }
        if size_t == 0 {
            size_t = 1;
        }
        // Final consistency check.
        let check_c = if tiff_meta.is_rgb { 1 } else { size_c };
        if size_z * size_t * check_c != image_count {
            size_z = image_count;
            size_t = 1;
            if !tiff_meta.is_rgb {
                size_c = 1;
            }
        }

        let mut meta_map: HashMap<String, MetadataValue> = tiff_meta.series_metadata.clone();
        meta_map.insert(
            "format".into(),
            MetadataValue::String("MetaMorph STK".into()),
        );
        meta_map.extend(read_metamorph_original_metadata(path).unwrap_or_default());
        // Per-plane UIC2/UIC3 metadata (z-distances, creation timestamps,
        // wavelengths), mirroring Java parseUIC2Tags / UIC3 handling.
        if let (Ok(data), Some(file)) = (
            std::fs::read(path),
            File::open(path).ok().and_then(|f| {
                let buf = BufReader::new(f);
                TiffParser::new(buf)
                    .ok()
                    .and_then(|mut p| p.read_ifd(p.first_ifd_offset).ok())
            }),
        ) {
            let (ifd, _) = file;
            meta_map.extend(parse_uic_per_plane_metadata(&data, &ifd, mm_planes));
        }
        if let Some(n) = uic_planes {
            meta_map.insert("uic_plane_count".into(), MetadataValue::Int(n as i64));
        }

        let meta = ImageMetadata {
            size_z,
            size_c,
            size_t,
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
        // Planes map 1:1 to the inner TIFF reader when it exposes enough planes
        // (Java rebuilds one IFD per plane). When the STK stores all planes as
        // strips in a single IFD, fall back to reading the plane directly from
        // the concatenated strip data.
        if plane_index < inner_count {
            return self.inner.open_bytes(plane_index);
        }
        self.read_concatenated_plane(plane_index)
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
        if plane_index < inner_count {
            return self.inner.open_bytes_region(plane_index, x, y, w, h);
        }
        // Crop from the concatenated-strip plane.
        let full = self.read_concatenated_plane(plane_index)?;
        let meta = self.meta.as_ref().unwrap();
        let bps = meta.pixel_type.bytes_per_sample();
        let row = meta.size_x as usize * bps;
        let out_row = w as usize * bps;
        let mut out = Vec::with_capacity(h as usize * out_row);
        for r in 0..h as usize {
            let src = &full[(y as usize + r) * row..];
            out.extend_from_slice(&src[x as usize * bps..x as usize * bps + out_row]);
        }
        Ok(out)
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

    #[test]
    fn metamorph_decode_time_formats_hms_ms() {
        // 1 hour + 2 min + 3 sec + 4 ms = 3_723_004 ms.
        let millis = (3600 + 120 + 3) * 1000 + 4;
        assert_eq!(decode_time(millis), "01:02:03:004");
        assert_eq!(decode_time(0), "00:00:00:000");
    }

    #[test]
    fn metamorph_decode_date_is_dd_mm_yyyy() {
        // Julian day number 2451545 corresponds to 2000-01-01 (noon).
        // decodeDate uses the Metamorph spec's algorithm; verify the shape and
        // a known value (01/01/2000).
        let s = decode_date(2451544);
        assert_eq!(s, "01/01/2000");
    }

    #[test]
    fn metamorph_int_format_max_pads_to_width() {
        assert_eq!(int_format_max(3, 100), "003");
        assert_eq!(int_format_max(42, 9), "42");
        assert_eq!(int_format_max(7, 10), "07");
    }
}
