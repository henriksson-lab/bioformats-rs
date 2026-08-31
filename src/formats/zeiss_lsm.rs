//! Zeiss LSM format reader (confocal laser scanning microscopy).
//!
//! LSM files are TIFF-based with a proprietary CZ_LSMInfo block (tag 34412).
//! The CZ_LSMInfo block provides the true Z/C/T dimensions.
//! Every other IFD is a thumbnail; only even-indexed IFDs contain full-res data.

use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{
    DimensionOrder, ImageMetadata, LookupTable, MetadataValue, ModuloAnnotation,
};
use crate::common::ome_metadata::{
    create_lsid, OmeDetector, OmeDichroic, OmeFilter, OmeInstrument, OmeLightPath, OmeLightSource,
    OmeObjective, OmePlane,
};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::tiff::ifd::{tag, Compression, IfdValue};
use crate::tiff::parser::TiffParser;
use crate::tiff::TiffReader;

// ── Tag IDs ───────────────────────────────────────────────────────────────────
const CZ_LSM_INFO: u16 = 34412;

// ── Scan-information sub-block constants ─────────────────────────────────────
const TYPE_SUBBLOCK: i32 = 0;
const TYPE_ASCII: i32 = 2;
const TYPE_LONG: i32 = 4;
const TYPE_RATIONAL: i32 = 5;

const SUBBLOCK_RECORDING: i32 = 0x1000_0000;
const SUBBLOCK_LASER: i32 = 0x5000_0000;
const SUBBLOCK_TRACK: i32 = 0x4000_0000;
const SUBBLOCK_DETECTION_CHANNEL: i32 = 0x7000_0000;
const SUBBLOCK_ILLUMINATION_CHANNEL: i32 = 0x9000_0000u32 as i32;
const SUBBLOCK_BEAM_SPLITTER: i32 = 0xb000_0000u32 as i32;
const SUBBLOCK_DATA_CHANNEL: i32 = 0xd000_0000u32 as i32;
#[cfg(test)]
const SUBBLOCK_END: i32 = -1;

const RECORDING_NAME: i32 = 0x1000_0001;
const RECORDING_DESCRIPTION: i32 = 0x1000_0002;
const RECORDING_OBJECTIVE: i32 = 0x1000_0004;
const RECORDING_ZOOM: i32 = 0x1000_0016;
const RECORDING_SAMPLE_0TIME: i32 = 0x1000_0036;
const RECORDING_USER: i32 = 0x1000_0047;
const RECORDING_CAMERA_BINNING: i32 = 0x1000_0052;

const TRACK_ACQUIRE: i32 = 0x4000_0006;
const TRACK_TIME_BETWEEN_STACKS: i32 = 0x4000_000b;

const LASER_NAME: i32 = 0x5000_0001;
const LASER_ACQUIRE: i32 = 0x5000_0002;
const LASER_POWER: i32 = 0x5000_0003;

const CHANNEL_DETECTOR_GAIN: i32 = 0x7000_0003;
const CHANNEL_AMPLIFIER_GAIN: i32 = 0x7000_0005;
const CHANNEL_PINHOLE_DIAMETER: i32 = 0x7000_0009;
const CHANNEL_ACQUIRE: i32 = 0x7000_000b;
const CHANNEL_FILTER_SET: i32 = 0x7000_000f;
const CHANNEL_FILTER: i32 = 0x7000_0010;
const CHANNEL_NAME: i32 = 0x7000_0014;

const ILLUM_CHANNEL_NAME: i32 = 0x9000_0001u32 as i32;
const ILLUM_CHANNEL_ATTENUATION: i32 = 0x9000_0002u32 as i32;
const ILLUM_CHANNEL_WAVELENGTH: i32 = 0x9000_0003u32 as i32;
const ILLUM_CHANNEL_ACQUIRE: i32 = 0x9000_0004u32 as i32;

const BEAM_SPLITTER_FILTER: i32 = 0xb000_0002u32 as i32;
const BEAM_SPLITTER_FILTER_SET: i32 = 0xb000_0003u32 as i32;
const DATA_CHANNEL_NAME: i32 = 0xd000_0001u32 as i32;
const DATA_CHANNEL_ACQUIRE: i32 = 0xd000_0017u32 as i32;

// ── CZ_LSMInfo block (partial) ────────────────────────────────────────────────
// Only the fields we actually need:
//   offset 0:  MagicNumber (int32) = 0x00300494
//   offset 4:  StructureSize (int32)
//   offset 8:  DimensionX (int32)
//   offset 12: DimensionY (int32)
//   offset 16: DimensionZ (int32)
//   offset 20: DimensionChannels (int32)
//   offset 24: DimensionTime (int32)
//   offset 28: DataType (int32) -> 1=uint8, 2=uint12, 5=float32
//   offset 32: ThumbnailX (int32)
//   offset 36: ThumbnailY (int32)
//   offset 40: VoxelSizeX (float64)
//   offset 48: VoxelSizeY (float64)
//   offset 56: VoxelSizeZ (float64)
// Known CZ_LSMInfo magic numbers. ZeissLSMReader.java does not gate on the
// magic value at all (it only records it as metadata), so we accept both
// documented variants and do not hard-fail on others.
const LSM_MAGIC: u32 = 0x0030_0494;
const LSM_MAGIC_ALT: u32 = 0x0040_0494;

#[derive(Debug, Default)]
struct LsmInfo {
    dim_z: u32,
    dim_c: u32,
    dim_t: u32,
    data_type: i32,
    /// CZ-LSMINFO ScanType (short at offset 88); selects the dimension order.
    scan_type: i16,
    voxel_x: f64,
    voxel_y: f64,
    voxel_z: f64,
    /// CZ-LSMINFO OffsetChannelColors (int at offset 108): absolute file offset
    /// of the channel-colours/-names sub-block, or 0 when absent.
    channel_colors_offset: u32,
    /// CZ-LSMINFO overlay offsets in Java's VectorOverlay, InputLut, OutputLut,
    /// ROI, BleachROI, MeanOfRoisOverlay, TopoIsolineOverlay,
    /// TopoProfileOverlay, LinescanOverlay order.
    #[allow(dead_code)]
    overlay_offsets: [u32; 9],
    /// CZ-LSMINFO TimeInterval (double at offset 112); seconds between frames.
    time_interval: f64,
    /// CZ-LSMINFO OffsetTimeStamps (int at offset 132).
    timestamp_offset: u32,
    /// CZ-LSMINFO OffsetScanInformation (int at offset 124).
    scan_information_offset: u32,
    /// CZ-LSMINFO OffsetApplicationTags/OffsetEventList/OffsetChannelWavelength.
    application_tag_offset: u32,
    #[allow(dead_code)]
    event_list_offset: u32,
    #[allow(dead_code)]
    channel_wavelength_offset: u32,
    /// Origin values from CZ-LSMINFO, converted from meters to micrometers.
    origin_um: [f64; 3],
    /// CZ-LSMINFO OffsetTilePositions (int at offset 336).
    tile_position_offset: u32,
    /// CZ-LSMINFO OffsetPositions (int at offset 376).
    position_offset: u32,
    /// CZ-LSMINFO Rotations/Phases/Illuminations counters (ints at offsets
    /// 272/276/280). Java projects these to ModuloAlongZ/T/C metadata.
    rotations: u32,
    phases: u32,
    illuminations: u32,
    /// CZ-LSMINFO DimensionP/DimensionM (ints at offsets 264/268). Java uses
    /// their product to split one physical LSM into multiple logical series.
    dimension_p: u32,
    dimension_m: u32,
    /// Per-timepoint timestamps in seconds, read from OffsetTimeStamps.
    timestamps: Vec<f64>,
    /// Per-position coordinates in reference-frame units, matching Java's OME
    /// Plane PositionX/Y/Z projection for single-file LSM series.
    positions: Vec<[f64; 3]>,
    /// Per-channel names parsed from the channel-colours sub-block (Java
    /// ZeissLSMReader.java:1162-1181).
    channel_names: Vec<String>,
    /// Per-channel display colors parsed from the channel-colours sub-block
    /// (Java ZeissLSMReader.java:1125-1159).
    channel_colors: Vec<[u8; 3]>,
    /// Java `parseApplicationTags` sets this when application entries start
    /// with SimOut/SimPar, changing black channel-colour fallback for SIM data.
    is_sim: bool,
    scan_info: LsmScanInfo,
}

#[derive(Debug, Default, Clone)]
struct LsmScanInfo {
    recordings: Vec<LsmRecording>,
    lasers: Vec<LsmLaser>,
    tracks: Vec<LsmTrack>,
    detection_channels: Vec<LsmDetectionChannel>,
    illumination_channels: Vec<LsmIlluminationChannel>,
    beam_splitters: Vec<LsmBeamSplitter>,
    data_channels: Vec<LsmDataChannel>,
    block_order: Vec<LsmScanBlockRef>,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
enum LsmScanBlockRef {
    Recording(usize),
    Laser(usize),
    Track(usize),
    DetectionChannel(usize),
    IlluminationChannel(usize),
    BeamSplitter(usize),
    DataChannel(usize),
}

#[derive(Debug, Default, Clone)]
struct LsmRecording {
    acquire: bool,
    name: Option<String>,
    description: Option<String>,
    binning: Option<String>,
    start_time: Option<String>,
    user_name: Option<String>,
    objective_model: Option<String>,
    correction: Option<String>,
    immersion: Option<String>,
    magnification: Option<f64>,
    lens_na: Option<f64>,
    zoom: Option<f64>,
}

#[derive(Debug, Default, Clone)]
struct LsmLaser {
    acquire: bool,
    model: Option<String>,
    laser_type: Option<String>,
    medium: Option<String>,
    power: Option<f64>,
}

#[derive(Debug, Default, Clone)]
struct LsmTrack {
    acquire: bool,
    time_increment: Option<f64>,
}

#[derive(Debug, Default, Clone)]
struct LsmDetectionChannel {
    acquire: bool,
    channel_name: Option<String>,
    pinhole: Option<f64>,
    gain: Option<f64>,
    amplification_gain: Option<f64>,
    filter: Option<String>,
    filter_set: Option<String>,
}

#[derive(Debug, Default, Clone)]
struct LsmIlluminationChannel {
    acquire: bool,
    name: Option<String>,
    wavelength: Option<f64>,
    attenuation: Option<f64>,
}

#[derive(Debug, Default, Clone)]
struct LsmBeamSplitter {
    filter: Option<String>,
    filter_set: Option<String>,
}

#[derive(Debug, Default, Clone)]
struct LsmDataChannel {
    acquire: bool,
    name: Option<String>,
}

fn checked_plane_count(size_z: u32, size_c: u32, size_t: u32) -> Result<u32> {
    size_z
        .checked_mul(size_c)
        .and_then(|v| v.checked_mul(size_t))
        .ok_or_else(|| BioFormatsError::Format("LSM: plane count overflow".into()))
}

#[cfg(test)]
fn resolve_lsm_plane_index(
    plane_index: u32,
    logical_count: u32,
    physical_count: u32,
) -> Result<u32> {
    if plane_index >= logical_count || plane_index >= physical_count {
        return Err(BioFormatsError::PlaneOutOfRange(plane_index));
    }
    Ok(plane_index)
}

fn read_i32_lsm(buf: &[u8], off: usize, le: bool) -> i32 {
    let b = [buf[off], buf[off + 1], buf[off + 2], buf[off + 3]];
    if le {
        i32::from_le_bytes(b)
    } else {
        i32::from_be_bytes(b)
    }
}
fn read_i16_lsm(buf: &[u8], off: usize, le: bool) -> i16 {
    let b = [buf[off], buf[off + 1]];
    if le {
        i16::from_le_bytes(b)
    } else {
        i16::from_be_bytes(b)
    }
}
fn read_f64_lsm(buf: &[u8], off: usize, le: bool) -> f64 {
    let b: [u8; 8] = buf[off..off + 8].try_into().unwrap_or([0u8; 8]);
    if le {
        f64::from_le_bytes(b)
    } else {
        f64::from_be_bytes(b)
    }
}

fn positive_f64(v: f64) -> Option<f64> {
    (v > 0.0 && v.is_finite()).then_some(v)
}

fn non_empty_string(v: Option<String>) -> Option<String> {
    v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn parse_lsm_info(bytes: &[u8], le: bool) -> Result<LsmInfo> {
    if bytes.len() < 64 {
        return Err(BioFormatsError::Format(
            "LSM: CZ_LSMInfo block is shorter than 64 bytes".into(),
        ));
    }
    // ZeissLSMReader.java never rejects based on the magic number; it only
    // records it. We mirror that: accept the documented magics (0x00300494 and
    // 0x00400494) and, for any other value, only emit a debug-level note rather
    // than failing to parse the block.
    let magic = read_i32_lsm(bytes, 0, le) as u32;
    if magic != LSM_MAGIC && magic != LSM_MAGIC_ALT {
        // Not a hard error: continue parsing dimensions like Java does.
    }

    let dim_z = read_i32_lsm(bytes, 16, le);
    // ZeissLSMReader.java:773-777 reads sizeZ (offset 16), SKIPS the channel
    // field (offset 20), then reads sizeT (offset 24). sizeC is taken from the
    // TIFF, not from this struct, so the offset-20 channel count is read here
    // only for the validity check (Java does not use it for sizeC).
    let dim_c = read_i32_lsm(bytes, 20, le);
    let dim_t = read_i32_lsm(bytes, 24, le);
    if dim_z <= 0 || dim_c <= 0 || dim_t <= 0 {
        return Err(BioFormatsError::Format(format!(
            "LSM: invalid non-positive dimensions Z={dim_z} C={dim_c} T={dim_t}"
        )));
    }

    Ok(LsmInfo {
        dim_z: dim_z as u32,
        dim_c: dim_c as u32,
        dim_t: dim_t as u32,
        data_type: read_i32_lsm(bytes, 28, le),
        // ZeissLSMReader.java:822-824 seeks to offset 88 and reads a short for
        // ScanType. Missing/short blocks fall back to 0 (-> XYZCT), matching the
        // Java default case.
        scan_type: if bytes.len() >= 90 {
            read_i16_lsm(bytes, 88, le)
        } else {
            0
        },
        voxel_x: if bytes.len() >= 48 {
            read_f64_lsm(bytes, 40, le)
        } else {
            0.0
        },
        voxel_y: if bytes.len() >= 56 {
            read_f64_lsm(bytes, 48, le)
        } else {
            0.0
        },
        voxel_z: if bytes.len() >= 64 {
            read_f64_lsm(bytes, 56, le)
        } else {
            0.0
        },
        // ZeissLSMReader.java:952 reads OffsetChannelColors and java:954 reads
        // TimeInterval. After seek(88) the field order is: scanType(2),
        // spectralScan(2), type(4), overlay[0..2](12) -> offset 108 holds
        // channelColorsOffset, offset 112 holds TimeInterval(double).
        channel_colors_offset: if bytes.len() >= 112 {
            read_i32_lsm(bytes, 108, le) as u32
        } else {
            0
        },
        overlay_offsets: [
            if bytes.len() >= 100 {
                read_i32_lsm(bytes, 96, le).max(0) as u32
            } else {
                0
            },
            if bytes.len() >= 104 {
                read_i32_lsm(bytes, 100, le).max(0) as u32
            } else {
                0
            },
            if bytes.len() >= 108 {
                read_i32_lsm(bytes, 104, le).max(0) as u32
            } else {
                0
            },
            if bytes.len() >= 144 {
                read_i32_lsm(bytes, 140, le).max(0) as u32
            } else {
                0
            },
            if bytes.len() >= 148 {
                read_i32_lsm(bytes, 144, le).max(0) as u32
            } else {
                0
            },
            if bytes.len() >= 188 {
                read_i32_lsm(bytes, 184, le).max(0) as u32
            } else {
                0
            },
            if bytes.len() >= 192 {
                read_i32_lsm(bytes, 188, le).max(0) as u32
            } else {
                0
            },
            if bytes.len() >= 196 {
                read_i32_lsm(bytes, 192, le).max(0) as u32
            } else {
                0
            },
            if bytes.len() >= 200 {
                read_i32_lsm(bytes, 196, le).max(0) as u32
            } else {
                0
            },
        ],
        time_interval: if bytes.len() >= 120 {
            read_f64_lsm(bytes, 112, le)
        } else {
            0.0
        },
        timestamp_offset: if bytes.len() >= 136 {
            read_i32_lsm(bytes, 132, le).max(0) as u32
        } else {
            0
        },
        scan_information_offset: if bytes.len() >= 128 {
            read_i32_lsm(bytes, 124, le).max(0) as u32
        } else {
            0
        },
        application_tag_offset: if bytes.len() >= 132 {
            read_i32_lsm(bytes, 128, le).max(0) as u32
        } else {
            0
        },
        event_list_offset: if bytes.len() >= 140 {
            read_i32_lsm(bytes, 136, le).max(0) as u32
        } else {
            0
        },
        channel_wavelength_offset: if bytes.len() >= 208 {
            read_i32_lsm(bytes, 204, le).max(0) as u32
        } else {
            0
        },
        origin_um: [
            if bytes.len() >= 72 {
                read_f64_lsm(bytes, 64, le) * 1e6
            } else {
                0.0
            },
            if bytes.len() >= 80 {
                read_f64_lsm(bytes, 72, le) * 1e6
            } else {
                0.0
            },
            if bytes.len() >= 88 {
                read_f64_lsm(bytes, 80, le) * 1e6
            } else {
                0.0
            },
        ],
        tile_position_offset: if bytes.len() >= 340 {
            read_i32_lsm(bytes, 336, le).max(0) as u32
        } else {
            0
        },
        position_offset: if bytes.len() >= 380 {
            read_i32_lsm(bytes, 376, le).max(0) as u32
        } else {
            0
        },
        rotations: if bytes.len() >= 276 {
            read_i32_lsm(bytes, 272, le).max(0) as u32
        } else {
            0
        },
        phases: if bytes.len() >= 280 {
            read_i32_lsm(bytes, 276, le).max(0) as u32
        } else {
            0
        },
        illuminations: if bytes.len() >= 284 {
            read_i32_lsm(bytes, 280, le).max(0) as u32
        } else {
            0
        },
        dimension_p: if bytes.len() >= 268 {
            read_i32_lsm(bytes, 264, le).max(0) as u32
        } else {
            0
        },
        dimension_m: if bytes.len() >= 272 {
            read_i32_lsm(bytes, 268, le).max(0) as u32
        } else {
            0
        },
        timestamps: Vec::new(),
        positions: Vec::new(),
        channel_names: Vec::new(),
        channel_colors: Vec::new(),
        is_sim: false,
        scan_info: LsmScanInfo::default(),
    })
}

#[derive(Debug, Clone)]
enum LsmScanValue {
    Int(i32),
    Float(f64),
    Text(String),
}

fn parse_lsm_scan_entry_value(
    file_bytes: &[u8],
    p: &mut usize,
    le: bool,
) -> Option<(i32, LsmScanValue)> {
    if *p + 12 > file_bytes.len() {
        return None;
    }
    let entry = read_i32_lsm(file_bytes, *p, le);
    let block_type = read_i32_lsm(file_bytes, *p + 4, le);
    let data_size = read_i32_lsm(file_bytes, *p + 8, le);
    *p += 12;

    match block_type {
        TYPE_LONG => {
            if *p + 4 > file_bytes.len() {
                return None;
            }
            let value = read_i32_lsm(file_bytes, *p, le);
            *p += 4;
            Some((entry, LsmScanValue::Int(value)))
        }
        TYPE_RATIONAL => {
            if *p + 8 > file_bytes.len() {
                return None;
            }
            let value = read_f64_lsm(file_bytes, *p, le);
            *p += 8;
            Some((entry, LsmScanValue::Float(value)))
        }
        TYPE_ASCII => {
            if data_size < 0 {
                return None;
            }
            let len = data_size as usize;
            if *p + len > file_bytes.len() {
                return None;
            }
            let raw = &file_bytes[*p..*p + len];
            *p += len;
            let stop = raw.iter().position(|&b| b < 10).unwrap_or(raw.len());
            let value = String::from_utf8_lossy(&raw[..stop]).trim().to_string();
            Some((entry, LsmScanValue::Text(value)))
        }
        TYPE_SUBBLOCK => None,
        _ => {
            if data_size < 0 {
                return None;
            }
            *p = (*p)
                .saturating_add(data_size as usize)
                .min(file_bytes.len());
            Some((entry, LsmScanValue::Text(String::new())))
        }
    }
}

fn parse_lsm_scan_block_data(
    file_bytes: &[u8],
    p: &mut usize,
    le: bool,
) -> HashMap<i32, LsmScanValue> {
    let mut block_data = HashMap::new();
    while *p + 12 <= file_bytes.len() {
        let entry_start = *p;
        let block_type = read_i32_lsm(file_bytes, entry_start + 4, le);
        if block_type == TYPE_SUBBLOCK {
            break;
        }
        if let Some((entry, value)) = parse_lsm_scan_entry_value(file_bytes, p, le) {
            block_data.entry(entry).or_insert(value);
        } else {
            break;
        }
    }
    block_data
}

fn scan_int(data: &HashMap<i32, LsmScanValue>, key: i32) -> Option<i32> {
    match data.get(&key) {
        Some(LsmScanValue::Int(v)) => Some(*v),
        Some(LsmScanValue::Float(v)) => Some(*v as i32),
        _ => None,
    }
}

fn scan_float(data: &HashMap<i32, LsmScanValue>, key: i32) -> Option<f64> {
    match data.get(&key) {
        Some(LsmScanValue::Float(v)) => Some(*v),
        Some(LsmScanValue::Int(v)) => Some(*v as f64),
        _ => None,
    }
}

fn scan_text(data: &HashMap<i32, LsmScanValue>, key: i32) -> Option<String> {
    match data.get(&key) {
        Some(LsmScanValue::Text(v)) => Some(v.clone()),
        Some(LsmScanValue::Int(v)) => Some(v.to_string()),
        Some(LsmScanValue::Float(v)) => Some(v.to_string()),
        _ => None,
    }
}

fn scan_acquire(data: &HashMap<i32, LsmScanValue>, key: i32) -> bool {
    scan_int(data, key).is_none_or(|v| v != 0)
}

fn parse_lsm_recording(data: &HashMap<i32, LsmScanValue>) -> LsmRecording {
    let mut binning = non_empty_string(scan_text(data, RECORDING_CAMERA_BINNING));
    if let Some(b) = &binning {
        if !b.contains('x') {
            binning = (b != "0").then(|| format!("{b}x{b}"));
        }
    }

    let objective_model = non_empty_string(scan_text(data, RECORDING_OBJECTIVE));
    let (correction, magnification, lens_na, immersion) =
        parse_lsm_objective(objective_model.as_deref().unwrap_or(""));

    LsmRecording {
        acquire: true,
        name: non_empty_string(scan_text(data, RECORDING_NAME)),
        description: non_empty_string(scan_text(data, RECORDING_DESCRIPTION)),
        binning,
        start_time: scan_float(data, RECORDING_SAMPLE_0TIME)
            .filter(|v| *v > 0.0)
            .and_then(microsoft_days_to_iso8601),
        user_name: non_empty_string(scan_text(data, RECORDING_USER)),
        objective_model,
        correction,
        immersion,
        magnification,
        lens_na,
        zoom: positive_f64(scan_float(data, RECORDING_ZOOM).unwrap_or(-1.0)),
    }
}

fn parse_lsm_objective(
    objective: &str,
) -> (Option<String>, Option<f64>, Option<f64>, Option<String>) {
    let tokens: Vec<&str> = objective.split_whitespace().collect();
    let mut correction_parts = Vec::new();
    let mut next = 0usize;
    while next < tokens.len() && !tokens[next].contains('/') {
        correction_parts.push(tokens[next]);
        next += 1;
    }
    let correction = (!correction_parts.is_empty()).then(|| correction_parts.join(""));
    let mut magnification = None;
    let mut lens_na = None;
    if next < tokens.len() {
        let p = tokens[next];
        next += 1;
        if let Some(slash) = p.find('/') {
            if slash > 0 {
                magnification = p[..slash].trim_end_matches('x').parse::<f64>().ok();
            }
            if slash + 1 < p.len() {
                lens_na = p[slash + 1..].parse::<f64>().ok();
            }
        }
    }
    let immersion = tokens.get(next).map(|s| (*s).to_string());
    (correction, magnification, lens_na, immersion)
}

fn parse_lsm_laser(data: &HashMap<i32, LsmScanValue>) -> LsmLaser {
    let model = non_empty_string(scan_text(data, LASER_NAME));
    let mut laser_type = model.clone().unwrap_or_default();
    let mut medium = String::new();
    if laser_type.starts_with("HeNe") {
        medium = "HeNe".into();
        laser_type = "Gas".into();
    } else if laser_type.starts_with("Argon") {
        medium = "Ar".into();
        laser_type = "Gas".into();
    } else if laser_type == "Titanium:Sapphire" || laser_type == "Mai Tai" {
        medium = "TiSapphire".into();
        laser_type = "SolidState".into();
    } else if laser_type == "YAG" {
        laser_type = "SolidState".into();
    } else if laser_type == "Ar/Kr" {
        laser_type = "Gas".into();
    }

    LsmLaser {
        acquire: scan_acquire(data, LASER_ACQUIRE),
        model,
        laser_type: non_empty_string(Some(laser_type)),
        medium: non_empty_string(Some(medium)),
        power: positive_f64(scan_float(data, LASER_POWER).unwrap_or(-1.0)),
    }
}

fn parse_lsm_detection_channel(data: &HashMap<i32, LsmScanValue>) -> LsmDetectionChannel {
    let filter = non_empty_string(scan_text(data, CHANNEL_FILTER)).filter(|v| v != "None");
    LsmDetectionChannel {
        acquire: scan_acquire(data, CHANNEL_ACQUIRE),
        channel_name: non_empty_string(scan_text(data, CHANNEL_NAME)),
        pinhole: positive_f64(scan_float(data, CHANNEL_PINHOLE_DIAMETER).unwrap_or(-1.0)),
        gain: positive_f64(scan_float(data, CHANNEL_DETECTOR_GAIN).unwrap_or(-1.0)),
        amplification_gain: positive_f64(scan_float(data, CHANNEL_AMPLIFIER_GAIN).unwrap_or(-1.0)),
        filter,
        filter_set: non_empty_string(scan_text(data, CHANNEL_FILTER_SET)),
    }
}

fn parse_lsm_illumination_channel(data: &HashMap<i32, LsmScanValue>) -> LsmIlluminationChannel {
    let name = non_empty_string(scan_text(data, ILLUM_CHANNEL_NAME));
    let wavelength = name
        .as_deref()
        .and_then(|s| s.parse::<f64>().ok())
        .or_else(|| scan_float(data, ILLUM_CHANNEL_WAVELENGTH))
        .and_then(positive_f64);
    LsmIlluminationChannel {
        acquire: scan_acquire(data, ILLUM_CHANNEL_ACQUIRE),
        name,
        wavelength,
        attenuation: positive_f64(scan_float(data, ILLUM_CHANNEL_ATTENUATION).unwrap_or(-1.0)),
    }
}

fn parse_lsm_beam_splitter(data: &HashMap<i32, LsmScanValue>) -> LsmBeamSplitter {
    LsmBeamSplitter {
        filter: non_empty_string(scan_text(data, BEAM_SPLITTER_FILTER)).filter(|v| v != "None"),
        filter_set: non_empty_string(scan_text(data, BEAM_SPLITTER_FILTER_SET)),
    }
}

fn parse_lsm_data_channel(data: &HashMap<i32, LsmScanValue>) -> LsmDataChannel {
    let name = non_empty_string(scan_text(data, DATA_CHANNEL_NAME).map(|s| {
        let stop = s.as_bytes().iter().position(|&b| b < 10).unwrap_or(s.len());
        s[..stop].to_string()
    }));
    LsmDataChannel {
        acquire: scan_acquire(data, DATA_CHANNEL_ACQUIRE),
        name,
    }
}

fn parse_lsm_scan_info(file_bytes: &[u8], scan_information_offset: u32, le: bool) -> LsmScanInfo {
    let mut scan_info = LsmScanInfo::default();
    let mut p = scan_information_offset as usize;
    while p + 12 <= file_bytes.len() {
        let entry = read_i32_lsm(file_bytes, p, le);
        let block_type = read_i32_lsm(file_bytes, p + 4, le);
        let data_size = read_i32_lsm(file_bytes, p + 8, le);
        p += 12;
        if block_type == TYPE_SUBBLOCK {
            let data = parse_lsm_scan_block_data(file_bytes, &mut p, le);
            match entry {
                SUBBLOCK_RECORDING => {
                    let idx = scan_info.recordings.len();
                    scan_info.recordings.push(parse_lsm_recording(&data));
                    scan_info.block_order.push(LsmScanBlockRef::Recording(idx));
                }
                SUBBLOCK_LASER => {
                    let idx = scan_info.lasers.len();
                    scan_info.lasers.push(parse_lsm_laser(&data));
                    scan_info.block_order.push(LsmScanBlockRef::Laser(idx));
                }
                SUBBLOCK_TRACK => {
                    let idx = scan_info.tracks.len();
                    scan_info.tracks.push(LsmTrack {
                        acquire: scan_acquire(&data, TRACK_ACQUIRE),
                        time_increment: positive_f64(
                            scan_float(&data, TRACK_TIME_BETWEEN_STACKS).unwrap_or(-1.0),
                        ),
                    });
                    scan_info.block_order.push(LsmScanBlockRef::Track(idx));
                }
                SUBBLOCK_DETECTION_CHANNEL => {
                    let idx = scan_info.detection_channels.len();
                    scan_info
                        .detection_channels
                        .push(parse_lsm_detection_channel(&data));
                    scan_info
                        .block_order
                        .push(LsmScanBlockRef::DetectionChannel(idx));
                }
                SUBBLOCK_ILLUMINATION_CHANNEL => {
                    let idx = scan_info.illumination_channels.len();
                    scan_info
                        .illumination_channels
                        .push(parse_lsm_illumination_channel(&data));
                    scan_info
                        .block_order
                        .push(LsmScanBlockRef::IlluminationChannel(idx));
                }
                SUBBLOCK_BEAM_SPLITTER => {
                    let idx = scan_info.beam_splitters.len();
                    scan_info
                        .beam_splitters
                        .push(parse_lsm_beam_splitter(&data));
                    scan_info
                        .block_order
                        .push(LsmScanBlockRef::BeamSplitter(idx));
                }
                SUBBLOCK_DATA_CHANNEL => {
                    let idx = scan_info.data_channels.len();
                    scan_info.data_channels.push(parse_lsm_data_channel(&data));
                    scan_info
                        .block_order
                        .push(LsmScanBlockRef::DataChannel(idx));
                }
                _ => {}
            }
        } else if data_size > 0 {
            p = p.saturating_add(data_size as usize).min(file_bytes.len());
        } else {
            break;
        }
    }
    normalize_lsm_scan_info_like_java(&mut scan_info);
    scan_info
}

fn lsm_scan_block_acquired(scan_info: &LsmScanInfo, block: LsmScanBlockRef) -> bool {
    match block {
        LsmScanBlockRef::Recording(i) => scan_info.recordings.get(i).is_some_and(|v| v.acquire),
        LsmScanBlockRef::Laser(i) => scan_info.lasers.get(i).is_some_and(|v| v.acquire),
        LsmScanBlockRef::Track(i) => scan_info.tracks.get(i).is_some_and(|v| v.acquire),
        LsmScanBlockRef::DetectionChannel(i) => scan_info
            .detection_channels
            .get(i)
            .is_some_and(|v| v.acquire),
        LsmScanBlockRef::IlluminationChannel(i) => scan_info
            .illumination_channels
            .get(i)
            .is_some_and(|v| v.acquire),
        LsmScanBlockRef::BeamSplitter(_) => true,
        LsmScanBlockRef::DataChannel(i) => {
            scan_info.data_channels.get(i).is_some_and(|v| v.acquire)
        }
    }
}

fn normalize_lsm_scan_info_like_java(scan_info: &mut LsmScanInfo) {
    let acquired_blocks: Vec<LsmScanBlockRef> = scan_info
        .block_order
        .iter()
        .copied()
        .filter(|block| lsm_scan_block_acquired(scan_info, *block))
        .collect();

    for (i, block) in acquired_blocks.iter().copied().enumerate() {
        match block {
            LsmScanBlockRef::IlluminationChannel(idx) => {
                let valid_next = acquired_blocks.get(i + 1).is_some_and(|next| {
                    matches!(
                        next,
                        LsmScanBlockRef::DataChannel(_) | LsmScanBlockRef::IlluminationChannel(_)
                    )
                });
                if !valid_next {
                    if let Some(channel) = scan_info.illumination_channels.get_mut(idx) {
                        channel.wavelength = None;
                    }
                }
            }
            LsmScanBlockRef::DetectionChannel(idx) => {
                let valid_prev = i > 0
                    && acquired_blocks.get(i - 1).is_some_and(|prev| {
                        matches!(
                            prev,
                            LsmScanBlockRef::Track(_) | LsmScanBlockRef::DetectionChannel(_)
                        )
                    });
                if !valid_prev {
                    if let Some(channel) = scan_info.detection_channels.get_mut(idx) {
                        channel.acquire = false;
                    }
                }
            }
            _ => {}
        }
    }
}

fn lsm_scan_blocks_for_java_population(scan_info: &LsmScanInfo) -> Vec<LsmScanBlockRef> {
    let mut acquired = Vec::new();
    let mut non_acquired = Vec::new();
    for block in scan_info.block_order.iter().copied() {
        if lsm_scan_block_acquired(scan_info, block) {
            acquired.push(block);
        } else {
            non_acquired.push(block);
        }
    }
    acquired.extend(non_acquired);
    acquired
}

fn microsoft_days_to_iso8601(days: f64) -> Option<String> {
    let seconds = (days * 86_400.0).round() as i64 - 2_209_161_600;
    unix_seconds_to_iso8601(seconds)
}

fn unix_seconds_to_iso8601(seconds: i64) -> Option<String> {
    let days = seconds.div_euclid(86_400);
    let sod = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = sod / 3_600;
    let minute = (sod % 3_600) / 60;
    let second = sod % 60;
    Some(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}"
    ))
}

fn civil_from_days(days_since_unix_epoch: i64) -> (i32, u32, u32) {
    let z = days_since_unix_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    let year = y + if m <= 2 { 1 } else { 0 };
    (year as i32, m as u32, d as u32)
}

/// Parses the per-channel names from the channel-colours sub-block, mirroring
/// ZeissLSMReader.java:1112-1182. `colors_offset`/`names_offset` are relative to
/// `channel_colors_offset`; the name table is a sequence of (int length, bytes)
/// records with trailing NULs stripped.
fn parse_channel_names(
    file_bytes: &[u8],
    channel_colors_offset: u32,
    size_c: u32,
    le: bool,
) -> Vec<String> {
    let mut names = Vec::new();
    if channel_colors_offset == 0 {
        return names;
    }
    let base = channel_colors_offset as usize;
    // Need at least the two offset ints at base+12 and base+16.
    if base + 20 > file_bytes.len() {
        return names;
    }
    let names_offset = read_i32_lsm(file_bytes, base + 16, le);
    if names_offset <= 0 {
        return names;
    }
    let mut p = base + names_offset as usize;
    for _ in 0..size_c {
        if p + 4 > file_bytes.len() {
            break;
        }
        let length = read_i32_lsm(file_bytes, p, le);
        p += 4;
        if length < 0 {
            break;
        }
        let length = length as usize;
        if p + length > file_bytes.len() {
            break;
        }
        let raw = &file_bytes[p..p + length];
        p += length;
        let trimmed = raw.split(|&b| b == 0).next().unwrap_or(&[]);
        names.push(String::from_utf8_lossy(trimmed).into_owned());
    }
    names
}

/// Parses the per-channel display colours from the channel-colours sub-block.
/// Java reads the stored 32-bit value as red=low byte, green=next byte, and
/// blue=third byte. Black is special-cased to white, except for later SIM
/// channels where Java copies the previous channel colour.
fn parse_channel_colors(
    file_bytes: &[u8],
    channel_colors_offset: u32,
    size_c: u32,
    is_sim: bool,
    le: bool,
) -> Vec<[u8; 3]> {
    let mut colors = Vec::new();
    if channel_colors_offset == 0 {
        return colors;
    }
    let base = channel_colors_offset as usize;
    if base + 16 > file_bytes.len() {
        return colors;
    }
    let colors_offset = read_i32_lsm(file_bytes, base + 12, le);
    if colors_offset <= 0 {
        return colors;
    }
    let mut p = base + colors_offset as usize;
    for i in 0..size_c as usize {
        if p + 4 > file_bytes.len() {
            break;
        }
        let color = read_i32_lsm(file_bytes, p, le) as u32;
        p += 4;
        let mut red = (color & 0xff) as u8;
        let mut green = ((color & 0xff00) >> 8) as u8;
        let mut blue = ((color & 0xff0000) >> 16) as u8;
        if red == 0 && green == 0 && blue == 0 {
            if i > 0 && is_sim {
                [red, green, blue] = colors[i - 1];
            } else {
                red = 255;
                green = 255;
                blue = 255;
            }
        }
        colors.push([red, green, blue]);
    }
    colors
}

fn parse_lsm_timestamps(file_bytes: &[u8], timestamp_offset: u32, le: bool) -> Vec<f64> {
    let base = timestamp_offset as usize;
    if timestamp_offset == 0 || base + 8 > file_bytes.len() {
        return Vec::new();
    }
    // Java seeks to offset + 4 and reads the stamp count, then one double per
    // stamp (ZeissLSMReader.java:1187-1193).
    let n_stamps = read_i32_lsm(file_bytes, base + 4, le);
    if n_stamps <= 0 {
        return Vec::new();
    }
    let mut p = base + 8;
    let available = (file_bytes.len().saturating_sub(p)) / 8;
    let count = (n_stamps as usize).min(available);
    let mut timestamps = Vec::with_capacity(count);
    for _ in 0..count {
        let stamp = read_f64_lsm(file_bytes, p, le);
        if stamp.is_finite() {
            timestamps.push(stamp);
        }
        p += 8;
    }
    timestamps
}

fn parse_lsm_position_table(
    file_bytes: &[u8],
    offset: u32,
    origin_um: [f64; 3],
    le: bool,
) -> Vec<[f64; 3]> {
    let mut positions = Vec::new();
    let mut p = offset as usize;
    if offset == 0 || p + 4 > file_bytes.len() {
        return positions;
    }
    let n_positions = read_i32_lsm(file_bytes, p, le);
    if n_positions <= 0 {
        return positions;
    }
    p += 4;
    let available = (file_bytes.len().saturating_sub(p)) / 24;
    let count = (n_positions as usize).min(available);
    positions.reserve(count);
    for _ in 0..count {
        let x = origin_um[0] + read_f64_lsm(file_bytes, p, le) * 1e6;
        let y = origin_um[1] + read_f64_lsm(file_bytes, p + 8, le) * 1e6;
        let z = origin_um[2] + read_f64_lsm(file_bytes, p + 16, le) * 1e6;
        if x.is_finite() && y.is_finite() && z.is_finite() {
            positions.push([x, y, z]);
        }
        p += 24;
    }
    positions
}

fn parse_lsm_positions(file_bytes: &[u8], info: &LsmInfo, le: bool) -> Vec<[f64; 3]> {
    // Java reads OffsetPositions first, then OffsetTilePositions. Tile entries
    // are added to existing coordinates for the same index, or appended when no
    // base position exists (ZeissLSMReader.java:1050-1098).
    let mut positions =
        parse_lsm_position_table(file_bytes, info.position_offset, info.origin_um, le);
    let tiles = parse_lsm_position_table(file_bytes, info.tile_position_offset, info.origin_um, le);
    for (i, tile) in tiles.into_iter().enumerate() {
        if let Some(position) = positions.get_mut(i) {
            position[0] += tile[0];
            position[1] += tile[1];
            position[2] += tile[2];
        } else if positions.len() == i {
            positions.push(tile);
        }
    }
    positions
}

fn parse_lsm_application_tags_is_sim(file_bytes: &[u8], offset: u32, le: bool) -> bool {
    let mut p = offset as usize;
    if p == 0 || p + 8 > file_bytes.len() {
        return false;
    }

    let _block_size = read_i32_lsm(file_bytes, p, le);
    p += 4;
    let entries = read_i32_lsm(file_bytes, p, le).max(0) as usize;
    p += 4;

    for _ in 0..entries {
        let entry_start = p;
        if p + 16 > file_bytes.len() {
            break;
        }
        let entry_size = read_i32_lsm(file_bytes, p, le);
        p += 4;
        let name_len = read_i32_lsm(file_bytes, p, le);
        p += 4;
        if entry_size <= 0 || name_len < 0 {
            break;
        }
        let name_len = name_len as usize;
        if p + name_len + 8 > file_bytes.len() {
            break;
        }
        let name = String::from_utf8_lossy(&file_bytes[p..p + name_len]);
        if name.starts_with("SimOut") || name.starts_with("SimPar") {
            return true;
        }
        p += name_len;
        let _data_type = read_i32_lsm(file_bytes, p, le);
        p += 4;
        let data_size = read_i32_lsm(file_bytes, p, le);
        p += 4;
        if data_size < 0 {
            break;
        }
        let next_entry = entry_start.saturating_add(entry_size as usize);
        p = next_entry.max(p.saturating_add(data_size as usize));
        if p > file_bytes.len() {
            break;
        }
    }
    false
}

fn lsm_pack_rgba([red, green, blue]: [u8; 3]) -> i32 {
    u32::from_be_bytes([red, green, blue, 0xff]) as i32
}

fn lsm_8bit_lookup_table([red, green, blue]: [u8; 3]) -> LookupTable {
    let ramp = |component: u8| -> Vec<u16> {
        (0..256)
            .map(|p| (((component as f64 / 255.0) * p as f64) as u8) as u16)
            .collect()
    };
    LookupTable {
        red: ramp(red),
        green: ramp(green),
        blue: ramp(blue),
    }
}

fn lsm_16bit_lookup_table([red, green, blue]: [u8; 3]) -> LookupTable {
    let ramp = |component: u8| -> Vec<u16> {
        (0..65536)
            .map(|p| ((component as f64 / 255.0) * p as f64) as u16)
            .collect()
    };
    LookupTable {
        red: ramp(red),
        green: ramp(green),
        blue: ramp(blue),
    }
}

fn lsm_lookup_table_for_pixel_type(color: [u8; 3], pixel_type: PixelType) -> Option<LookupTable> {
    match pixel_type {
        PixelType::Uint8 => Some(lsm_8bit_lookup_table(color)),
        PixelType::Uint16 => Some(lsm_16bit_lookup_table(color)),
        _ => None,
    }
}

fn lsm_modulo(
    parent_dimension: &str,
    modulo_type: &str,
    step: u32,
    count: u32,
) -> Option<ModuloAnnotation> {
    (count > 1).then(|| ModuloAnnotation {
        parent_dimension: parent_dimension.to_string(),
        modulo_type: modulo_type.to_string(),
        start: 0.0,
        step: step as f64,
        end: (step.saturating_mul(count.saturating_sub(1))) as f64,
        unit: String::new(),
        labels: Vec::new(),
    })
}

fn lsm_series_plan(info: &LsmInfo, full_res_ifd_count: u32) -> (u32, u32) {
    let requested_series_count = info
        .dimension_m
        .checked_mul(info.dimension_p)
        .filter(|&count| count > 0)
        .unwrap_or(1);
    let series_count = requested_series_count.min(full_res_ifd_count.max(1));
    let ifds_per_series = (full_res_ifd_count / series_count).max(1);
    (series_count, ifds_per_series)
}

/// Maps the CZ-LSMINFO ScanType to a dimension order, mirroring
/// ZeissLSMReader.java:824-885.
///
/// Base switch (java:825-873):
///   3 / 5 / 9 -> XYTCZ   (time series x-y / Mean of ROIs / time series spline x-z)
///   4 / 6     -> XYZTC   (time series x-z / time series x-y-z)
///   7         -> XYCTZ   (spline scan)
///   8         -> XYCZT   (spline scan x-z)
///   0,1,2,10,default -> XYZCT
///
/// When the image is RGB (java:881-885), C is shuffled to the front: "C" is
/// removed from the order then re-inserted right after "XY", i.e. the result is
/// always "XYC" + the remaining two axes.
fn lsm_dimension_order(scan_type: i16, is_rgb: bool) -> DimensionOrder {
    let base = match scan_type {
        3 | 5 | 9 => DimensionOrder::XYTCZ,
        4 | 6 => DimensionOrder::XYZTC,
        7 => DimensionOrder::XYCTZ,
        8 => DimensionOrder::XYCZT,
        // 0, 1, 2, 10 and any other value -> XYZCT
        _ => DimensionOrder::XYZCT,
    };
    if !is_rgb {
        return base;
    }
    // Shuffle C to the front (after XY), preserving the relative order of the
    // remaining Z/T axes. base never already has C right after XY here.
    match base {
        // XYTCZ -> XYTZ -> XYCTZ
        DimensionOrder::XYTCZ => DimensionOrder::XYCTZ,
        // XYZTC -> XYZT -> XYCZT
        DimensionOrder::XYZTC => DimensionOrder::XYCZT,
        // XYCTZ -> XYTZ -> XYCTZ (unchanged)
        DimensionOrder::XYCTZ => DimensionOrder::XYCTZ,
        // XYCZT -> XYZT -> XYCZT (unchanged)
        DimensionOrder::XYCZT => DimensionOrder::XYCZT,
        // XYZCT -> XYZT -> XYCZT
        DimensionOrder::XYZCT => DimensionOrder::XYCZT,
        DimensionOrder::XYTZC => DimensionOrder::XYCTZ,
    }
}

fn lsm_pixel_type(data_type: i32, tiff_pixel_type: PixelType) -> PixelType {
    // Java derives pixelType from the TIFF IFD before reading CZ-LSMInfo
    // (ZeissLSMReader.java:724-738). CZ-LSMInfo DataType is mostly descriptive:
    // 2 marks 12-bit data stored in 16-bit samples, 5 marks float data, and
    // 0/"varying" or unknown values do not make the file unsupported.
    match data_type {
        2 => PixelType::Uint16,
        5 => PixelType::Float32,
        _ => {
            if tiff_pixel_type == PixelType::Uint32 {
                PixelType::Float32
            } else {
                tiff_pixel_type
            }
        }
    }
}

fn lsm_filter_type_and_range(filter: &str) -> (Option<String>, Option<f64>, Option<f64>) {
    let Some((kind, range)) = filter.trim().split_once(' ') else {
        return (None, None, None);
    };
    let filter_type = match kind.trim() {
        "BP" => Some("BandPass".to_string()),
        "LP" => Some("LongPass".to_string()),
        other if !other.is_empty() => Some(other.to_string()),
        _ => None,
    };
    let mut values = range.split('-');
    let cut_in = values.next().and_then(|v| v.trim().parse::<f64>().ok());
    let cut_out = values.next().and_then(|v| v.trim().parse::<f64>().ok());
    (filter_type, cut_in, cut_out)
}

fn lsm_zct_coords(meta: &ImageMetadata, plane_index: u32) -> (u32, u32, u32) {
    let z_size = meta.size_z.max(1);
    let c_size = meta.size_c.max(1);
    let t_size = meta.size_t.max(1);
    match meta.dimension_order {
        DimensionOrder::XYZCT => {
            let z = plane_index % z_size;
            let c = (plane_index / z_size) % c_size;
            let t = plane_index / (z_size * c_size);
            (z, c, t.min(t_size.saturating_sub(1)))
        }
        DimensionOrder::XYCTZ => {
            let c = plane_index % c_size;
            let t = (plane_index / c_size) % t_size;
            let z = plane_index / (c_size * t_size);
            (z.min(z_size.saturating_sub(1)), c, t)
        }
        DimensionOrder::XYCZT => {
            let c = plane_index % c_size;
            let z = (plane_index / c_size) % z_size;
            let t = plane_index / (c_size * z_size);
            (z, c, t.min(t_size.saturating_sub(1)))
        }
        DimensionOrder::XYTCZ => {
            let t = plane_index % t_size;
            let c = (plane_index / t_size) % c_size;
            let z = plane_index / (t_size * c_size);
            (z.min(z_size.saturating_sub(1)), c, t)
        }
        DimensionOrder::XYZTC => {
            let z = plane_index % z_size;
            let t = (plane_index / z_size) % t_size;
            let c = plane_index / (z_size * t_size);
            (z, c.min(c_size.saturating_sub(1)), t)
        }
        DimensionOrder::XYTZC => {
            let t = plane_index % t_size;
            let z = (plane_index / t_size) % z_size;
            let c = plane_index / (t_size * z_size);
            (z, c.min(c_size.saturating_sub(1)), t)
        }
    }
}

fn lsm_populate_java_planes(
    image: &mut crate::common::ome_metadata::OmeImage,
    meta: &ImageMetadata,
    timestamps: &[f64],
    position: Option<[f64; 3]>,
) {
    if !image.planes.is_empty() {
        return;
    }
    let first_stamp = timestamps.first().copied().unwrap_or(0.0);
    image.planes.reserve(meta.image_count as usize);
    for plane_index in 0..meta.image_count {
        let (the_z, the_c, the_t) = lsm_zct_coords(meta, plane_index);
        let delta_t = if meta.size_t > 1 {
            timestamps
                .get(the_t as usize)
                .map(|stamp| stamp - first_stamp)
                .filter(|v| v.is_finite())
        } else {
            None
        };
        image.planes.push(OmePlane {
            the_z,
            the_c,
            the_t,
            delta_t,
            exposure_time: None,
            position_x: position.map(|p| p[0]),
            position_y: position.map(|p| p[1]),
            position_z: position.map(|p| p[2]),
        });
    }
}

fn enrich_lsm_ome_from_scan_info(
    ome: &mut crate::common::ome_metadata::OmeMetadata,
    scan_info: &LsmScanInfo,
    size_c: usize,
) {
    if scan_info.recordings.is_empty()
        && scan_info.lasers.is_empty()
        && scan_info.detection_channels.is_empty()
        && scan_info.illumination_channels.is_empty()
        && scan_info.beam_splitters.is_empty()
    {
        return;
    }

    if ome.instruments.is_empty() {
        ome.instruments.push(OmeInstrument {
            id: Some(create_lsid("Instrument", &[0])),
            ..Default::default()
        });
    }
    if let Some(image) = ome.images.get_mut(0) {
        image.instrument_ref = Some(0);
        if image.light_paths.len() < image.channels.len() {
            image
                .light_paths
                .resize_with(image.channels.len(), OmeLightPath::default);
        }
    }

    let mut next_laser = 0usize;
    let mut next_detector = 0usize;
    let mut next_filter = 0usize;
    let mut next_dichroic_channel = 0usize;
    let mut next_dichroic = 0usize;
    let mut next_detect_channel = 0usize;
    let mut next_illum_channel = 0usize;

    for block in lsm_scan_blocks_for_java_population(scan_info) {
        match block {
            LsmScanBlockRef::Recording(idx) => {
                let Some(recording) = scan_info.recordings.get(idx) else {
                    continue;
                };
                let instrument = &mut ome.instruments[0];
                if instrument.objectives.is_empty() {
                    instrument.objectives.push(OmeObjective {
                        id: Some(create_lsid("Objective", &[0, 0])),
                        ..Default::default()
                    });
                }
                let objective = &mut instrument.objectives[0];
                objective.model = recording.objective_model.clone();
                objective.nominal_magnification = recording.magnification;
                objective.lens_na = recording.lens_na;
                objective.immersion = recording.immersion.clone();
                objective.correction = recording.correction.clone();

                if recording.acquire {
                    if let Some(image) = ome.images.get_mut(0) {
                        image.description = recording.description.clone();
                        image.acquisition_date = recording.start_time.clone();
                        image.objective_ref = Some(0);
                        for channel in &mut image.channels {
                            channel.detector_settings_binning = recording.binning.clone();
                        }
                    }
                }
                let _ = (&recording.name, &recording.zoom, &recording.user_name);
            }
            LsmScanBlockRef::Laser(idx) => {
                let Some(laser) = scan_info.lasers.get(idx) else {
                    continue;
                };
                let instrument = &mut ome.instruments[0];
                instrument.light_sources.push(OmeLightSource {
                    id: Some(create_lsid("LightSource", &[0, next_laser])),
                    model: laser.model.clone(),
                    light_source_type: Some("Laser".into()),
                    power: laser.power,
                    ..Default::default()
                });
                next_laser += 1;
                let _ = (&laser.acquire, &laser.laser_type, &laser.medium);
            }
            LsmScanBlockRef::Track(idx) => {
                let Some(track) = scan_info.tracks.get(idx) else {
                    continue;
                };
                if track.acquire && track.time_increment.is_some() {
                    if let Some(image) = ome.images.get_mut(0) {
                        image.time_increment = track.time_increment;
                    }
                }
            }
            LsmScanBlockRef::DetectionChannel(idx) => {
                let Some(channel) = scan_info.detection_channels.get(idx) else {
                    continue;
                };
                let detector_id = create_lsid("Detector", &[0, next_detector]);
                {
                    let instrument = &mut ome.instruments[0];
                    if let Some(filter_model) = &channel.filter {
                        let filter_id = create_lsid("Filter", &[0, next_filter]);
                        let (filter_type, cut_in, cut_out) =
                            lsm_filter_type_and_range(filter_model);
                        instrument.filters.push(OmeFilter {
                            id: Some(filter_id.clone()),
                            model: Some(filter_model.clone()),
                            filter_type,
                            cut_in,
                            cut_out,
                            ..Default::default()
                        });
                        if channel.acquire {
                            if let Some(image) = ome.images.get_mut(0) {
                                if next_detect_channel < image.light_paths.len() {
                                    image.light_paths[next_detect_channel]
                                        .emission_filter_ids
                                        .push(filter_id);
                                }
                            }
                        }
                        next_filter += 1;
                    }

                    instrument.detectors.push(OmeDetector {
                        id: Some(detector_id.clone()),
                        detector_type: Some("PMT".into()),
                        gain: channel.gain,
                        ..Default::default()
                    });
                }

                if channel.acquire && next_detector < size_c {
                    if let Some(image) = ome.images.get_mut(0) {
                        if next_detector < image.channels.len() {
                            let ome_channel = &mut image.channels[next_detector];
                            ome_channel.pinhole_size = channel.pinhole;
                            ome_channel.detector_settings_gain = channel.gain;
                            ome_channel.detector_ref = Some(detector_id);
                        }
                    }
                }
                next_detect_channel += 1;
                next_detector += 1;
                // Java uses scan-info ChannelName only as a guard for detector
                // IDs; OME Channel.Name is populated from the channel-colours
                // name table later in ome_metadata().
                let _ = (
                    &channel.channel_name,
                    &channel.amplification_gain,
                    &channel.filter_set,
                );
            }
            LsmScanBlockRef::IlluminationChannel(idx) => {
                let Some(illumination) = scan_info.illumination_channels.get(idx) else {
                    continue;
                };
                if illumination.acquire && illumination.wavelength.is_some() {
                    let instrument = &mut ome.instruments[0];
                    if instrument.light_sources.len() <= next_illum_channel {
                        instrument.light_sources.push(OmeLightSource {
                            id: Some(create_lsid("LightSource", &[0, next_illum_channel])),
                            light_source_type: Some("Laser".into()),
                            ..Default::default()
                        });
                    }
                    instrument.light_sources[next_illum_channel].wavelength =
                        illumination.wavelength;
                    next_illum_channel += 1;
                }
                let _ = (&illumination.name, &illumination.attenuation);
            }
            LsmScanBlockRef::BeamSplitter(idx) => {
                let Some(beam_splitter) = scan_info.beam_splitters.get(idx) else {
                    continue;
                };
                if beam_splitter.filter_set.is_some() {
                    if let Some(filter_model) = &beam_splitter.filter {
                        let instrument = &mut ome.instruments[0];
                        instrument.dichroics.push(OmeDichroic {
                            id: Some(create_lsid("Dichroic", &[0, next_dichroic])),
                            model: Some(filter_model.clone()),
                            ..Default::default()
                        });
                        next_dichroic += 1;
                    }
                    next_dichroic_channel += 1;
                }
                let _ = next_dichroic_channel;
            }
            LsmScanBlockRef::DataChannel(idx) => {
                let Some(data_channel) = scan_info.data_channels.get(idx) else {
                    continue;
                };
                let _ = (&data_channel.acquire, &data_channel.name);
            }
        }
    }
}

// ── Minimal TIFF IFD reader for fetching CZ_LSMInfo bytes ────────────────────
fn read_lsm_info_from_file(path: &Path) -> Result<(LsmInfo, bool)> {
    let f = File::open(path).map_err(BioFormatsError::Io)?;
    let buf = BufReader::new(f);
    let mut parser = TiffParser::new(buf)?;
    let le = parser.little_endian;
    let (ifd, _) = parser.read_ifd(parser.first_ifd_offset)?;

    // Find CZ_LSMInfo tag
    let lsm_bytes = match ifd.get(CZ_LSM_INFO) {
        Some(IfdValue::Byte(b)) => b.clone(),
        Some(IfdValue::Undefined(b)) => b.clone(),
        _ => {
            return Err(BioFormatsError::Format(
                "LSM: CZ_LSMInfo tag (34412) not found in first IFD".into(),
            ))
        }
    };

    let mut info = parse_lsm_info(&lsm_bytes, le)?;

    // Referenced LSM sub-structures are addressed by absolute file offsets.
    // Read the whole file once to resolve the Java-populated channel, scan,
    // timestamp, and position tables.
    if info.channel_colors_offset != 0
        || info.scan_information_offset != 0
        || info.application_tag_offset != 0
        || info.timestamp_offset != 0
        || info.position_offset != 0
        || info.tile_position_offset != 0
    {
        if let Ok(file_bytes) = std::fs::read(path) {
            info.timestamps = parse_lsm_timestamps(&file_bytes, info.timestamp_offset, le);
            info.positions = parse_lsm_positions(&file_bytes, &info, le);
            info.is_sim =
                parse_lsm_application_tags_is_sim(&file_bytes, info.application_tag_offset, le);
            info.channel_colors = parse_channel_colors(
                &file_bytes,
                info.channel_colors_offset,
                info.dim_c,
                info.is_sim,
                le,
            );
            info.channel_names =
                parse_channel_names(&file_bytes, info.channel_colors_offset, info.dim_c, le);
            if info.scan_information_offset != 0 {
                info.scan_info = parse_lsm_scan_info(&file_bytes, info.scan_information_offset, le);
            }
        }
    }

    Ok((info, le))
}

// ── Reader ────────────────────────────────────────────────────────────────────

pub struct ZeissLsmReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    series_metas: Vec<ImageMetadata>,
    current_series: usize,
    series_plane_offsets: Vec<u32>,
    /// Inner TIFF reader handles pixel I/O; we select the correct series.
    inner: TiffReader,
    /// When true, one physical IFD packs all `size_c` channels in planar order
    /// and a logical plane maps to (ifd = plane / sizeC, channel = plane % sizeC),
    /// with the channel sliced out (Java splitPlanes path).
    split_planes: bool,
    /// Per-channel names parsed from the CZ-LSMINFO channel-colours sub-block.
    channel_names: Vec<String>,
    /// OME image names (file stem plus Java position suffix for multi-position
    /// single-file LSM), mirroring ZeissLSMReader.getLSMFileFromSeries naming.
    image_names: Vec<Option<String>>,
    /// Active OME image name.
    image_name: Option<String>,
    /// Parsed recording/laser/detector/filter scan-information subblocks.
    scan_info: LsmScanInfo,
    /// Per-channel display colours from the CZ-LSMINFO channel-colours sub-block.
    channel_colors: Vec<[u8; 3]>,
    /// Per-timepoint timestamps in seconds from the LSM timestamp table.
    timestamps: Vec<f64>,
    /// Per-position coordinates from LSM position/tile-position tables.
    positions: Vec<[f64; 3]>,
}

impl ZeissLsmReader {
    pub fn new() -> Self {
        ZeissLsmReader {
            path: None,
            meta: None,
            series_metas: Vec::new(),
            current_series: 0,
            series_plane_offsets: Vec::new(),
            inner: TiffReader::new(),
            split_planes: false,
            channel_names: Vec::new(),
            image_names: Vec::new(),
            image_name: None,
            scan_info: LsmScanInfo::default(),
            channel_colors: Vec::new(),
            timestamps: Vec::new(),
            positions: Vec::new(),
        }
    }

    fn collect_full_resolution_ifds(&self, best_series: usize) -> Vec<usize> {
        let series = self.inner.series_list();
        let Some(target) = series.get(best_series).map(|s| &s.metadata) else {
            return Vec::new();
        };

        series
            .iter()
            .filter(|s| {
                let meta = &s.metadata;
                meta.size_x == target.size_x
                    && meta.size_y == target.size_y
                    && meta.size_c == target.size_c
                    && meta.bits_per_pixel == target.bits_per_pixel
                    && meta.pixel_type == target.pixel_type
                    && meta.is_rgb == target.is_rgb
                    && meta.is_interleaved == target.is_interleaved
            })
            .flat_map(|s| s.ifd_indices.iter().copied())
            .collect()
    }

    fn configure_full_resolution_series(&mut self, best_series: usize) -> u32 {
        let full_res_ifds = self.collect_full_resolution_ifds(best_series);
        let full_res_ifd_count = full_res_ifds.len() as u32;
        if !full_res_ifds.is_empty() {
            if let Some(series) = self.inner.series_list_mut().get_mut(best_series) {
                series.ifd_indices = full_res_ifds;
                series.plane_ifd_indices.clear();
                series.metadata.image_count = full_res_ifd_count;
                series.metadata.size_z = full_res_ifd_count;
            }
        }
        full_res_ifd_count
    }

    fn activate_series(&mut self, s: usize) -> Result<()> {
        let meta = self
            .series_metas
            .get(s)
            .cloned()
            .ok_or(BioFormatsError::SeriesOutOfRange(s))?;
        self.current_series = s;
        self.meta = Some(meta);
        self.image_name = self.image_names.get(s).cloned().unwrap_or(None);
        Ok(())
    }
}

impl Default for ZeissLsmReader {
    fn default() -> Self {
        Self::new()
    }
}

fn is_lsm_mdb_header(header: &[u8]) -> bool {
    header.get(4..6) == Some(&[0x53, 0x74])
        && header
            .get(6..)
            .is_some_and(|rest| rest.windows(2).any(|window| window == b"ID"))
}

impl FormatReader for ZeissLsmReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        // ZeissLSMReader.java:205-211 registers both "lsm" and "mdb"; MDB
        // companion parsing is still not implemented below, but name detection
        // should not reject the Java-registered suffix.
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("lsm" | "mdb"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // LSM files are TIFF; keep TIFF byte ownership with the TIFF reader.
        // Java ZeissLSMReader.java also accepts MDB streams when bytes 4..6
        // equal 0x5374 and the remaining probe block contains "ID".
        is_lsm_mdb_header(header)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        // First, read the CZ_LSMInfo block to get true dimensions
        let (lsm_info, le) = read_lsm_info_from_file(path)?;

        // Open with inner TIFF reader to get pixel dimensions and read pixel data
        self.inner.set_id(path)?;

        // ZeissLSMReader.java:544-548 — many .lsm files carry a stray
        // PREDICTOR=2 tag on IFDs whose compression is NOT LZW; the predictor
        // must only be honoured for LZW data. Force PREDICTOR=1 on every IFD
        // that is not LZW-compressed, after the inner reader has parsed the
        // IFDs and before any pixel read (get_samples re-derives the
        // predictor from the live IFD, so this mutation takes effect).
        let ifd_count = self.inner.ifd_count();
        for i in 0..ifd_count {
            if let Some(ifd) = self.inner.ifd_mut(i) {
                if ifd.compression() != Compression::Lzw {
                    ifd.entries.insert(tag::PREDICTOR, IfdValue::Short(vec![1]));
                }
            }
        }

        // The TIFF reader may have multiple series (full-res + thumbnails).
        // Select the series with the largest images.
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
        // Capture the first full-resolution IFD index *before* the series is
        // reconfigured, so we can inspect its SamplesPerPixel.
        let first_full_res_ifd = self
            .collect_full_resolution_ifds(best_series)
            .first()
            .copied();
        let full_res_ifd_count = self.configure_full_resolution_series(best_series);
        let tiff_meta = self.inner.metadata().clone();

        // ZeissLSMReader.java:720,725 — sizeC/rgb derive from the full-res IFD's
        // SamplesPerPixel. When a single IFD carries more than one sample (planar
        // multi-channel, e.g. SamplesPerPixel=2, PlanarConfiguration=2), every
        // physical IFD holds *all* channels and Java splits them into separate
        // planes (splitPlanes path, java:410-428, 988-992). Otherwise the file
        // stores one channel per IFD.
        let samples_per_ifd = first_full_res_ifd
            .and_then(|i| self.inner.ifd(i))
            .map(|ifd| ifd.samples_per_pixel())
            .unwrap_or(1);

        // Build corrected metadata using LSM dimensions.
        //
        // sizeC comes from the CZ-LSMINFO channel field (offset 20). There are
        // two physical layouts (see `samples_per_ifd` above):
        //
        //   * packed (samples_per_ifd > 1): one IFD per Z/T plane carries all C
        //     channels in planar order. full_res_ifd_count == Z*T. Java splits
        //     these into C logical planes (splitPlanes), so imageCount = Z*C*T.
        //     We slice the requested channel out of the planar IFD in
        //     open_bytes_region.
        //   * separate (samples_per_ifd == 1): one IFD per channel. We expose
        //     each IFD as a logical plane directly. imageCount = ifd count.
        let dim_z = lsm_info.dim_z;
        let dim_c = lsm_info.dim_c;
        let dim_t = lsm_info.dim_t;

        // A planar/packed multichannel LSM: SamplesPerPixel>1 on the full-res
        // IFD and exactly one IFD per Z/T plane (java:410 condition
        // `ifds.size() == sizeZ * sizeT`).
        let (lsm_series_count, ifds_per_series) = lsm_series_plan(&lsm_info, full_res_ifd_count);

        let split_planes = samples_per_ifd > 1
            && dim_c > 1
            && checked_plane_count(dim_z, 1, dim_t).ok() == Some(ifds_per_series);

        let image_count = if split_planes {
            checked_plane_count(dim_z, dim_c, dim_t)?
        } else {
            ifds_per_series
        };

        let pixel_type = lsm_pixel_type(lsm_info.data_type, tiff_meta.pixel_type);
        // ZeissLSMReader sets rgb=samples>1 to drive the dimension-order shuffle,
        // but always flattens rgb back to false once channels are split / the
        // image is indexed (java:877, 990). We never expose LSM as packed RGB.
        let rgb_for_order = samples_per_ifd > 1;
        let is_rgb = false;

        let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
        meta_map.insert(
            "voxel_size_x_um".into(),
            MetadataValue::Float(lsm_info.voxel_x * 1e6),
        );
        meta_map.insert(
            "voxel_size_y_um".into(),
            MetadataValue::Float(lsm_info.voxel_y * 1e6),
        );
        meta_map.insert(
            "voxel_size_z_um".into(),
            MetadataValue::Float(lsm_info.voxel_z * 1e6),
        );
        // ZeissLSMReader.java:954 records TimeInterval; surfaced as the OME
        // TimeIncrement (seconds).
        if lsm_info.time_interval != 0.0 {
            meta_map.insert(
                "time_increment_s".into(),
                MetadataValue::Float(lsm_info.time_interval),
            );
        }

        // OME image name: Java uses the LSM file path; ImageReader/OME later
        // reduce it to the file's base name. Use the file stem to match.
        let base_image_name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string());

        // Java sets indexed=true when the channel-colours sub-block contains a
        // colour table (java:1122-1129). Names alone do not make the series
        // indexed.
        let is_indexed = !lsm_info.channel_colors.is_empty();
        let lookup_table = lsm_info
            .channel_colors
            .first()
            .copied()
            .and_then(|color| lsm_lookup_table_for_pixel_type(color, pixel_type));

        let meta = ImageMetadata {
            size_x: tiff_meta.size_x,
            size_y: tiff_meta.size_y,
            size_z: dim_z,
            size_c: dim_c,
            size_t: dim_t,
            pixel_type,
            bits_per_pixel: (tiff_meta.bits_per_pixel),
            image_count,
            dimension_order: lsm_dimension_order(lsm_info.scan_type, rgb_for_order),
            is_rgb,
            is_interleaved: tiff_meta.is_interleaved,
            is_indexed,
            is_little_endian: le,
            resolution_count: 1,
            thumbnail: false,
            series_metadata: meta_map,
            lookup_table,
            // ZeissLSMReader.java:1000-1025 maps Rotations/Illuminations/Phases
            // to ModuloAlongZ/C/T with step equal to the pre-expanded axis size.
            modulo_z: lsm_modulo("Z", "angle", dim_z, lsm_info.rotations),
            modulo_c: lsm_modulo("C", "illumination", dim_c, lsm_info.illuminations),
            modulo_t: lsm_modulo("T", "phase", dim_t, lsm_info.phases),
        };

        self.split_planes = split_planes;
        self.channel_names = lsm_info.channel_names;
        self.scan_info = lsm_info.scan_info;
        self.channel_colors = lsm_info.channel_colors;
        self.timestamps = lsm_info.timestamps;
        self.positions = lsm_info.positions;
        self.series_metas = (0..lsm_series_count).map(|_| meta.clone()).collect();
        self.series_plane_offsets = (0..lsm_series_count).map(|s| s * ifds_per_series).collect();
        self.image_names = (0..lsm_series_count)
            .map(|s| {
                base_image_name.as_ref().map(|name| {
                    if lsm_series_count > 1 {
                        format!("{name} #{}", s + 1)
                    } else {
                        name.clone()
                    }
                })
            })
            .collect();
        self.activate_series(0)?;
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.series_metas = Vec::new();
        self.current_series = 0;
        self.series_plane_offsets = Vec::new();
        self.split_planes = false;
        self.channel_names = Vec::new();
        self.image_names = Vec::new();
        self.image_name = None;
        self.scan_info = LsmScanInfo::default();
        self.channel_colors = Vec::new();
        self.timestamps = Vec::new();
        self.positions = Vec::new();
        let _ = self.inner.close();
        Ok(())
    }

    fn series_count(&self) -> usize {
        self.series_metas.len()
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        self.activate_series(s)
    }

    fn series(&self) -> usize {
        self.current_series
    }

    fn metadata(&self) -> &ImageMetadata {
        self.meta
            .as_ref()
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let (w, h) = (meta.size_x, meta.size_y);
        self.open_bytes_region(plane_index, 0, 0, w, h)
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

        if self.split_planes {
            // One physical IFD packs all channels in planar order. Map the
            // logical plane to (ifd, channel) and slice the channel out, mirroring
            // ZeissLSMReader.java:410-428 (getSamples + ImageTools.splitChannels,
            // non-interleaved). dimensionOrder is XY C..., so C is the fastest
            // axis: ifd = no / sizeC, channel = no % sizeC.
            let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
            let size_c = meta.size_c.max(1);
            let bpp = meta.pixel_type.bytes_per_sample();
            let physical = self
                .series_plane_offsets
                .get(self.current_series)
                .copied()
                .unwrap_or(0)
                + plane_index / size_c;
            let channel = (plane_index % size_c) as usize;
            if physical >= inner_count {
                return Err(BioFormatsError::PlaneOutOfRange(plane_index));
            }
            let packed = self.inner.open_bytes_region(physical, x, y, w, h)?;
            let chan_len = (w as usize) * (h as usize) * bpp;
            let start = chan_len * channel;
            let end = start + chan_len;
            if end > packed.len() {
                return Err(BioFormatsError::Format(format!(
                    "LSM: split-channel slice {start}..{end} exceeds plane length {}",
                    packed.len()
                )));
            }
            return Ok(packed[start..end].to_vec());
        }

        let inner_idx = self
            .series_plane_offsets
            .get(self.current_series)
            .copied()
            .unwrap_or(0)
            .checked_add(plane_index)
            .ok_or_else(|| BioFormatsError::Format("LSM: plane index overflow".into()))?;
        if inner_idx >= inner_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        self.inner.open_bytes_region(inner_idx, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = ((meta.size_x - tw) / 2, (meta.size_y - th) / 2);
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn lookup_table(&mut self, plane_index: u32) -> Result<Option<LookupTable>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count || !meta.is_indexed {
            return Ok(None);
        }
        let (_z, channel, _t) = lsm_zct_coords(meta, plane_index);
        let Some(color) = self.channel_colors.get(channel as usize).copied() else {
            return Ok(None);
        };
        Ok(lsm_lookup_table_for_pixel_type(color, meta.pixel_type))
    }

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        use crate::common::metadata::MetadataValue;
        use crate::common::ome_metadata::OmeMetadata;
        let meta = self.meta.as_ref()?;
        let mut ome = OmeMetadata::from_image_metadata(meta);
        {
            let img = &mut ome.images[0];
            let get_f = |k: &str| -> Option<f64> {
                if let Some(MetadataValue::Float(v)) = meta.series_metadata.get(k) {
                    Some(*v)
                } else {
                    None
                }
            };
            // Already stored in µm
            img.physical_size_x = get_f("voxel_size_x_um");
            img.physical_size_y = get_f("voxel_size_y_um");
            img.physical_size_z = get_f("voxel_size_z_um");
            img.time_increment = get_f("time_increment_s");
            img.name = self.image_name.clone();
        }
        enrich_lsm_ome_from_scan_info(&mut ome, &self.scan_info, meta.size_c as usize);
        let img = &mut ome.images[0];
        // Channel names from the CZ-LSMINFO channel-colours sub-block
        // (ZeissLSMReader.java:1351 store.setChannelName).
        for (ci, name) in self.channel_names.iter().enumerate() {
            if let Some(ch) = img.channels.get_mut(ci) {
                if !name.is_empty() {
                    ch.name = Some(name.clone());
                }
            }
        }
        for (ci, color) in self.channel_colors.iter().copied().enumerate() {
            if let Some(ch) = img.channels.get_mut(ci) {
                ch.color = Some(lsm_pack_rgba(color));
            }
        }
        let timestamp_start = self
            .current_series
            .checked_mul(meta.size_t as usize)
            .unwrap_or(usize::MAX);
        let timestamps = self.timestamps.get(timestamp_start..).unwrap_or(&[]);
        lsm_populate_java_planes(
            img,
            meta,
            timestamps,
            self.positions.get(self.current_series).copied(),
        );
        Some(ome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lsm_plane_mapping_rejects_logical_planes_without_physical_ifds() {
        assert_eq!(resolve_lsm_plane_index(0, 3, 2).unwrap(), 0);
        assert_eq!(resolve_lsm_plane_index(1, 3, 2).unwrap(), 1);
        assert!(matches!(
            resolve_lsm_plane_index(2, 3, 2),
            Err(BioFormatsError::PlaneOutOfRange(2))
        ));
    }

    #[test]
    fn lsm_plane_mapping_rejects_planes_past_logical_count() {
        assert!(matches!(
            resolve_lsm_plane_index(2, 2, 4),
            Err(BioFormatsError::PlaneOutOfRange(2))
        ));
    }

    #[test]
    fn lsm_split_plane_image_count_multiplies_by_channels() {
        // Packed multichannel: one IFD per Z/T plane carries all channels, so
        // the logical plane count is Z*C*T.
        assert_eq!(checked_plane_count(33, 2, 1).unwrap(), 66);
        assert_eq!(checked_plane_count(2, 3, 4).unwrap(), 24);
    }

    #[test]
    fn lsm_dimension_order_shuffles_c_when_packed() {
        // scanType 0 -> XYZCT, RGB-style shuffle moves C to front -> XYCZT.
        assert_eq!(lsm_dimension_order(0, false), DimensionOrder::XYZCT);
        assert_eq!(lsm_dimension_order(0, true), DimensionOrder::XYCZT);
    }

    #[test]
    fn lsm_pixel_type_keeps_tiff_type_for_varying_data_type_like_java() {
        assert_eq!(lsm_pixel_type(0, PixelType::Uint8), PixelType::Uint8);
        assert_eq!(lsm_pixel_type(99, PixelType::Int16), PixelType::Int16);
        assert_eq!(lsm_pixel_type(0, PixelType::Uint32), PixelType::Float32);
        assert_eq!(lsm_pixel_type(2, PixelType::Uint8), PixelType::Uint16);
        assert_eq!(lsm_pixel_type(5, PixelType::Uint16), PixelType::Float32);
    }

    #[test]
    fn lsm_info_projects_java_rotation_phase_illumination_modulo() {
        let mut bytes = vec![0u8; 284];
        bytes[16..20].copy_from_slice(&3i32.to_le_bytes());
        bytes[20..24].copy_from_slice(&2i32.to_le_bytes());
        bytes[24..28].copy_from_slice(&4i32.to_le_bytes());
        bytes[264..268].copy_from_slice(&2i32.to_le_bytes());
        bytes[268..272].copy_from_slice(&3i32.to_le_bytes());
        bytes[272..276].copy_from_slice(&5i32.to_le_bytes());
        bytes[276..280].copy_from_slice(&6i32.to_le_bytes());
        bytes[280..284].copy_from_slice(&7i32.to_le_bytes());

        let info = parse_lsm_info(&bytes, true).unwrap();
        assert_eq!(info.rotations, 5);
        assert_eq!(info.phases, 6);
        assert_eq!(info.illuminations, 7);
        assert_eq!(info.dimension_p, 2);
        assert_eq!(info.dimension_m, 3);
        assert_eq!(lsm_series_plan(&info, 24), (6, 4));

        let modulo_z = lsm_modulo("Z", "angle", info.dim_z, info.rotations).unwrap();
        let modulo_c = lsm_modulo("C", "illumination", info.dim_c, info.illuminations).unwrap();
        let modulo_t = lsm_modulo("T", "phase", info.dim_t, info.phases).unwrap();
        assert_eq!(modulo_z.modulo_type, "angle");
        assert_eq!(modulo_z.step, 3.0);
        assert_eq!(modulo_z.end, 12.0);
        assert_eq!(modulo_c.modulo_type, "illumination");
        assert_eq!(modulo_c.step, 2.0);
        assert_eq!(modulo_c.end, 12.0);
        assert_eq!(modulo_t.modulo_type, "phase");
        assert_eq!(modulo_t.step, 4.0);
        assert_eq!(modulo_t.end, 20.0);
    }

    #[test]
    fn lsm_info_retains_java_rich_subblock_offsets() {
        let mut bytes = vec![0u8; 380];
        let mut put_i32 = |offset: usize, value: i32| {
            bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        };

        put_i32(16, 1);
        put_i32(20, 1);
        put_i32(24, 1);
        put_i32(96, 1001);
        put_i32(100, 1002);
        put_i32(104, 1003);
        put_i32(108, 2001);
        put_i32(124, 3001);
        put_i32(128, 3002);
        put_i32(132, 3003);
        put_i32(136, 3004);
        put_i32(140, 1004);
        put_i32(144, 1005);
        put_i32(184, 1006);
        put_i32(188, 1007);
        put_i32(192, 1008);
        put_i32(196, 1009);
        put_i32(204, 3005);

        let info = parse_lsm_info(&bytes, true).unwrap();
        assert_eq!(
            info.overlay_offsets,
            [1001, 1002, 1003, 1004, 1005, 1006, 1007, 1008, 1009]
        );
        assert_eq!(info.channel_colors_offset, 2001);
        assert_eq!(info.scan_information_offset, 3001);
        assert_eq!(info.application_tag_offset, 3002);
        assert_eq!(info.timestamp_offset, 3003);
        assert_eq!(info.event_list_offset, 3004);
        assert_eq!(info.channel_wavelength_offset, 3005);
    }

    #[test]
    fn lsm_series_plan_falls_back_when_dimension_mp_is_absent_like_java() {
        let info = LsmInfo {
            dimension_m: 0,
            dimension_p: 0,
            ..Default::default()
        };
        assert_eq!(lsm_series_plan(&info, 9), (1, 9));

        let info = LsmInfo {
            dimension_m: 2,
            dimension_p: 2,
            ..Default::default()
        };
        assert_eq!(lsm_series_plan(&info, 10), (4, 2));
    }

    #[test]
    fn lsm_name_detection_accepts_java_registered_mdb_suffix() {
        let reader = ZeissLsmReader::new();
        assert!(reader.is_this_type_by_name(Path::new("dataset.lsm")));
        assert!(reader.is_this_type_by_name(Path::new("dataset.MDB")));
        assert!(!reader.is_this_type_by_name(Path::new("dataset.tif")));
    }

    #[test]
    fn lsm_byte_detection_accepts_java_mdb_probe() {
        let reader = ZeissLsmReader::new();
        let mut header = vec![0u8; 4096];
        header[4..6].copy_from_slice(&[0x53, 0x74]);
        header[128..130].copy_from_slice(b"ID");

        assert!(is_lsm_mdb_header(&header));
        assert!(reader.is_this_type_by_bytes(&header));

        header[4] = 0;
        assert!(!reader.is_this_type_by_bytes(&header));
    }

    #[test]
    fn lsm_byte_detection_leaves_tiff_header_to_tiff_reader() {
        let reader = ZeissLsmReader::new();
        assert!(!reader.is_this_type_by_bytes(b"II*\0\x08\0\0\0"));
        assert!(!reader.is_this_type_by_bytes(b"MM\0*\0\0\0\x08"));
    }

    #[test]
    fn lsm_parse_channel_names_reads_length_prefixed_table() {
        let le = true;
        // channel-colours sub-block at file offset 4. Layout:
        //   +12 colorsOffset (int), +16 namesOffset (int)
        // names table at offset 4 + namesOffset.
        let names_offset: i32 = 24;
        let mut buf = vec![0u8; 4]; // 0..4 header padding (base = 4)
        buf.resize(4 + names_offset as usize, 0); // fill up to the names table
                                                  // +12 colorsOffset, +16 namesOffset relative to base=4
        buf[4 + 12..4 + 16].copy_from_slice(&0i32.to_le_bytes());
        buf[4 + 16..4 + 20].copy_from_slice(&names_offset.to_le_bytes());
        // names table at base + names_offset = index 28
        for name in ["Ch2-T1\0", "Ch1-T2\0"] {
            buf.extend_from_slice(&(name.len() as i32).to_le_bytes());
            buf.extend_from_slice(name.as_bytes());
        }
        let names = parse_channel_names(&buf, 4, 2, le);
        assert_eq!(names, vec!["Ch2-T1".to_string(), "Ch1-T2".to_string()]);
    }

    #[test]
    fn lsm_parse_channel_colors_applies_java_black_fallbacks() {
        let le = true;
        // channel-colours sub-block at file offset 4. +12 stores the relative
        // colorsOffset. Stored colors are read as R=low, G=middle, B=high byte.
        let colors_offset: i32 = 24;
        let mut buf = vec![0u8; 4];
        buf.resize(4 + colors_offset as usize, 0);
        buf[4 + 12..4 + 16].copy_from_slice(&colors_offset.to_le_bytes());
        for color in [0x0000_3322u32, 0x0000_0000u32, 0x0000_00ffu32] {
            buf.extend_from_slice(&color.to_le_bytes());
        }

        let non_sim = parse_channel_colors(&buf, 4, 3, false, le);
        assert_eq!(non_sim, vec![[0x22, 0x33, 0], [255, 255, 255], [255, 0, 0]]);

        let sim = parse_channel_colors(&buf, 4, 3, true, le);
        assert_eq!(sim, vec![[0x22, 0x33, 0], [0x22, 0x33, 0], [255, 0, 0]]);
    }

    #[test]
    fn lsm_parses_java_timestamp_and_position_tables() {
        let le = true;
        let mut buf = vec![0u8; 260];
        let timestamp_offset = 40usize;
        buf[timestamp_offset + 4..timestamp_offset + 8].copy_from_slice(&3i32.to_le_bytes());
        for (i, stamp) in [12.0f64, 13.5, 20.0].into_iter().enumerate() {
            let off = timestamp_offset + 8 + i * 8;
            buf[off..off + 8].copy_from_slice(&stamp.to_le_bytes());
        }

        let position_offset = 100usize;
        buf[position_offset..position_offset + 4].copy_from_slice(&1i32.to_le_bytes());
        for (i, value) in [0.001f64, 0.002, 0.003].into_iter().enumerate() {
            let off = position_offset + 4 + i * 8;
            buf[off..off + 8].copy_from_slice(&value.to_le_bytes());
        }

        let tile_position_offset = 200usize;
        buf[tile_position_offset..tile_position_offset + 4].copy_from_slice(&1i32.to_le_bytes());
        for (i, value) in [0.0001f64, 0.0002, 0.0003].into_iter().enumerate() {
            let off = tile_position_offset + 4 + i * 8;
            buf[off..off + 8].copy_from_slice(&value.to_le_bytes());
        }

        let info = LsmInfo {
            origin_um: [10.0, 20.0, 30.0],
            position_offset: position_offset as u32,
            tile_position_offset: tile_position_offset as u32,
            ..Default::default()
        };

        assert_eq!(
            parse_lsm_timestamps(&buf, timestamp_offset as u32, le),
            vec![12.0, 13.5, 20.0]
        );
        assert_eq!(
            parse_lsm_positions(&buf, &info, le),
            vec![[1120.0, 2240.0, 3360.0]]
        );
    }

    #[test]
    fn lsm_ome_planes_project_java_delta_t_and_positions() {
        let meta = ImageMetadata {
            size_x: 2,
            size_y: 2,
            size_z: 1,
            size_c: 1,
            size_t: 3,
            image_count: 3,
            pixel_type: PixelType::Uint8,
            bits_per_pixel: 8,
            dimension_order: DimensionOrder::XYTCZ,
            is_little_endian: true,
            ..Default::default()
        };
        let mut ome = crate::common::ome_metadata::OmeMetadata::from_image_metadata(&meta);
        lsm_populate_java_planes(
            &mut ome.images[0],
            &meta,
            &[12.0, 13.5, 20.0],
            Some([1.0, 2.0, 3.0]),
        );

        let planes = &ome.images[0].planes;
        assert_eq!(planes.len(), 3);
        assert_eq!(planes[0].delta_t, Some(0.0));
        assert_eq!(planes[1].delta_t, Some(1.5));
        assert_eq!(planes[2].delta_t, Some(8.0));
        assert_eq!(planes[0].position_x, Some(1.0));
        assert_eq!(planes[1].position_y, Some(2.0));
        assert_eq!(planes[2].position_z, Some(3.0));
    }

    #[test]
    fn lsm_multi_position_series_switches_name_timestamp_and_position() {
        let meta = ImageMetadata {
            size_x: 2,
            size_y: 2,
            size_z: 1,
            size_c: 1,
            size_t: 2,
            image_count: 2,
            pixel_type: PixelType::Uint8,
            bits_per_pixel: 8,
            dimension_order: DimensionOrder::XYTCZ,
            is_little_endian: true,
            ..Default::default()
        };
        let mut reader = ZeissLsmReader::new();
        reader.series_metas = vec![meta.clone(), meta];
        reader.series_plane_offsets = vec![0, 2];
        reader.image_names = vec![Some("multi #1".into()), Some("multi #2".into())];
        reader.timestamps = vec![10.0, 12.0, 100.0, 106.0];
        reader.positions = vec![[1.0, 2.0, 3.0], [4.0, 5.0, 6.0]];

        reader.set_series(1).unwrap();
        assert_eq!(reader.series(), 1);
        assert_eq!(reader.metadata().image_count, 2);

        let ome = reader.ome_metadata().expect("OME metadata");
        assert_eq!(ome.images[0].name.as_deref(), Some("multi #2"));
        assert_eq!(ome.images[0].planes[0].delta_t, Some(0.0));
        assert_eq!(ome.images[0].planes[1].delta_t, Some(6.0));
        assert_eq!(ome.images[0].planes[0].position_x, Some(4.0));
        assert_eq!(ome.images[0].planes[1].position_z, Some(6.0));
    }

    #[test]
    fn lsm_lookup_table_selects_plane_channel_like_java() {
        let mut reader = ZeissLsmReader::new();
        reader.meta = Some(ImageMetadata {
            size_x: 2,
            size_y: 2,
            size_z: 1,
            size_c: 2,
            size_t: 1,
            image_count: 2,
            pixel_type: PixelType::Uint8,
            bits_per_pixel: 8,
            dimension_order: DimensionOrder::XYCZT,
            is_indexed: true,
            is_little_endian: true,
            ..Default::default()
        });
        reader.channel_colors = vec![[255, 0, 0], [0, 128, 255]];

        let lut0 = reader.lookup_table(0).unwrap().expect("channel 0 LUT");
        assert_eq!(lut0.red[255], 255);
        assert_eq!(lut0.green[255], 0);
        assert_eq!(lut0.blue[255], 0);

        let lut1 = reader.lookup_table(1).unwrap().expect("channel 1 LUT");
        assert_eq!(lut1.red[255], 0);
        assert_eq!(lut1.green[255], 128);
        assert_eq!(lut1.blue[255], 255);

        let ome = reader.ome_metadata().expect("OME metadata");
        assert_eq!(ome.images[0].channels[0].color, Some(0xff0000ffu32 as i32));
        assert_eq!(ome.images[0].channels[1].color, Some(0x0080ffffu32 as i32));
    }

    fn push_subblock(buf: &mut Vec<u8>, entry: i32) {
        buf.extend_from_slice(&entry.to_le_bytes());
        buf.extend_from_slice(&TYPE_SUBBLOCK.to_le_bytes());
        buf.extend_from_slice(&0i32.to_le_bytes());
    }

    fn push_long(buf: &mut Vec<u8>, entry: i32, value: i32) {
        buf.extend_from_slice(&entry.to_le_bytes());
        buf.extend_from_slice(&TYPE_LONG.to_le_bytes());
        buf.extend_from_slice(&4i32.to_le_bytes());
        buf.extend_from_slice(&value.to_le_bytes());
    }

    fn push_rational(buf: &mut Vec<u8>, entry: i32, value: f64) {
        buf.extend_from_slice(&entry.to_le_bytes());
        buf.extend_from_slice(&TYPE_RATIONAL.to_le_bytes());
        buf.extend_from_slice(&8i32.to_le_bytes());
        buf.extend_from_slice(&value.to_le_bytes());
    }

    fn push_ascii(buf: &mut Vec<u8>, entry: i32, value: &str) {
        let mut bytes = value.as_bytes().to_vec();
        bytes.push(0);
        buf.extend_from_slice(&entry.to_le_bytes());
        buf.extend_from_slice(&TYPE_ASCII.to_le_bytes());
        buf.extend_from_slice(&(bytes.len() as i32).to_le_bytes());
        buf.extend_from_slice(&bytes);
    }

    fn push_application_tag(buf: &mut Vec<u8>, name: &str, data_type: i32, payload: &[u8]) {
        let entry_size = 16 + name.len() + payload.len();
        buf.extend_from_slice(&(entry_size as i32).to_le_bytes());
        buf.extend_from_slice(&(name.len() as i32).to_le_bytes());
        buf.extend_from_slice(name.as_bytes());
        buf.extend_from_slice(&data_type.to_le_bytes());
        buf.extend_from_slice(&(payload.len() as i32).to_le_bytes());
        buf.extend_from_slice(payload);
    }

    #[test]
    fn lsm_application_tags_set_sim_color_fallback_like_java() {
        let mut buf = vec![0u8; 32];
        let app_offset = buf.len();
        buf.extend_from_slice(&0i32.to_le_bytes());
        buf.extend_from_slice(&2i32.to_le_bytes());
        push_application_tag(&mut buf, "OtherApplicationTag", TYPE_ASCII, b"value");
        push_application_tag(&mut buf, "SimPar0", TYPE_LONG, &1i32.to_le_bytes());

        assert!(parse_lsm_application_tags_is_sim(
            &buf,
            app_offset as u32,
            true
        ));

        let colors_offset = buf.len();
        let table_offset: i32 = 24;
        buf.resize(colors_offset + table_offset as usize, 0);
        buf[colors_offset + 12..colors_offset + 16].copy_from_slice(&table_offset.to_le_bytes());
        for color in [0x0000_3322u32, 0x0000_0000u32] {
            buf.extend_from_slice(&color.to_le_bytes());
        }

        let sim_colors = parse_channel_colors(&buf, colors_offset as u32, 2, true, true);
        let non_sim_colors = parse_channel_colors(&buf, colors_offset as u32, 2, false, true);
        assert_eq!(sim_colors, vec![[0x22, 0x33, 0], [0x22, 0x33, 0]]);
        assert_eq!(non_sim_colors, vec![[0x22, 0x33, 0], [255, 255, 255]]);
    }

    #[test]
    fn lsm_scan_info_parses_recording_laser_detector_and_filters() {
        let mut buf = vec![0u8; 16];
        push_subblock(&mut buf, SUBBLOCK_RECORDING);
        push_ascii(&mut buf, RECORDING_DESCRIPTION, "desc");
        push_ascii(&mut buf, RECORDING_USER, "alice");
        push_ascii(
            &mut buf,
            RECORDING_OBJECTIVE,
            "Plan-Apochromat 63x/1.40 Oil",
        );
        push_rational(&mut buf, RECORDING_SAMPLE_0TIME, 2.0);
        push_subblock(&mut buf, SUBBLOCK_END);
        push_subblock(&mut buf, SUBBLOCK_LASER);
        push_ascii(&mut buf, LASER_NAME, "HeNe 633");
        push_long(&mut buf, LASER_ACQUIRE, 1);
        push_rational(&mut buf, LASER_POWER, 4.5);
        push_subblock(&mut buf, SUBBLOCK_END);
        push_subblock(&mut buf, SUBBLOCK_TRACK);
        push_long(&mut buf, TRACK_ACQUIRE, 1);
        push_subblock(&mut buf, SUBBLOCK_DETECTION_CHANNEL);
        push_ascii(&mut buf, CHANNEL_NAME, "PMT 1");
        push_ascii(&mut buf, CHANNEL_FILTER, "BP 500-550");
        push_long(&mut buf, CHANNEL_ACQUIRE, 1);
        push_rational(&mut buf, CHANNEL_DETECTOR_GAIN, 700.0);
        push_rational(&mut buf, CHANNEL_PINHOLE_DIAMETER, 45.0);
        push_subblock(&mut buf, SUBBLOCK_ILLUMINATION_CHANNEL);
        push_ascii(&mut buf, ILLUM_CHANNEL_NAME, "633");
        push_long(&mut buf, ILLUM_CHANNEL_ACQUIRE, 1);
        push_subblock(&mut buf, SUBBLOCK_DATA_CHANNEL);
        push_ascii(&mut buf, DATA_CHANNEL_NAME, "Ch1");
        push_long(&mut buf, DATA_CHANNEL_ACQUIRE, 1);
        push_subblock(&mut buf, SUBBLOCK_END);

        let info = parse_lsm_scan_info(&buf, 16, true);
        assert_eq!(info.recordings[0].description.as_deref(), Some("desc"));
        assert_eq!(info.recordings[0].user_name.as_deref(), Some("alice"));
        assert_eq!(info.recordings[0].magnification, Some(63.0));
        assert_eq!(info.recordings[0].lens_na, Some(1.40));
        assert_eq!(info.lasers[0].medium.as_deref(), Some("HeNe"));
        assert_eq!(info.lasers[0].laser_type.as_deref(), Some("Gas"));
        assert_eq!(info.illumination_channels[0].wavelength, Some(633.0));
        assert_eq!(
            info.detection_channels[0].filter.as_deref(),
            Some("BP 500-550")
        );
        assert_eq!(info.detection_channels[0].gain, Some(700.0));
    }

    #[test]
    fn lsm_scan_info_applies_java_adjacency_rules() {
        let mut buf = vec![0u8; 16];
        push_subblock(&mut buf, SUBBLOCK_LASER);
        push_ascii(&mut buf, LASER_NAME, "HeNe 633");
        push_long(&mut buf, LASER_ACQUIRE, 1);
        push_subblock(&mut buf, SUBBLOCK_DETECTION_CHANNEL);
        push_ascii(&mut buf, CHANNEL_NAME, "PMT 1");
        push_long(&mut buf, CHANNEL_ACQUIRE, 1);
        push_subblock(&mut buf, SUBBLOCK_ILLUMINATION_CHANNEL);
        push_ascii(&mut buf, ILLUM_CHANNEL_NAME, "488");
        push_long(&mut buf, ILLUM_CHANNEL_ACQUIRE, 1);
        push_subblock(&mut buf, SUBBLOCK_LASER);
        push_ascii(&mut buf, LASER_NAME, "Argon");
        push_long(&mut buf, LASER_ACQUIRE, 1);
        push_subblock(&mut buf, SUBBLOCK_TRACK);
        push_long(&mut buf, TRACK_ACQUIRE, 1);
        push_subblock(&mut buf, SUBBLOCK_DETECTION_CHANNEL);
        push_ascii(&mut buf, CHANNEL_NAME, "PMT 2");
        push_long(&mut buf, CHANNEL_ACQUIRE, 1);
        push_subblock(&mut buf, SUBBLOCK_ILLUMINATION_CHANNEL);
        push_ascii(&mut buf, ILLUM_CHANNEL_NAME, "561");
        push_long(&mut buf, ILLUM_CHANNEL_ACQUIRE, 1);
        push_subblock(&mut buf, SUBBLOCK_DATA_CHANNEL);
        push_ascii(&mut buf, DATA_CHANNEL_NAME, "Ch2");
        push_long(&mut buf, DATA_CHANNEL_ACQUIRE, 1);
        push_subblock(&mut buf, SUBBLOCK_END);

        let info = parse_lsm_scan_info(&buf, 16, true);
        assert!(!info.detection_channels[0].acquire);
        assert!(info.detection_channels[1].acquire);
        assert_eq!(info.illumination_channels[0].wavelength, None);
        assert_eq!(info.illumination_channels[1].wavelength, Some(561.0));
    }

    #[test]
    fn lsm_scan_info_enriches_supported_ome_fields() {
        let meta = ImageMetadata {
            size_x: 2,
            size_y: 2,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            image_count: 1,
            pixel_type: PixelType::Uint8,
            bits_per_pixel: 8,
            dimension_order: DimensionOrder::XYZCT,
            is_little_endian: true,
            ..Default::default()
        };
        let mut ome = crate::common::ome_metadata::OmeMetadata::from_image_metadata(&meta);
        let scan_info = LsmScanInfo {
            recordings: vec![LsmRecording {
                acquire: true,
                description: Some("recording".into()),
                objective_model: Some("Plan-Apochromat 63x/1.40 Oil".into()),
                magnification: Some(63.0),
                lens_na: Some(1.4),
                immersion: Some("Oil".into()),
                correction: Some("Plan-Apochromat".into()),
                ..Default::default()
            }],
            lasers: vec![LsmLaser {
                model: Some("HeNe 633".into()),
                power: Some(4.5),
                ..Default::default()
            }],
            illumination_channels: vec![LsmIlluminationChannel {
                acquire: true,
                wavelength: Some(633.0),
                ..Default::default()
            }],
            detection_channels: vec![LsmDetectionChannel {
                acquire: true,
                channel_name: Some("PMT 1".into()),
                pinhole: Some(45.0),
                gain: Some(700.0),
                filter: Some("BP 500-550".into()),
                ..Default::default()
            }],
            beam_splitters: vec![LsmBeamSplitter {
                filter: Some("NFT 545".into()),
                filter_set: Some("Main".into()),
            }],
            block_order: vec![
                LsmScanBlockRef::Recording(0),
                LsmScanBlockRef::Laser(0),
                LsmScanBlockRef::IlluminationChannel(0),
                LsmScanBlockRef::DetectionChannel(0),
                LsmScanBlockRef::BeamSplitter(0),
            ],
            ..Default::default()
        };

        enrich_lsm_ome_from_scan_info(&mut ome, &scan_info, 1);
        assert_eq!(ome.images[0].description.as_deref(), Some("recording"));
        assert_eq!(ome.instruments[0].objectives[0].lens_na, Some(1.4));
        assert_eq!(ome.instruments[0].light_sources[0].wavelength, Some(633.0));
        assert_eq!(ome.instruments[0].detectors[0].gain, Some(700.0));
        assert_eq!(
            ome.instruments[0].filters[0].filter_type.as_deref(),
            Some("BandPass")
        );
        assert_eq!(ome.images[0].channels[0].name, None);
        assert_eq!(ome.images[0].channels[0].pinhole_size, Some(45.0));
        assert_eq!(
            ome.images[0].light_paths[0].emission_filter_ids[0].as_str(),
            "Filter:0:0"
        );
        assert_eq!(
            ome.instruments[0].dichroics[0].id.as_deref(),
            Some("Dichroic:0:0")
        );
    }

    #[test]
    fn lsm_scan_info_populates_acquired_blocks_before_non_acquired_like_java() {
        let meta = ImageMetadata {
            size_x: 2,
            size_y: 2,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            image_count: 1,
            pixel_type: PixelType::Uint8,
            bits_per_pixel: 8,
            dimension_order: DimensionOrder::XYZCT,
            is_little_endian: true,
            ..Default::default()
        };
        let mut ome = crate::common::ome_metadata::OmeMetadata::from_image_metadata(&meta);
        let scan_info = LsmScanInfo {
            detection_channels: vec![
                LsmDetectionChannel {
                    acquire: false,
                    channel_name: Some("skipped PMT".into()),
                    gain: Some(10.0),
                    filter: Some("BP 600-650".into()),
                    ..Default::default()
                },
                LsmDetectionChannel {
                    acquire: true,
                    channel_name: Some("active PMT".into()),
                    pinhole: Some(42.0),
                    gain: Some(20.0),
                    filter: Some("BP 500-550".into()),
                    ..Default::default()
                },
            ],
            block_order: vec![
                LsmScanBlockRef::DetectionChannel(0),
                LsmScanBlockRef::DetectionChannel(1),
            ],
            ..Default::default()
        };

        enrich_lsm_ome_from_scan_info(&mut ome, &scan_info, 1);

        assert_eq!(ome.instruments[0].detectors.len(), 2);
        assert_eq!(ome.instruments[0].detectors[0].gain, Some(20.0));
        assert_eq!(ome.instruments[0].detectors[1].gain, Some(10.0));
        assert_eq!(ome.images[0].channels[0].name, None);
        assert_eq!(
            ome.images[0].channels[0].detector_ref.as_deref(),
            Some("Detector:0:0")
        );
        assert_eq!(
            ome.images[0].light_paths[0].emission_filter_ids,
            vec!["Filter:0:0".to_string()]
        );
        assert_eq!(
            ome.instruments[0].filters[0].model.as_deref(),
            Some("BP 500-550")
        );
        assert_eq!(
            ome.instruments[0].filters[1].model.as_deref(),
            Some("BP 600-650")
        );
    }

    #[test]
    fn lsm_real_fixture_scan_info_and_ome_counts_match_java_summary() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/lsm/colocsample1b.lsm");
        if !path.exists() {
            eprintln!("skip missing {path:?}");
            return;
        }
        let (info, le) = read_lsm_info_from_file(&path).unwrap();
        assert_eq!(info.scan_info.lasers.len(), 2);
        assert_eq!(info.scan_info.detection_channels.len(), 6);
        assert_eq!(info.scan_info.beam_splitters.len(), 10);
        assert_eq!(info.overlay_offsets[0], 0);
        assert_eq!(info.overlay_offsets[3..], [0; 6]);
        assert_eq!(
            info.overlay_offsets[1..3],
            [9676, 9740],
            "checked-in LSM fixture only carries InputLut/OutputLut overlay offsets"
        );
        let file_bytes = std::fs::read(&path).unwrap();
        for &offset in &info.overlay_offsets[1..3] {
            let offset = offset as usize;
            let number_of_shapes = read_i32_lsm(&file_bytes, offset, le);
            let block_size = read_i32_lsm(&file_bytes, offset + 4, le);
            assert_eq!(number_of_shapes, 64);
            assert_eq!(
                block_size, 0,
                "Java parseOverlays returns before OME ROI projection when size <= 194"
            );
        }
        assert_eq!(
            info.overlay_offsets[3], 0,
            "checked-in LSM fixture cannot exercise Java ROI overlay projection"
        );
        assert_eq!(
            info.application_tag_offset, 0,
            "checked-in LSM fixture cannot exercise Java parseApplicationTags path"
        );
        assert_eq!(
            info.event_list_offset, 0,
            "checked-in LSM fixture cannot exercise Java event-list parsing path"
        );
        assert_eq!(
            info.channel_wavelength_offset, 0,
            "checked-in LSM fixture cannot exercise Java channel-wavelength table parsing"
        );

        let mut reader = ZeissLsmReader::new();
        reader.set_id(&path).unwrap();
        let ome = reader.ome_metadata().unwrap();
        assert_eq!(
            ome.instruments
                .iter()
                .map(|i| i.detectors.len())
                .sum::<usize>(),
            6
        );
        assert_eq!(
            ome.instruments
                .iter()
                .map(|i| i.light_sources.len())
                .sum::<usize>(),
            2
        );
        assert_eq!(
            ome.instruments
                .iter()
                .map(|i| i.filters.len())
                .sum::<usize>(),
            4
        );
        assert_eq!(
            ome.instruments
                .iter()
                .map(|i| i.dichroics.len())
                .sum::<usize>(),
            6
        );
        assert_eq!(ome.images.iter().map(|i| i.planes.len()).sum::<usize>(), 66);
    }
}
