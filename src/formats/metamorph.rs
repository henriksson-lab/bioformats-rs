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
use crate::common::region::crop_full_plane;
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
            let z = if den != 0 {
                num as f64 / den as f64
            } else {
                0.0
            };
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
            let v = if *d != 0 {
                *n as f64 / *d as f64
            } else {
                *n as f64
            };
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
    let year = if (month as f64) > 2.5 {
        c - 4716
    } else {
        c - 4715
    };
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
            let z = if den != 0 {
                num as f64 / den as f64
            } else {
                0.0
            };
            out.insert(format!("zDistance[{label}]"), MetadataValue::Float(z));
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
            let v = if *d != 0 {
                *n as f64 / *d as f64
            } else {
                *n as f64
            };
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
    /// `.nd`-driven multi-file series grid: `stks[series][file]` holds the
    /// resolved STK/TIFF path for each constituent file, mirroring Java
    /// `MetamorphReader.stks`. `None` when reading a standalone STK.
    stks: Option<Vec<Vec<Option<PathBuf>>>>,
    /// Per-series core metadata (parallel to `stks`).
    metas: Vec<ImageMetadata>,
    /// Currently selected series.
    series: usize,
    /// The companion `.nd` file path, if any (Java `ndFilename`).
    nd_filename: Option<PathBuf>,
}

impl MetamorphReader {
    pub fn new() -> Self {
        MetamorphReader {
            path: None,
            meta: None,
            inner: TiffReader::new(),
            stks: None,
            metas: Vec::new(),
            series: 0,
            nd_filename: None,
        }
    }
}

// ── .nd companion file parsing (multi-STK series) ──────────────────────────────

const NDINFOFILE_VER1: &str = "Version 1.0";
const NDINFOFILE_VER2: &str = "Version 2.0";

/// Parsed contents of a MetaMorph `.nd` info file, mirroring the key/value
/// pairs Java's `initFile` extracts.
#[derive(Default)]
struct NdInfo {
    version: String,
    z_steps: Option<i32>,       // NZSteps
    n_wavelengths: Option<i32>, // NWavelengths
    n_time_points: Option<i32>, // NTimePoints
    do_timelapse: bool,         // DoTimelapse
    do_z_series: bool,          // DoZSeries (globalDoZ)
    do_wave: bool,              // DoWave
    n_stage_positions: i32,     // NStagePositions
    use_wave_names: bool,       // WaveInFileName
    wave_names: Vec<String>,    // WaveName<n>
    wave_do_z: Vec<bool>,       // WaveDoZ<n>
    bizarre_multichannel: bool, // "Both lasers" / "DUAL" wave name
}

/// Read a `.nd` file as windows-1252 (each byte maps directly to a char),
/// matching `DataTools.readFile(.., "windows-1252")`.
fn read_nd_text(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).map_err(BioFormatsError::Io)?;
    Ok(bytes.iter().map(|&b| b as char).collect())
}

/// Parse the key/value pairs of an `.nd` file. The `.nd` format quotes keys as
/// `"Key", value`; values may span multiple lines until the next comma.
fn parse_nd(text: &str) -> NdInfo {
    let mut info = NdInfo {
        version: NDINFOFILE_VER1.to_string(),
        do_z_series: true,
        use_wave_names: true,
        ..Default::default()
    };

    // Java accumulates a multi-line value until the next line containing a comma
    // (or "EndFile"), then dispatches on the *previous* key.
    let mut current_value = String::new();
    let mut key = String::new();
    let mut global_do_z = true;

    for line in text.split('\n') {
        let comma = line.find(',').map(|i| i as i64).unwrap_or(-1);
        if comma <= 0 && !line.contains("EndFile") {
            current_value.push('\n');
            current_value.push_str(line);
            continue;
        }

        let value = current_value.clone();
        match key.as_str() {
            "NDInfoFile" => info.version = value.clone(),
            "NZSteps" => info.z_steps = value.trim().parse().ok(),
            "DoTimelapse" => info.do_timelapse = parse_bool(&value),
            "NWavelengths" => info.n_wavelengths = value.trim().parse().ok(),
            "NTimePoints" => info.n_time_points = value.trim().parse().ok(),
            k if k.starts_with("WaveDoZ") => info.wave_do_z.push(parse_bool(&value)),
            k if k.starts_with("WaveName") => {
                // Strip the surrounding quotes (value is like `"FITC"`).
                let trimmed = value.trim();
                let wave_name = if trimmed.len() >= 2 {
                    trimmed[1..trimmed.len() - 1].to_string()
                } else {
                    trimmed.to_string()
                };
                if wave_name == "Both lasers" || wave_name.starts_with("DUAL") {
                    info.bizarre_multichannel = true;
                }
                info.wave_names.push(wave_name);
            }
            "NStagePositions" => info.n_stage_positions = value.trim().parse().unwrap_or(0),
            "WaveInFileName" => info.use_wave_names = parse_bool(&value),
            "DoZSeries" => {
                global_do_z = parse_bool(&value);
                info.do_z_series = global_do_z;
            }
            "DoWave" => info.do_wave = parse_bool(&value),
            _ => {}
        }

        // The key for the *next* value is between the leading quote and the comma:
        // Java: key = line.substring(1, comma - 1).trim().
        if comma >= 1 {
            let c = comma as usize;
            key = line
                .get(1..c.saturating_sub(1))
                .unwrap_or("")
                .trim()
                .to_string();
        } else {
            key = String::new();
        }
        current_value.clear();
        if comma >= 0 {
            let c = comma as usize;
            current_value.push_str(line.get(c + 1..).unwrap_or("").trim());
        }
    }

    if !global_do_z {
        for z in info.wave_do_z.iter_mut() {
            *z = false;
        }
    }
    info
}

fn parse_bool(s: &str) -> bool {
    s.trim().eq_ignore_ascii_case("true")
}

/// The suffix Java appends to STK names for a given wave/format version.
fn nd_format_suffix(
    version: &str,
    has_z_for_wave: bool,
    any_z: bool,
    global_do_z: bool,
) -> &'static str {
    if version == NDINFOFILE_VER1 {
        if (any_z && has_z_for_wave) || global_do_z {
            ".STK"
        } else {
            ".TIF"
        }
    } else if version == NDINFOFILE_VER2 {
        ".TIF"
    } else {
        ".STK"
    }
}

/// Sanitize a wavelength name for use in a filename (Java translates
/// `_ / \ ( )` to `-`).
fn sanitize_wave_name(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '_' | '/' | '\\' | '(' | ')' => '-',
            other => other,
        })
        .collect()
}

/// Resolve an STK/TIFF file referenced by the `.nd` grid, trying extension
/// fallbacks (`.STK` → `.TIF` → `.tif` → `.stk`) like Java `getRealSTKFile`.
fn resolve_real_stk(dir: &Path, name: &str) -> Option<PathBuf> {
    let direct = dir.join(name);
    if direct.exists() {
        return Some(direct);
    }
    let candidates = if name.contains('%') {
        vec![name.replace('%', "-"), name.to_string()]
    } else {
        vec![name.to_string()]
    };
    for cand in candidates {
        let p = dir.join(&cand);
        if p.exists() {
            return Some(p);
        }
        let stem = match cand.rfind('.') {
            Some(i) => &cand[..i],
            None => &cand,
        };
        for ext in [".TIF", ".tif", ".stk", ".STK"] {
            let alt = dir.join(format!("{stem}{ext}"));
            if alt.exists() {
                return Some(alt);
            }
        }
    }
    None
}

/// Locate a sibling `.nd` file for the given STK path (Java initFile's
/// canLookForND branch, simplified): match on the shared prefix up to the first
/// underscore, validating that trailing chars look like `_w`/`_t`/`_s`.
fn find_nd_file(stk: &Path) -> Option<PathBuf> {
    let dir = stk.parent()?;
    let stk_name = stk.file_name()?.to_str()?;
    let stk_prefix = match stk_name.find('_') {
        Some(i) => &stk_name[..i + 1],
        None => stk_name,
    };
    let mut best: Option<(usize, PathBuf)> = None;
    for entry in std::fs::read_dir(dir).ok()?.filter_map(|e| e.ok()) {
        let fname = entry.file_name();
        let fname = match fname.to_str() {
            Some(s) => s.to_string(),
            None => continue,
        };
        let lower = fname.to_ascii_lowercase();
        if !(lower.ends_with(".nd") || lower.ends_with(".scan")) {
            continue;
        }
        let prefix = match fname.rfind('.') {
            Some(i) => &fname[..i],
            None => &fname,
        };
        let prefix = match prefix.find('_') {
            Some(i) => &prefix[..i + 1],
            None => prefix,
        };
        if stk_name.starts_with(prefix) || prefix == stk_prefix {
            let mut count = 0;
            for (a, b) in fname.chars().zip(stk_name.chars()) {
                if a == b {
                    count += 1;
                } else {
                    break;
                }
            }
            let extra = stk_name[count.min(stk_name.len())..].to_ascii_lowercase();
            let extra_bytes = extra.as_bytes();
            let mut valid = true;
            for i in 0..extra_bytes.len().saturating_sub(1) {
                if extra_bytes[i] == b'_' {
                    let ch = extra_bytes[i + 1];
                    if ch != b'w' && ch != b't' && ch != b's' {
                        valid = false;
                        break;
                    }
                }
            }
            if valid && best.as_ref().map(|(c, _)| count > *c).unwrap_or(true) {
                best = Some((count, entry.path()));
            }
        }
    }
    best.map(|(_, p)| p)
}

/// The `stks[series][file]` grid plus the derived base dimensions.
struct NdGrid {
    stks: Vec<Vec<Option<PathBuf>>>,
    size_z: u32,
    size_c: u32,
    size_t: u32,
    bizarre_multichannel: bool,
}

/// Build the `stks[series][file]` grid from a parsed `.nd` file, porting Java
/// `initFile`'s series/file enumeration.
#[allow(clippy::too_many_lines)]
fn build_nd_grid(nd_path: &Path, info: &NdInfo) -> NdGrid {
    let dir = nd_path.parent().unwrap_or_else(|| Path::new("."));
    let prefix = nd_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();

    let zc = info.z_steps.unwrap_or(1).max(1);
    let mut cc = info.n_wavelengths.unwrap_or(1);
    let mut tc = match info.n_time_points {
        Some(t) => t,
        None if !info.do_timelapse => 1,
        None => 1,
    };

    if cc == 0 {
        cc = 1;
    }
    if cc == 1 && info.bizarre_multichannel {
        cc = 2;
    }
    if tc == 0 {
        tc = 1;
    }

    let nstages = info.n_stage_positions;
    let mut num_files = cc * tc;
    if nstages > 0 {
        num_files *= nstages;
    }

    let stages_count = if nstages == 0 { 1 } else { nstages };
    let mut series_count = stages_count;

    // Detect channels with differing Z behaviour -> series doubling.
    let has_z = &info.wave_do_z;
    let mut different_zs = false;
    for i in 0..cc as usize {
        let has_z1 = i < has_z.len() && has_z[i];
        let has_z2 = i != 0 && (i - 1) < has_z.len() && has_z[i - 1];
        if i > 0 && has_z1 != has_z2 && info.do_z_series {
            if !different_zs {
                series_count *= 2;
            }
            different_zs = true;
        }
    }

    let mut channels_in_first_series = cc;
    if different_zs {
        channels_in_first_series = 0;
        for i in 0..cc as usize {
            let z0 = has_z.first().copied().unwrap_or(false);
            if (!z0 && i == 0) || (z0 && i < has_z.len() && has_z[i]) {
                channels_in_first_series += 1;
            }
        }
    }

    // Allocate the grid.
    let series_count = series_count.max(1) as usize;
    let mut stks: Vec<Vec<Option<PathBuf>>> = vec![Vec::new(); series_count];
    if series_count == 1 {
        stks[0] = vec![None; num_files.max(0) as usize];
    } else if different_zs {
        for i in 0..stages_count as usize {
            stks[i * 2] = vec![None; (channels_in_first_series * tc).max(0) as usize];
            stks[i * 2 + 1] = vec![None; ((cc - channels_in_first_series) * tc).max(0) as usize];
        }
    } else {
        let per = num_files as usize / series_count;
        for s in stks.iter_mut() {
            *s = vec![None; per];
        }
    }

    let any_z = has_z.iter().any(|&z| z);
    let global_do_z = info.do_z_series;
    let mut pt = vec![0usize; series_count];

    for i in 0..tc {
        for s in 0..stages_count {
            for j in 0..cc {
                let valid_z = (j as usize) >= has_z.len() || has_z[j as usize];
                let mut series_ndx = (s as usize) * (series_count / stages_count.max(1) as usize);

                if (series_count != 1 && (!valid_z || (!has_z.is_empty() && !has_z[0])))
                    || (nstages == 0 && ((!valid_z && cc > 1) || series_count > 1))
                {
                    if any_z
                        && j > 0
                        && series_ndx < series_count - 1
                        && (!valid_z || !has_z.first().copied().unwrap_or(false))
                    {
                        series_ndx += 1;
                    }
                }

                if series_ndx >= stks.len()
                    || series_ndx >= pt.len()
                    || pt[series_ndx] >= stks[series_ndx].len()
                {
                    continue;
                }

                let mut name = prefix.clone();
                let has_z_for_wave = (j as usize) < has_z.len() && has_z[j as usize];
                let suffix = nd_format_suffix(&info.version, has_z_for_wave, any_z, global_do_z);

                if (j as usize) < info.wave_names.len() && info.do_wave {
                    name.push_str(&format!("_w{}", j + 1));
                    if info.use_wave_names {
                        name.push_str(&sanitize_wave_name(&info.wave_names[j as usize]));
                    }
                }
                if nstages > 0 {
                    name.push_str(&format!("_s{}", s + 1));
                }
                if tc > 1 || info.do_timelapse {
                    name.push_str(&format!("_t{}{}", i + 1, suffix));
                } else {
                    name.push_str(suffix);
                }

                stks[series_ndx][pt[series_ndx]] = resolve_real_stk(dir, &name);
                pt[series_ndx] += 1;
            }
        }
    }

    let size_z = if !has_z.is_empty() && !has_z[0] {
        1
    } else {
        zc
    };
    NdGrid {
        stks,
        size_z: size_z.max(1) as u32,
        size_c: cc.max(1) as u32,
        size_t: tc.max(1) as u32,
        bizarre_multichannel: info.bizarre_multichannel,
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
        let plane_bytes = (meta.size_x as usize)
            .checked_mul(meta.size_y as usize)
            .and_then(|px| px.checked_mul(bps))
            .ok_or_else(|| {
                BioFormatsError::InvalidData("MetaMorph STK plane byte count overflow".into())
            })?;

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
        let offset = (plane_index as u64)
            .checked_mul(plane_bytes as u64)
            .and_then(|delta| base_offset.checked_add(delta))
            .ok_or_else(|| {
                BioFormatsError::InvalidData("MetaMorph STK plane offset overflow".into())
            })?;

        let mut file = File::open(path).map_err(BioFormatsError::Io)?;
        let len = file.metadata().map_err(BioFormatsError::Io)?.len();
        let end = offset.checked_add(plane_bytes as u64).ok_or_else(|| {
            BioFormatsError::InvalidData("MetaMorph STK plane end offset overflow".into())
        })?;
        if end > len {
            return Err(BioFormatsError::InvalidData(format!(
                "MetaMorph STK plane {plane_index} is truncated: need bytes {offset}..{end}, file length {len}"
            )));
        }
        let mut out = vec![0u8; plane_bytes];
        file.seek(SeekFrom::Start(offset))
            .map_err(BioFormatsError::Io)?;
        file.read_exact(&mut out).map_err(BioFormatsError::Io)?;
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
            .map(|e| {
                let e = e.to_ascii_lowercase();
                e == "stk" || e == "nd" || e == "scan"
            })
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        // STK is a TIFF; we rely on extension detection
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        // Determine whether we were given an .nd/.scan companion or an STK.
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        let is_nd_entry = matches!(ext.as_deref(), Some("nd") | Some("scan"));

        // Resolve the .nd file and the STK file to initialize the inner reader.
        let (nd_file, stk_path): (Option<PathBuf>, PathBuf) = if is_nd_entry {
            // Given an .nd file: find an associated STK in the same directory
            // (Java: scan the directory for `<prefix>` + `` / `_w` / `_s` / `_t`).
            let stk = find_first_stk_for_nd(path).ok_or_else(|| {
                BioFormatsError::Format(format!(
                    "MetaMorph: no STK file found alongside {}",
                    path.display()
                ))
            })?;
            (Some(path.to_path_buf()), stk)
        } else {
            // Given an STK: look for a sibling .nd file.
            (find_nd_file(path), path.to_path_buf())
        };

        // Build single-STK base metadata from the (first) STK file.
        let (base_meta, _mm_planes) = self.init_single_stk(&stk_path)?;

        // If a companion .nd describes a multi-file group, assemble the series.
        if let Some(nd) = nd_file {
            if let Ok(text) = read_nd_text(&nd) {
                let info = parse_nd(&text);
                let grid = build_nd_grid(&nd, &info);
                if grid.stks.iter().any(|s| s.iter().any(|f| f.is_some())) {
                    self.build_series_from_grid(nd, base_meta, grid)?;
                    self.path = Some(stk_path);
                    return Ok(());
                }
            }
        }

        // Single-file STK path: one series.
        self.metas = vec![base_meta.clone()];
        self.series = 0;
        self.stks = None;
        self.nd_filename = None;
        self.meta = Some(base_meta);
        self.path = Some(stk_path);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.stks = None;
        self.metas.clear();
        self.series = 0;
        self.nd_filename = None;
        let _ = self.inner.close();
        Ok(())
    }

    fn series_count(&self) -> usize {
        self.metas.len().max(1)
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if s >= self.metas.len().max(1) {
            return Err(BioFormatsError::SeriesOutOfRange(s));
        }
        self.series = s;
        if let Some(m) = self.metas.get(s) {
            self.meta = Some(m.clone());
        }
        Ok(())
    }

    fn series(&self) -> usize {
        self.series
    }

    fn metadata(&self) -> &ImageMetadata {
        self.metas
            .get(self.series)
            .or(self.meta.as_ref())
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let count = self
            .metas
            .get(self.series)
            .map(|m| m.image_count)
            .or_else(|| self.meta.as_ref().map(|m| m.image_count))
            .unwrap_or(0);
        if plane_index >= count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        // Multi-file .nd series: read from the constituent STK grid.
        if let Some(bytes) = self.open_grid_plane(plane_index)? {
            return Ok(bytes);
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
        let count = self
            .metas
            .get(self.series)
            .map(|m| m.image_count)
            .or_else(|| self.meta.as_ref().map(|m| m.image_count))
            .unwrap_or(0);
        if plane_index >= count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        // For .nd series, crop the full grid plane.
        if self.stks.is_some() {
            let full = self.open_bytes(plane_index)?;
            let meta = self.metas.get(self.series).unwrap();
            return crop_full_plane("MetaMorph STK", &full, meta, 1, x, y, w, h);
        }
        let inner_count = self.inner.metadata().image_count;
        if plane_index < inner_count {
            return self.inner.open_bytes_region(plane_index, x, y, w, h);
        }
        // Crop from the concatenated-strip plane.
        let full = self.read_concatenated_plane(plane_index)?;
        let meta = self.meta.as_ref().unwrap();
        crop_full_plane("MetaMorph STK", &full, meta, 1, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self
            .metas
            .get(self.series)
            .or(self.meta.as_ref())
            .ok_or(BioFormatsError::NotInitialized)?;
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = ((meta.size_x - tw) / 2, (meta.size_y - th) / 2);
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}

/// Find the first STK/TIFF file in the `.nd` file's directory whose name starts
/// with the `.nd` prefix and continues with `` / `_w` / `_s` / `_t` (Java
/// initFile's `.nd` entry branch).
fn find_first_stk_for_nd(nd_path: &Path) -> Option<PathBuf> {
    let dir = nd_path.parent()?;
    let stem = nd_path.file_stem().and_then(|s| s.to_str())?;
    let mut matches: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            let name = match p.file_name().and_then(|n| n.to_str()) {
                Some(n) => n,
                None => return false,
            };
            let lower = name.to_ascii_lowercase();
            if !(lower.ends_with(".stk") || lower.ends_with(".tif") || lower.ends_with(".tiff")) {
                return false;
            }
            if !name.starts_with(stem) {
                return false;
            }
            let rest = &name[stem.len()..];
            let middle = match rest.rfind('.') {
                Some(i) => &rest[..i],
                None => rest,
            };
            middle.is_empty()
                || middle.starts_with("_w")
                || middle.starts_with("_s")
                || middle.starts_with("_t")
        })
        .collect();
    matches.sort();
    matches.into_iter().next()
}

impl MetamorphReader {
    /// Build per-series core metadata + the `stks` grid from a parsed `.nd`
    /// grid, cloning the single-STK base metadata for sizing (Java initFile's
    /// `newCore` block).
    fn build_series_from_grid(
        &mut self,
        nd: PathBuf,
        base_meta: ImageMetadata,
        grid: NdGrid,
    ) -> Result<()> {
        // Determine X/Y (and pixel type) from the first valid STK in the grid.
        let first = grid
            .stks
            .iter()
            .flat_map(|s| s.iter())
            .flatten()
            .next()
            .cloned();
        let mut probe_meta = base_meta.clone();
        if let Some(f) = &first {
            let (m, _) = self.init_single_stk(f)?;
            probe_meta = m;
        }

        let mut size_x = probe_meta.size_x;
        if grid.bizarre_multichannel {
            size_x /= 2;
        }
        let size_z = grid.size_z;
        let size_c = grid.size_c;
        let size_t = grid.size_t;
        let rgb_mult = if probe_meta.is_rgb { 3 } else { 1 };
        let image_count = size_z * (size_c * rgb_mult) * size_t;

        let series_count = grid.stks.len().max(1);
        let mut metas: Vec<ImageMetadata> = Vec::with_capacity(series_count);
        for s in 0..series_count {
            let mut sm = probe_meta.series_metadata.clone();
            sm.insert(
                "format".into(),
                MetadataValue::String("MetaMorph STK".into()),
            );
            sm.insert(
                "ndFilename".into(),
                MetadataValue::String(nd.to_string_lossy().into_owned()),
            );
            sm.insert("series".into(), MetadataValue::Int(s as i64));
            metas.push(ImageMetadata {
                size_x,
                size_z,
                size_c: size_c * rgb_mult,
                size_t,
                image_count,
                dimension_order: DimensionOrder::XYZCT,
                series_metadata: sm,
                ..probe_meta.clone()
            });
        }

        self.stks = Some(grid.stks);
        self.metas = metas;
        self.series = 0;
        self.nd_filename = Some(nd);
        self.meta = self.metas.first().cloned();
        Ok(())
    }

    /// Derive single-STK base metadata (the original single-file path), returning
    /// it along with the `mmPlanes` count.
    fn init_single_stk(&mut self, path: &Path) -> Result<(ImageMetadata, u32)> {
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

        Ok((meta, mm_planes))
    }

    /// Resolve and read a plane from the `.nd` series grid (Java openBytes when
    /// `stks != null`). Maps the plane index to a constituent STK file and the
    /// plane within it.
    fn open_grid_plane(&mut self, plane_index: u32) -> Result<Option<Vec<u8>>> {
        let stks = match &self.stks {
            Some(s) => s,
            None => return Ok(None),
        };
        let series = self.series;
        let meta = self
            .metas
            .get(series)
            .ok_or(BioFormatsError::NotInitialized)?;
        let size_z = meta.size_z.max(1);
        let row = stks.get(series).ok_or(BioFormatsError::NotInitialized)?;
        if row.is_empty() {
            return Ok(None);
        }
        // ndx = no / sizeZ; plane within the file = no % sizeZ (Java).
        let (ndx, plane) = if row.len() == 1 {
            (0usize, plane_index)
        } else {
            ((plane_index / size_z) as usize, plane_index % size_z)
        };
        let file = match row.get(ndx).and_then(|f| f.clone()) {
            Some(f) => f,
            // Missing file: return a blank plane (Java returns the buffer as-is).
            None => {
                let bps = meta.pixel_type.bytes_per_sample();
                return Ok(Some(vec![
                    0u8;
                    meta.size_x as usize * meta.size_y as usize * bps
                ]));
            }
        };
        let mut reader = MetamorphReader::new();
        reader.set_id(&file)?;
        let inner = reader.metadata().image_count.max(1);
        Ok(Some(reader.open_bytes(plane % inner)?))
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

    // ── .nd companion file (multi-STK series) tests ────────────────────────

    #[test]
    fn metamorph_parse_nd_extracts_dimensions_and_waves() {
        // Mirrors the .nd key/value grammar Java MetamorphReader.initFile reads
        // ("Key", value lines, EndFile terminator).
        let nd = "\"NDInfoFile\", Version 1.0\n\
                  \"DoTimelapse\", TRUE\n\
                  \"NTimePoints\", 3\n\
                  \"DoStage\", TRUE\n\
                  \"NStagePositions\", 2\n\
                  \"DoWave\", TRUE\n\
                  \"NWavelengths\", 2\n\
                  \"WaveName1\", \"FITC\"\n\
                  \"WaveName2\", \"DAPI\"\n\
                  \"WaveInFileName\", TRUE\n\
                  \"DoZSeries\", FALSE\n\
                  \"EndFile\",\n";
        let info = parse_nd(nd);
        assert_eq!(info.version, "Version 1.0");
        assert!(info.do_timelapse);
        assert_eq!(info.n_time_points, Some(3));
        assert_eq!(info.n_stage_positions, 2);
        assert!(info.do_wave);
        assert_eq!(info.n_wavelengths, Some(2));
        assert_eq!(
            info.wave_names,
            vec!["FITC".to_string(), "DAPI".to_string()]
        );
        assert!(info.use_wave_names);
        assert!(!info.do_z_series);
    }

    #[test]
    fn metamorph_build_nd_grid_enumerates_per_position_series_filenames() {
        // 2 stage positions, 2 waves, 3 time points, no Z. Java builds one
        // series per stage position, each holding sizeC * sizeT files named
        // `<prefix>_w<wave><name>_s<stage>_t<time>.TIF` (DoZSeries FALSE ->
        // .TIF for V1.0).
        let dir = std::env::temp_dir().join(format!(
            "mm_nd_grid_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let prefix = "exp";
        // Create the expected constituent files so resolve_real_stk finds them.
        for s in 1..=2 {
            for t in 1..=3 {
                for (w, name) in [(1, "FITC"), (2, "DAPI")] {
                    let fname = format!("{prefix}_w{w}{name}_s{s}_t{t}.TIF");
                    std::fs::write(dir.join(&fname), b"x").unwrap();
                }
            }
        }
        let nd_path = dir.join(format!("{prefix}.nd"));
        std::fs::write(&nd_path, b"placeholder").unwrap();

        let info = NdInfo {
            version: NDINFOFILE_VER1.to_string(),
            n_wavelengths: Some(2),
            n_time_points: Some(3),
            do_timelapse: true,
            do_z_series: false,
            do_wave: true,
            n_stage_positions: 2,
            use_wave_names: true,
            wave_names: vec!["FITC".into(), "DAPI".into()],
            wave_do_z: vec![],
            ..Default::default()
        };
        let grid = build_nd_grid(&nd_path, &info);

        // One series per stage position.
        assert_eq!(grid.stks.len(), 2);
        // sizeC * sizeT = 2 * 3 files per series.
        assert_eq!(grid.stks[0].len(), 6);
        assert_eq!(grid.stks[1].len(), 6);
        // Every referenced file resolved to a real path.
        for series in &grid.stks {
            for f in series {
                assert!(f.is_some(), "expected resolved STK path, got None");
            }
        }
        // The first file of series 0 is the wave-1, stage-1, time-1 file.
        let first = grid.stks[0][0].as_ref().unwrap();
        assert_eq!(
            first.file_name().unwrap().to_str().unwrap(),
            "exp_w1FITC_s1_t1.TIF"
        );
        // Series 1 (stage 2) starts at _s2.
        let s2first = grid.stks[1][0].as_ref().unwrap();
        assert!(s2first
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .contains("_s2_t1"));
        assert_eq!(grid.size_c, 2);
        assert_eq!(grid.size_t, 3);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn metamorph_build_nd_grid_single_position_single_series() {
        // No stage positions, single wave, single time point -> one series with
        // one file named `<prefix>.STK` (DoZSeries default true -> .STK).
        let dir = std::env::temp_dir().join(format!(
            "mm_nd_single_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("solo.STK"), b"x").unwrap();
        let nd_path = dir.join("solo.nd");
        std::fs::write(&nd_path, b"x").unwrap();

        let info = NdInfo {
            version: NDINFOFILE_VER1.to_string(),
            n_wavelengths: Some(1),
            n_time_points: Some(1),
            do_timelapse: false,
            do_z_series: true,
            do_wave: false,
            n_stage_positions: 0,
            ..Default::default()
        };
        let grid = build_nd_grid(&nd_path, &info);
        assert_eq!(grid.stks.len(), 1);
        assert_eq!(grid.stks[0].len(), 1);
        assert_eq!(
            grid.stks[0][0].as_ref().unwrap().file_name().unwrap(),
            "solo.STK"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
