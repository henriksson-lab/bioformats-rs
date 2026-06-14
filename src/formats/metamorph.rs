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

#[allow(dead_code)]
const UIC1_TAG: u16 = 33628;
#[allow(dead_code)]
const UIC2_TAG: u16 = 33629;
#[allow(dead_code)]
const UIC3_TAG: u16 = 33630;
#[allow(dead_code)]
const UIC4_TAG: u16 = 33631;

/// Read the plane count (`mmPlanes`) from the UIC TIFF entries.
///
/// Java `MetamorphReader.java:1337-1342`: `mmPlanes = uic4tagEntry.getValueCount()`
/// (the UIC4 TIFF entry's value *count*), falling back to `ifds.size()`. The
/// previous implementation incorrectly used UIC1Tag's first rational numerator,
/// but UIC1 is a list of (fieldID, value) pairs, so that numerator is a metadata
/// field-ID, not a plane count.
///
/// Here we return the UIC4 entry's `count`, falling back to UIC2's `count`.
/// The final `ifds.size()` fallback is applied by the caller.
fn read_uic_plane_count(path: &Path) -> Result<Option<u32>> {
    let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
    // Prefer the UIC4 TIFF entry's value count (Java mmPlanes source).
    if let Some((_, _, count)) = read_tag_value_offset(&data, UIC4_TAG) {
        if count > 0 {
            return Ok(Some(count));
        }
    }
    // Fall back to the UIC2 entry's value count.
    if let Some((_, _, count)) = read_tag_value_offset(&data, UIC2_TAG) {
        if count > 0 {
            return Ok(Some(count));
        }
    }
    Ok(None)
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
    // Java MetamorphReader.java:1316 seeds sizeZ = 1 before parseUIC2Tags
    // (line 1740) increments it once per non-zero z-distance. Mirror that base
    // of 1 so the downstream Z/T reconciliation matches Java bitwise.
    let mut size_z = 1u32;
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

/// Read a signed 4-byte int at `off` with the given endianness.
fn rd_i32_at(data: &[u8], off: usize, le: bool) -> Option<i32> {
    if off + 4 > data.len() {
        return None;
    }
    Some(if le {
        i32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
    } else {
        i32::from_be_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
    })
}

/// Read a TIFF rational (num/den i32 pair) at `off`. Returns `num` when the
/// denominator is zero, matching Java `TiffRational.doubleValue`.
fn read_rational_at(data: &[u8], off: usize, le: bool) -> Option<f64> {
    let num = rd_i32_at(data, off, le)?;
    let den = rd_i32_at(data, off + 4, le)?;
    Some(if den != 0 {
        num as f64 / den as f64
    } else {
        num as f64
    })
}

/// Java `FormatTools.getPhysicalSize*`: a physical size is only emitted when it
/// is strictly positive.
fn physical_size(value: f64) -> Option<f64> {
    if value > 0.0 {
        Some(value)
    } else {
        None
    }
}

/// Parse the UIC1 X/Y calibration rationals (field IDs 4/5) and the first UIC2
/// z-distance, returning the physical pixel sizes in µm. Mirrors Java
/// MetamorphReader's XCalibration/YCalibration handling and `stepSize =
/// zDistances[0]`.
fn read_metamorph_physical_sizes(path: &Path) -> (Option<f64>, Option<f64>, Option<f64>) {
    let Ok(data) = std::fs::read(path) else {
        return (None, None, None);
    };
    let mut phys_x = None;
    let mut phys_y = None;
    if let Some((le, uic1_offset, count)) = read_tag_value_offset(&data, UIC1_TAG) {
        let mut off = uic1_offset as usize;
        for _ in 0..count {
            let (Some(id), Some(val_or_offset)) =
                (rd_i32_at(&data, off, le), rd_i32_at(&data, off + 4, le))
            else {
                break;
            };
            let val_off = val_or_offset as u32 as usize;
            match id {
                4 => phys_x = read_rational_at(&data, val_off, le).and_then(physical_size),
                5 => phys_y = read_rational_at(&data, val_off, le).and_then(physical_size),
                _ => {}
            }
            off += 8;
        }
    }
    // physicalSizeZ comes from the first UIC2 z-distance (Java stepSize).
    let phys_z = read_tag_value_offset(&data, UIC2_TAG)
        .and_then(|(le, uic2_offset, _)| read_rational_at(&data, uic2_offset as usize, le))
        .and_then(physical_size);
    (phys_x, phys_y, phys_z)
}

/// Per-file data fields extracted from the UIC1/UIC2/UIC4 proprietary tables,
/// mirroring the instance fields Java `MetamorphReader` carries. These describe
/// the acquisition (camera/stage/timing) for a single STK file.
#[derive(Default, Clone)]
struct MetamorphData {
    /// Display name (UIC1 field 7; Java `imageName`).
    image_name: Option<String>,
    /// Acquisition date/time string (UIC1 field 17 `LastSavedTime`; Java
    /// `imageCreationDate`).
    image_creation_date: Option<String>,
    /// Camera binning, e.g. `"2x2"` (UIC1 field 46 `CameraBin`; Java `binning`).
    binning: Option<String>,
    /// Per-plane stage X positions in reference-frame units (UIC1/UIC4 field 28;
    /// Java `stageX`).
    stage_x: Vec<f64>,
    /// Per-plane stage Y positions (UIC1/UIC4 field 28; Java `stageY`).
    stage_y: Vec<f64>,
    /// Per-plane absolute Z positions accumulated into stage Z (UIC1 field 40 /
    /// UIC4 field 40 `AbsoluteZ`; Java `zDistances`/`zStart` feed planePositionZ).
    /// We store the raw per-plane absolute-Z list here.
    absolute_z: Vec<f64>,
    /// Per-plane stage labels (UIC4 field 37 `readStageLabels`; Java
    /// `stageLabels`).
    stage_labels: Vec<String>,
    /// Per-plane z-distances (UIC2; Java `zDistances`).
    z_distances: Vec<f64>,
    /// Per-plane creation timestamps in ms since epoch (UIC2; Java
    /// `internalStamps`).
    internal_stamps: Vec<i64>,
    /// Emission wavelengths from UIC3 (Java `emWavelength` / `wave`).
    em_wavelength: Vec<f64>,
    /// Z step size, `zDistances[0]` (Java `stepSize`).
    step_size: Option<f64>,
    /// Whether UIC field 28 (stage positions) was present (Java
    /// `hasStagePositions`).
    has_stage_positions: bool,
    /// Whether UIC field 40 (absolute Z) was present (Java `hasAbsoluteZ`).
    has_absolute_z: bool,
    /// First absolute-Z value when valid (Java `tempZ`).
    temp_z: f64,
    /// Whether the first plane's absolute-Z is valid (Java `validZ`).
    valid_z: bool,
}

/// Read a UIC1/UIC4 stage-position table (`mmPlanes` pairs of rationals) at
/// `off`, returning (stageX, stageY). Mirrors Java `readStagePositions`.
fn read_stage_positions(data: &[u8], off: usize, le: bool, mm_planes: u32) -> (Vec<f64>, Vec<f64>) {
    let mut xs = Vec::new();
    let mut ys = Vec::new();
    let mut o = off;
    for _ in 0..mm_planes {
        let (Some(x), Some(y)) = (read_rational_at(data, o, le), read_rational_at(data, o + 8, le))
        else {
            break;
        };
        xs.push(x);
        ys.push(y);
        o += 16;
    }
    (xs, ys)
}

/// Read a UIC absolute-Z rational table (`mmPlanes` rationals) at `off`,
/// returning the per-plane values. Mirrors Java `readRationals(["..absoluteZ"])`.
fn read_absolute_z(data: &[u8], off: usize, le: bool, mm_planes: u32) -> Vec<f64> {
    let mut out = Vec::new();
    let mut o = off;
    for _ in 0..mm_planes {
        let Some(v) = read_rational_at(data, o, le) else {
            break;
        };
        out.push(v);
        o += 8;
    }
    out
}

/// Read the UIC4 stage-label table (`mmPlanes` length-prefixed C-strings) at
/// `off`. Mirrors Java `readStageLabels` / `readCString`.
fn read_stage_labels(data: &[u8], off: usize, le: bool, mm_planes: u32) -> Vec<String> {
    let mut out = Vec::new();
    let mut o = off;
    for _ in 0..mm_planes {
        let Some(len) = rd_i32_at(data, o, le) else {
            break;
        };
        o += 4;
        if len < 0 {
            break;
        }
        let len = len as usize;
        if o + len > data.len() {
            break;
        }
        let bytes = &data[o..o + len];
        let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        out.push(String::from_utf8_lossy(&bytes[..end]).into_owned());
        o += len;
    }
    out
}

/// Read a length-prefixed string at `off` (Java `in.readInt(); in.readString`).
fn read_len_prefixed_string(data: &[u8], off: usize, le: bool) -> Option<String> {
    let n = rd_i32_at(data, off, le)?;
    if n < 0 {
        return None;
    }
    let n = n as usize;
    let start = off + 4;
    if start + n > data.len() {
        return None;
    }
    Some(String::from_utf8_lossy(&data[start..start + n]).into_owned())
}

/// Read a Julian date + ms-of-day pair at `off` and format `dd/mm/yyyy hh:mm:ss:SSS`
/// (Java UIC1 field 16/17 handling: `decodeDate + " " + decodeTime`).
fn read_uic1_datetime(data: &[u8], off: usize, le: bool) -> Option<String> {
    let date_raw = rd_i32_at(data, off, le)?;
    let time_raw = rd_i32_at(data, off + 4, le)?;
    Some(format!("{} {}", decode_date(date_raw), decode_time(time_raw)))
}

/// Parse the UIC4 binary id-list table (Java `parseUIC4Tags`), extracting the
/// stage positions (id 28), absolute Z (id 40) and stage labels (id 37). The
/// table is a sequence of 16-bit ids terminated by id 0; each known id is
/// followed by `mmPlanes` records, unknown ids by a single 4-byte value.
fn parse_uic4_data_fields(data: &[u8], off: usize, le: bool, mm_planes: u32, out: &mut MetamorphData) {
    let rd_i16 = |o: usize| -> Option<i16> {
        if o + 2 > data.len() {
            return None;
        }
        Some(if le {
            i16::from_le_bytes([data[o], data[o + 1]])
        } else {
            i16::from_be_bytes([data[o], data[o + 1]])
        })
    };
    let mut o = off;
    loop {
        let Some(id) = rd_i16(o) else { break };
        o += 2;
        if id == 0 {
            break;
        }
        match id {
            28 => {
                let (xs, ys) = read_stage_positions(data, o, le, mm_planes);
                let n = xs.len();
                if !xs.is_empty() {
                    out.stage_x = xs;
                    out.stage_y = ys;
                    out.has_stage_positions = true;
                }
                o += n * 16;
            }
            29 => {
                // cameraX/Y chip offsets: 2 rationals per plane (skip values).
                o += mm_planes as usize * 16;
            }
            37 => {
                let labels = read_stage_labels(data, o, le, mm_planes);
                // Advance past the table: re-walk to compute byte length.
                let mut adv = 0usize;
                for _ in 0..labels.len() {
                    if let Some(len) = rd_i32_at(data, o + adv, le) {
                        adv += 4 + len.max(0) as usize;
                    }
                }
                o += adv;
                if !labels.is_empty() {
                    out.stage_labels = labels;
                }
            }
            40 => {
                let zs = read_absolute_z(data, o, le, mm_planes);
                let n = zs.len();
                if !zs.is_empty() {
                    out.has_absolute_z = true;
                    if let Some(first) = zs.first() {
                        out.temp_z = *first;
                    }
                    out.absolute_z = zs;
                }
                o += n * 8;
            }
            41 => {
                // absoluteZValid: one int per plane.
                if let Some(v) = rd_i32_at(data, o, le) {
                    out.valid_z = v == 1;
                }
                o += mm_planes as usize * 4;
            }
            46 => {
                o += mm_planes as usize * 8;
            }
            _ => {
                o += 4;
            }
        }
        if o >= data.len() {
            break;
        }
    }
    if out.valid_z {
        // zStart = tempZ (Java parseUIC4Tags tail).
    }
}

/// Parse the UIC1 `(id, valueOrOffset)` table (Java `parseUIC1Tags`), extracting
/// the data fields Java keeps as instance variables: image name (7), image
/// creation date (17), binning (46), stage positions (28), absolute Z (40).
fn parse_uic1_data_fields(
    data: &[u8],
    off: usize,
    le: bool,
    count: u32,
    mm_planes: u32,
    out: &mut MetamorphData,
) {
    let mut o = off;
    for _ in 0..count {
        let (Some(id), Some(val)) = (rd_i32_at(data, o, le), rd_i32_at(data, o + 4, le)) else {
            break;
        };
        let val_off = (val as u32) as usize;
        match id {
            7 => {
                if val_off < data.len() {
                    if let Some(name) = read_len_prefixed_string(data, val_off, le) {
                        out.image_name = Some(name);
                    }
                }
            }
            17 => {
                if val_off < data.len() {
                    out.image_creation_date = read_uic1_datetime(data, val_off, le);
                }
            }
            28 => {
                if val_off < data.len() && !out.has_stage_positions {
                    let (xs, ys) = read_stage_positions(data, val_off, le, mm_planes);
                    if !xs.is_empty() {
                        out.stage_x = xs;
                        out.stage_y = ys;
                        out.has_stage_positions = true;
                    }
                }
            }
            40 => {
                if val != 0 && val_off < data.len() && !out.has_absolute_z {
                    let zs = read_absolute_z(data, val_off, le, mm_planes);
                    if !zs.is_empty() {
                        out.has_absolute_z = true;
                        if let Some(first) = zs.first() {
                            out.temp_z = *first;
                        }
                        out.absolute_z = zs;
                    }
                }
            }
            46 => {
                if val_off + 8 <= data.len() {
                    if let (Some(xb), Some(yb)) =
                        (rd_i32_at(data, val_off, le), rd_i32_at(data, val_off + 4, le))
                    {
                        out.binning = Some(format!("{xb}x{yb}"));
                    }
                }
            }
            _ => {}
        }
        o += 8;
    }
}

/// Parse all per-file UIC data fields (UIC1 + UIC4 + UIC2/UIC3) into a
/// [`MetamorphData`], mirroring Java `initStandardMetadata`'s
/// `parseUIC2Tags`/`parseUIC4Tags`/`parseUIC1Tags` sequence.
fn read_metamorph_data(path: &Path, ifd: &Ifd, mm_planes: u32) -> MetamorphData {
    let mut out = MetamorphData::default();
    let Ok(data) = std::fs::read(path) else {
        return out;
    };

    // UIC2: z-distances + internal timestamps (Java parseUIC2Tags).
    if let Some((le, uic2_offset, _)) = read_tag_value_offset(&data, UIC2_TAG) {
        let mut o = uic2_offset as usize;
        for _ in 0..mm_planes {
            let (Some(num), Some(den)) = (rd_i32_at(&data, o, le), rd_i32_at(&data, o + 4, le))
            else {
                break;
            };
            let z = if den != 0 { num as f64 / den as f64 } else { num as f64 };
            out.z_distances.push(z);
            // creation date (4B) + time (4B) -> ms timestamp.
            if let (Some(d_raw), Some(t_raw)) =
                (rd_i32_at(&data, o + 8, le), rd_i32_at(&data, o + 12, le))
            {
                out.internal_stamps
                    .push(julian_ms_timestamp(d_raw, t_raw));
            }
            o += 24;
        }
    }
    if let Some(first) = out.z_distances.first() {
        out.step_size = Some(*first);
    }

    // UIC3: emission wavelengths (Java `emWavelength`/`wave`).
    if let Some(IfdValue::Rational(waves)) = ifd.get(UIC3_TAG) {
        for (n, d) in waves {
            let v = if *d != 0 { *n as f64 / *d as f64 } else { *n as f64 };
            out.em_wavelength.push(v);
        }
    }

    // UIC4 binary table (stage positions, absolute Z, stage labels).
    if let Some((le, uic4_offset, _)) = read_tag_value_offset(&data, UIC4_TAG) {
        parse_uic4_data_fields(&data, uic4_offset as usize, le, mm_planes, &mut out);
    }

    // UIC1 (id, value) table (image name/date, binning, stage positions, abs Z).
    if let Some((le, uic1_offset, count)) = read_tag_value_offset(&data, UIC1_TAG) {
        parse_uic1_data_fields(&data, uic1_offset as usize, le, count, mm_planes, &mut out);
    }

    out
}

/// Compose a ms-since-epoch timestamp from a Julian date int and ms-of-day int,
/// approximating Java `DateTools.getTime(decodeDate + " " + decodeTime)`. We
/// compute days from the Julian date (the same epoch Java uses) directly.
fn julian_ms_timestamp(julian: i32, ms_of_day: i32) -> i64 {
    // Julian day number 2440588 == Unix epoch (1970-01-01). Metamorph stores the
    // Julian Day Number; convert to ms since epoch + ms-of-day.
    let days = julian as i64 - 2_440_588;
    days * 86_400_000 + ms_of_day.max(0) as i64
}

/// Emit the parsed [`MetamorphData`] both as Java's named series-metadata keys
/// (`Name`, `binning`, `stageX[..]`, `stageY[..]`, `stageLabel[..]`, …) and as
/// the generic OME keys the [`crate::common::ome_metadata::OmeMetadata`] builder
/// consumes, mirroring where Java's `MetadataStore` calls surface each datum:
///   stage X/Y/Z  → plane positions (setPlanePositionX/Y/Z)
///   exposure     → plane exposure  (setPlaneExposureTime)
///   binning/gain → DetectorSettings (setDetectorSettingsBinning/Gain)
///   emWavelength → channel emission (setChannelEmissionWavelength)
fn emit_metamorph_data_metadata(
    mm: &MetamorphData,
    mm_planes: u32,
    image_count: u32,
    size_c: u32,
    out: &mut HashMap<String, MetadataValue>,
) {
    if let Some(name) = &mm.image_name {
        out.insert("Name".into(), MetadataValue::String(name.clone()));
    }
    if let Some(date) = &mm.image_creation_date {
        out.insert("imageCreationDate".into(), MetadataValue::String(date.clone()));
    }
    if let Some(binning) = &mm.binning {
        out.insert("binning".into(), MetadataValue::String(binning.clone()));
    }

    // Stage X/Y per plane (Java readStagePositions addSeriesMeta + planePositionX/Y).
    for (i, x) in mm.stage_x.iter().enumerate() {
        let label = int_format_max(i as u32, mm_planes);
        out.insert(format!("stageX[{label}]"), MetadataValue::Float(*x));
        if (i as u32) < image_count {
            out.insert(format!("plane.{i}.position_x"), MetadataValue::Float(*x));
        }
    }
    for (i, y) in mm.stage_y.iter().enumerate() {
        let label = int_format_max(i as u32, mm_planes);
        out.insert(format!("stageY[{label}]"), MetadataValue::Float(*y));
        if (i as u32) < image_count {
            out.insert(format!("plane.{i}.position_y"), MetadataValue::Float(*y));
        }
    }

    // Stage Z: Java accumulates zDistances into planePositionZ from zStart, but
    // when absolute-Z is present it provides per-plane Z directly. Mirror Java's
    // zDistances accumulation: distance starts at zStart (validZ ? tempZ : 0),
    // then adds zDistances[p] (or zDistances[0] when 0) for p>0.
    if !mm.z_distances.is_empty() {
        let z_start = if mm.valid_z { mm.temp_z } else { 0.0 };
        let mut distance = z_start;
        for p in 0..image_count.min(mm.z_distances.len() as u32) {
            let pi = p as usize;
            if p > 0 {
                let d = mm.z_distances[pi];
                distance += if d != 0.0 { d } else { mm.z_distances[0] };
            }
            out.insert(format!("plane.{p}.position_z"), MetadataValue::Float(distance));
        }
    } else if !mm.absolute_z.is_empty() {
        for (i, z) in mm.absolute_z.iter().enumerate() {
            if (i as u32) < image_count {
                out.insert(format!("plane.{i}.position_z"), MetadataValue::Float(*z));
            }
        }
    }

    // Stage labels (Java readStageLabels addSeriesMeta "stageLabel[..]").
    for (i, label) in mm.stage_labels.iter().enumerate() {
        let key = int_format_max(i as u32, mm_planes);
        out.insert(
            format!("stageLabel[{key}]"),
            MetadataValue::String(label.clone()),
        );
    }

    // Emission wavelengths → per-channel emission (Java setChannelEmissionWavelength,
    // gated on >= 1 like Java's `(int) wave[waveIndex] >= 1`).
    for c in 0..size_c {
        if let Some(w) = mm.em_wavelength.get(c as usize) {
            if *w >= 1.0 {
                out.insert(
                    format!("channel.{c}.emission_wavelength"),
                    MetadataValue::Float(*w),
                );
            }
        }
    }

    // Binning → DetectorSettings per channel (Java setDetectorSettingsBinning).
    if let Some(binning) = &mm.binning {
        for c in 0..size_c {
            out.insert(
                format!("channel.{c}.detector_settings_binning"),
                MetadataValue::String(binning.clone()),
            );
        }
    }

    // Step size (Java stepSize = zDistances[0]; surfaced as physicalSizeZ via the
    // existing read_metamorph_physical_sizes path — record the raw value too).
    if let Some(step) = mm.step_size {
        out.insert("stepSize".into(), MetadataValue::Float(step));
    }
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
    /// Whether `set_id` may search for a sibling `.nd` file (Java
    /// `canLookForND`). Constituent STK readers created for an `.nd` grid set
    /// this to false to avoid re-resolving the `.nd` (Java setCanLookForND).
    can_look_for_nd: bool,
    /// Whether the `.nd` series is a "Both lasers"/DUAL bizarre multichannel
    /// acquisition (Java `bizarreMultichannelAcquisition`).
    bizarre_multichannel: bool,
    /// OME image name for a standalone STK (Java `makeImageName`, which is the
    /// empty string for a single-file STK). `None` for `.nd` series.
    image_name: Option<String>,
    /// Physical pixel sizes (µm) from the UIC1 X/Y calibration rationals and
    /// the first UIC2 z-distance (Java XCalibration/YCalibration/stepSize).
    phys_x: Option<f64>,
    phys_y: Option<f64>,
    phys_z: Option<f64>,
    /// Per-file acquisition data parsed from the UIC1/UIC2/UIC4 tables of the
    /// selected (first) STK, mirroring Java `MetamorphReader`'s data fields
    /// (stage X/Y/Z, binning, image name/date, emission wavelengths, …).
    data: MetamorphData,
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
            can_look_for_nd: true,
            bizarre_multichannel: false,
            image_name: None,
            phys_x: None,
            phys_y: None,
            phys_z: None,
            data: MetamorphData::default(),
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
    /// Number of stage positions (`NStagePositions`; Java `nstages`).
    nstages: i32,
    /// Stage count clamped to >= 1 (Java `stagesCount`).
    stages_count: i32,
    /// Z-step count (`NZSteps`; Java `zc`).
    zc: i32,
    /// Per-wavelength "do Z" flags (Java `hasZ`).
    wave_do_z: Vec<bool>,
}

/// Build the `stks[series][file]` grid from a parsed `.nd` file, porting Java
/// `initFile`'s series/file enumeration.
#[allow(clippy::too_many_lines)]
fn build_nd_grid(nd_path: &Path, info: &NdInfo, base: (u32, u32, u32)) -> NdGrid {
    let dir = nd_path.parent().unwrap_or_else(|| Path::new("."));
    let prefix = nd_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();

    // Java MetamorphReader.java:500 seeds zc/cc/tc from the base single-STK
    // getSizeZ()/getSizeC()/getSizeT(), then (587-592) overrides each only when
    // the .nd declares NZSteps/NWavelengths/NTimePoints (with the doTimelapse
    // special case for tc).
    let (base_z, base_c, base_t) = base;
    let zc = info.z_steps.unwrap_or(base_z as i32).max(1);
    let mut cc = info.n_wavelengths.unwrap_or(base_c as i32);
    let mut tc = match info.n_time_points {
        Some(t) => t,
        None if !info.do_timelapse => 1,
        None => base_t as i32,
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
        nstages,
        stages_count,
        zc,
        wave_do_z: has_z.clone(),
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
        // RGB STK planes store all samples interleaved (chunky) within the
        // plane, so the on-disk stride spans every sample (Java reconstructs one
        // multi-sample IFD per plane). Account for that here, otherwise the
        // per-plane offset for plane > 0 lands a third of the way into the plane.
        let samples = if meta.is_rgb {
            meta.size_c.max(1) as usize
        } else {
            1
        };
        let pixel_count = (meta.size_x as usize)
            .checked_mul(meta.size_y as usize)
            .ok_or_else(|| {
                BioFormatsError::InvalidData("MetaMorph STK plane byte count overflow".into())
            })?;
        let plane_bytes = pixel_count
            .checked_mul(samples)
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
        // On-disk RGB data is chunky (RGBRGB…); Bio-Formats exposes STK planes
        // de-interleaved (isInterleaved() == false), i.e. planar RRR…GGG…BBB.
        if samples > 1 {
            let mut planar = vec![0u8; plane_bytes];
            for c in 0..samples {
                for px in 0..pixel_count {
                    let dst = (c * pixel_count + px) * bps;
                    let src = (px * samples + c) * bps;
                    planar[dst..dst + bps].copy_from_slice(&out[src..src + bps]);
                }
            }
            return Ok(planar);
        }
        Ok(out)
    }

    /// Crop a region from a full planar plane (RRR…GGG…BBB), keeping the planar
    /// layout. Each sample sub-plane is cropped independently and concatenated.
    fn crop_planar_plane(
        meta: &ImageMetadata,
        full: &[u8],
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let samples = if meta.is_rgb {
            meta.size_c.max(1) as usize
        } else {
            1
        };
        if samples <= 1 {
            return crop_full_plane("MetaMorph STK", full, meta, 1, x, y, w, h);
        }
        let bps = meta.pixel_type.bytes_per_sample();
        let sub_len = (meta.size_x as usize) * (meta.size_y as usize) * bps;
        let mut out = Vec::with_capacity(samples * (w as usize) * (h as usize) * bps);
        for c in 0..samples {
            let start = c * sub_len;
            let sub = full.get(start..start + sub_len).ok_or_else(|| {
                BioFormatsError::InvalidData("MetaMorph STK plane too short".into())
            })?;
            out.extend(crop_full_plane("MetaMorph STK", sub, meta, 1, x, y, w, h)?);
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
        self.close()?;
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
        } else if self.can_look_for_nd {
            // Given an STK: look for a sibling .nd file (Java initFile:430,
            // gated on canLookForND).
            (find_nd_file(path), path.to_path_buf())
        } else {
            // Constituent STK reader: never re-resolve the .nd.
            (None, path.to_path_buf())
        };

        // Build single-STK base metadata from the (first) STK file.
        let (base_meta, _mm_planes) = self.init_single_stk(&stk_path)?;

        // If a companion .nd describes a multi-file group, assemble the series.
        if let Some(nd) = nd_file {
            if let Ok(text) = read_nd_text(&nd) {
                let info = parse_nd(&text);
                let grid = build_nd_grid(
                    &nd,
                    &info,
                    (base_meta.size_z, base_meta.size_c, base_meta.size_t),
                );
                if grid.stks.iter().any(|s| s.iter().any(|f| f.is_some())) {
                    self.build_series_from_grid(nd, base_meta, grid)?;
                    self.path = Some(stk_path);
                    return Ok(());
                }
            }
        }

        // Single-file STK path: one series.
        // OME image name for a standalone STK is the empty string (Java
        // makeImageName returns "" with no stage names / channel grid).
        self.image_name = Some(String::new());
        let (px, py, pz) = read_metamorph_physical_sizes(&stk_path);
        self.phys_x = px;
        self.phys_y = py;
        self.phys_z = pz;
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
        self.bizarre_multichannel = false;
        self.image_name = None;
        self.phys_x = None;
        self.phys_y = None;
        self.phys_z = None;
        self.data = MetamorphData::default();
        let _ = self.inner.close();
        Ok(())
    }

    fn series_count(&self) -> usize {
        self.metas.len()
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.metas.is_empty() {
            return Err(BioFormatsError::NotInitialized);
        }
        if s >= self.metas.len() {
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
        // Planes map 1:1 to the inner TIFF reader only when it exposes a
        // distinct IFD for every plane (true multi-IFD STK). When the STK packs
        // all planes as strips in a single IFD, read every plane (including
        // plane 0) from the concatenated strip data so RGB de-interleaving and
        // per-plane stride stay consistent across planes.
        if inner_count >= count {
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
        if inner_count >= count {
            return self.inner.open_bytes_region(plane_index, x, y, w, h);
        }
        // Crop from the concatenated-strip plane (planar RGB layout).
        let full = self.read_concatenated_plane(plane_index)?;
        let meta = self.meta.as_ref().unwrap();
        Self::crop_planar_plane(meta, &full, x, y, w, h)
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

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        let meta = self.metadata();
        if std::ptr::eq(meta, crate::common::reader::uninitialized_metadata()) {
            return None;
        }
        let mut ome = crate::common::ome_metadata::OmeMetadata::from_image_metadata(meta);
        if let Some(image) = ome.images.first_mut() {
            // Java sets the image name (makeImageName, "" for a standalone STK)
            // and the physical pixel sizes from the UIC calibration tags.
            if let Some(name) = &self.image_name {
                image.name = Some(name.clone());
            }
            if image.physical_size_x.is_none() {
                image.physical_size_x = self.phys_x;
            }
            if image.physical_size_y.is_none() {
                image.physical_size_y = self.phys_y;
            }
            if image.physical_size_z.is_none() {
                image.physical_size_z = self.phys_z;
            }
            // Java sets DetectorSettings binning (and gain) per channel from the
            // UIC1 CameraBin field / comment Gain (store.setDetectorSettingsBinning
            // / setDetectorSettingsGain). The generic OME builder only surfaces
            // channel name/wavelengths from series_metadata, so apply binning/gain
            // here directly.
            for ch in image.channels.iter_mut() {
                if ch.detector_settings_binning.is_none() {
                    if let Some(binning) = &self.data.binning {
                        ch.detector_settings_binning = Some(binning.clone());
                    }
                }
            }
        }
        Some(ome)
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
        // Java MetamorphReader.java:751-754 computes imageCount = sizeZ*sizeC*sizeT
        // *before* `if (isRGB) sizeC *= 3` — RGB samples are interleaved within a
        // single plane, so they must not be folded into the plane count.
        let image_count = size_z * size_c * size_t;

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

        // Java MetamorphReader.java:776-789 — the differing-Z doubling case
        // (`stks.length > nstages`) gives each series-pair its own sizeC/sizeZ
        // derived from the actual file counts, then recomputes imageCount.
        if series_count as i32 > grid.nstages && grid.stks.len() > 1 {
            // hasZ.size() > 1 && hasZ.get(1) && base sizeZ == 1 ? zc : 1
            let midx_size_z = if grid.wave_do_z.len() > 1 && grid.wave_do_z[1] && grid.size_z == 1 {
                grid.zc.max(1) as u32
            } else {
                1u32
            };
            for j in 0..grid.stages_count.max(1) as usize {
                let pidx = j * 2;
                let idx = j * 2 + 1;
                if idx >= metas.len() {
                    break;
                }
                let p_files = grid.stks[pidx].len() as u32;
                let m_files = grid.stks[idx].len() as u32;
                let p_size_t = metas[pidx].size_t.max(1);
                let m_size_t = metas[idx].size_t.max(1);

                let p_size_c = (p_files / p_size_t).max(1);
                metas[pidx].size_c = p_size_c;
                metas[pidx].image_count = p_size_c * p_size_t * metas[pidx].size_z.max(1);

                let m_size_c = (m_files / m_size_t).max(1);
                metas[idx].size_c = m_size_c;
                metas[idx].size_z = midx_size_z;
                metas[idx].image_count = m_size_c * m_size_t * midx_size_z;
            }
        }

        self.bizarre_multichannel = grid.bizarre_multichannel;
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
            // Parse the per-file UIC1/UIC4 data fields (image name/date, binning,
            // stage positions, absolute Z, stage labels, emission wavelengths),
            // mirroring Java parseUIC1Tags / parseUIC4Tags. Surface them both as
            // Java's named series-metadata keys and as the generic OME keys the
            // OmeMetadata builder consumes (plane positions/exposure, channel
            // emission/detector settings).
            let mut mm = read_metamorph_data(path, &ifd, mm_planes);
            emit_metamorph_data_metadata(&mm, mm_planes, image_count, size_c, &mut meta_map);
            self.data = std::mem::take(&mut mm);
        }
        if let Some(n) = uic_planes {
            meta_map.insert("uic_plane_count".into(), MetadataValue::Int(n as i64));
        }

        let meta = ImageMetadata {
            size_z,
            size_c,
            size_t,
            image_count,
            // Single-file STK keeps the MinimalTiffReader default order
            // (XYCZT). Only the multi-file .nd path uses XYZCT (Java
            // MetamorphReader.java:755 vs the inherited TIFF default).
            dimension_order: DimensionOrder::XYCZT,
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
        let size_c = meta.size_c.max(1);
        let bizarre = self.bizarre_multichannel;
        let row = stks.get(series).ok_or(BioFormatsError::NotInitialized)?;
        if row.is_empty() {
            return Ok(None);
        }
        // ndx = no / sizeZ; plane within the file = no % sizeZ (Java openBytes).
        // For XYZCT, no = z + sizeZ*(c + sizeC*t), so no/sizeZ = c + sizeC*t.
        let (mut ndx, plane) = if row.len() == 1 {
            (0usize, plane_index)
        } else {
            ((plane_index / size_z) as usize, plane_index % size_z)
        };

        // bizarreMultichannelAcquisition ("Both lasers" / DUAL): the two
        // channels are stored side by side in a single file at channel 0.
        // Java MetamorphReader.java:303-305 forces channel 0 in the file index
        // (ndx = getIndex(z, 0, t) / sizeZ); :322-324 crops the correct half.
        let mut channel = 0u32;
        if bizarre && row.len() != 1 {
            let rem = plane_index / size_z;
            channel = rem % size_c;
            let t = rem / size_c;
            // getIndex(z, 0, t) = z + sizeZ*sizeC*t; / sizeZ = sizeC*t (z < sizeZ).
            ndx = (size_c * t) as usize;
        }

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
        // Constituent STK reader: do not re-resolve the .nd (Java
        // setCanLookForND(false), MetamorphReader.java:801,811).
        let mut reader = MetamorphReader::new();
        reader.can_look_for_nd = false;
        reader.set_id(&file)?;
        let inner = reader.metadata().image_count.max(1);

        if bizarre {
            // The constituent file is 2*sizeX wide; channel 0 is the left half
            // (x), channel 1 is the right half (x + sizeX). Read the full
            // constituent plane and crop the appropriate sizeX-wide half.
            let crop_w = meta.size_x;
            let crop_h = meta.size_y;
            let real_x = if channel == 0 { 0 } else { crop_w };
            let full = reader.open_bytes(plane % inner)?;
            let full_meta = reader.metadata();
            let half = crop_full_plane(
                "MetaMorph STK",
                &full,
                full_meta,
                1,
                real_x,
                0,
                crop_w,
                crop_h,
            )?;
            return Ok(Some(half));
        }

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
        let grid = build_nd_grid(&nd_path, &info, (1, 1, 1));

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
        let grid = build_nd_grid(&nd_path, &info, (1, 1, 1));
        assert_eq!(grid.stks.len(), 1);
        assert_eq!(grid.stks[0].len(), 1);
        assert_eq!(
            grid.stks[0][0].as_ref().unwrap().file_name().unwrap(),
            "solo.STK"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── UIC1/UIC4 data-field parsing tests (Java parseUIC1Tags/parseUIC4Tags) ──

    fn le_i32(v: i32) -> [u8; 4] {
        v.to_le_bytes()
    }

    fn metadata_f64(metadata: &HashMap<String, MetadataValue>, key: &str) -> Option<f64> {
        match metadata.get(key) {
            Some(MetadataValue::Float(v)) => Some(*v),
            _ => None,
        }
    }

    #[test]
    fn metamorph_parse_uic1_extracts_binning_name_and_date() {
        // Build a synthetic UIC1 (id, valueOrOffset) table plus referenced data
        // at known offsets, mirroring Java parseUIC1Tags fields 7/17/46.
        // Layout: [data region][uic1 table]. The table holds 8-byte (id, off)
        // pairs; offsets point into the data region (absolute file offsets).
        let mut buf = vec![0u8; 0];
        // data region:
        // off 0: CameraBin (field 46): xBin=2, yBin=2
        buf.extend_from_slice(&le_i32(2));
        buf.extend_from_slice(&le_i32(2));
        // off 8: Name (field 7): len-prefixed "CELL"
        let name = b"CELL";
        buf.extend_from_slice(&le_i32(name.len() as i32));
        buf.extend_from_slice(name);
        // off 16: LastSavedTime (field 17): Julian date + ms-of-day
        // Julian 2451544 -> 01/01/2000; time 0 -> 00:00:00:000.
        buf.extend_from_slice(&le_i32(2451544));
        buf.extend_from_slice(&le_i32(0));
        // Now the UIC1 table (id, valueOrOffset pairs).
        let table_off = buf.len();
        let pairs: &[(i32, i32)] = &[(46, 0), (7, 8), (17, 16)];
        for (id, off) in pairs {
            buf.extend_from_slice(&le_i32(*id));
            buf.extend_from_slice(&le_i32(*off));
        }

        let mut out = MetamorphData::default();
        parse_uic1_data_fields(&buf, table_off, true, pairs.len() as u32, 1, &mut out);

        assert_eq!(out.binning.as_deref(), Some("2x2"));
        assert_eq!(out.image_name.as_deref(), Some("CELL"));
        assert_eq!(
            out.image_creation_date.as_deref(),
            Some("01/01/2000 00:00:00:000")
        );
    }

    #[test]
    fn metamorph_read_stage_positions_reads_rational_pairs() {
        // Two planes, each (stageX, stageY) as TIFF rationals (num/den i32s).
        let mut buf = Vec::new();
        // plane 0: x = 10/1, y = 20/1
        for v in [10, 1, 20, 1] {
            buf.extend_from_slice(&le_i32(v));
        }
        // plane 1: x = -5/1, y = 30/2 = 15
        for v in [-5, 1, 30, 2] {
            buf.extend_from_slice(&le_i32(v));
        }
        let (xs, ys) = read_stage_positions(&buf, 0, true, 2);
        assert_eq!(xs, vec![10.0, -5.0]);
        assert_eq!(ys, vec![20.0, 15.0]);
    }

    #[test]
    fn metamorph_emit_metadata_surfaces_plane_positions_and_channel_binning() {
        let mm = MetamorphData {
            binning: Some("2x2".into()),
            image_name: Some("img".into()),
            stage_x: vec![1.0, 2.0],
            stage_y: vec![3.0, 4.0],
            em_wavelength: vec![488.0, 561.0],
            z_distances: vec![0.5, 0.5],
            valid_z: false,
            ..Default::default()
        };
        let mut out = HashMap::new();
        // image_count = 2, size_c = 2.
        emit_metamorph_data_metadata(&mm, 2, 2, 2, &mut out);

        // Plane positions surfaced as generic OME keys.
        assert_eq!(metadata_f64(&out, "plane.0.position_x"), Some(1.0));
        assert_eq!(metadata_f64(&out, "plane.1.position_y"), Some(4.0));
        // Z accumulates from zStart=0: plane0=0, plane1=0+0.5=0.5.
        assert_eq!(metadata_f64(&out, "plane.0.position_z"), Some(0.0));
        assert_eq!(metadata_f64(&out, "plane.1.position_z"), Some(0.5));
        // Emission wavelengths surfaced per channel (>= 1).
        assert_eq!(metadata_f64(&out, "channel.0.emission_wavelength"), Some(488.0));
        assert_eq!(metadata_f64(&out, "channel.1.emission_wavelength"), Some(561.0));
        // Binning surfaced both as Java key and per-channel detector settings.
        assert_eq!(metadata_str(&out, "binning"), Some("2x2"));
        assert_eq!(metadata_str(&out, "Name"), Some("img"));
        assert_eq!(
            metadata_str(&out, "channel.0.detector_settings_binning"),
            Some("2x2")
        );
    }

    /// Integration test against the real `testdata/stk/C0.stk` fixture (skipped
    /// when absent). Verifies the UIC1/UIC4 data fields land in series metadata
    /// and OME.
    #[test]
    fn metamorph_real_stk_captures_uic1_data_fields() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("testdata")
            .join("stk")
            .join("C0.stk");
        if !path.exists() {
            eprintln!("SKIP metamorph_real_stk_captures_uic1_data_fields (no fixture)");
            return;
        }
        let mut reader = MetamorphReader::new();
        reader.set_id(&path).expect("set_id");
        let meta = reader.metadata();
        // Image name from UIC1 field 7 (Java reads the stored byte count, which
        // here includes a trailing NUL, matching DataTools/readString behaviour).
        assert!(metadata_str(&meta.series_metadata, "Name")
            .is_some_and(|n| n.starts_with("CT32_VR_FNK_2012APR20_0005_CY5_C004Z")));
        // Binning from UIC1 field 46 (CameraBin 1x1).
        assert_eq!(metadata_str(&meta.series_metadata, "binning"), Some("1x1"));
        // Creation date from UIC1 field 17 (LastSavedTime).
        assert!(metadata_str(&meta.series_metadata, "imageCreationDate")
            .is_some_and(|d| d.contains("/2012 ")));
        // Stage positions parsed (field 28 present in this file).
        assert!(reader.data.has_stage_positions);
        assert!(!reader.data.stage_x.is_empty());
        // Per-plane position keys surfaced for OME.
        assert!(meta.series_metadata.contains_key("plane.0.position_x"));
        // OME carries the binning as a detector setting.
        let ome = reader.ome_metadata().expect("ome");
        let ch0 = &ome.images[0].channels[0];
        assert_eq!(ch0.detector_settings_binning.as_deref(), Some("1x1"));
    }
}
