//! Zeiss ZVI format reader (OLE2/CFB container).
//!
//! ZVI is the Zeiss AxioVision proprietary microscopy format.
//! It uses OLE2 Compound File Binary (CFB) as its container — the same
//! format as old Microsoft Office .doc/.xls files.
//!
//! Key streams:
//!   /Image/CONTENTS            — global metadata (width, height, pixel type)
//!   /Image/Item(N)/CONTENTS    — per-plane pixel data (N is 1-based)
//!   /Image/Item(N)/Tags/CONTENTS — per-plane z/c/t indices

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{
    DimensionOrder, ImageMetadata, MetadataLevel, MetadataOptions, MetadataValue,
};
use crate::common::ome_metadata::{
    create_lsid, OmeDetector, OmeExperimenter, OmeInstrument, OmePlane, OmeROI, OmeShape,
};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;

pub struct ZeissZviReader {
    path: Option<PathBuf>,
    comp: Option<cfb::CompoundFile<File>>,
    meta: Option<ImageMetadata>,
    planes: Vec<ZviPlane>,
    bytes_per_pixel: usize,
    is_rgb: bool,
    /// Number of tiles; each tile is exposed as a separate series, matching
    /// ZeissZVIReader where `totalTiles = offsets.length / getImageCount()` and
    /// `coordinates[i][3]` (the tile index) selects the series.
    tile_count: usize,
    current_series: usize,
    /// OME enrichment harvested from the per-item Tags streams.
    ome_info: ZviOmeInfo,
    metadata_options: MetadataOptions,
}

/// OME metadata harvested from ZVI tag streams, mirroring the subset of
/// BaseZeissReader.parseMainTags that can be backed by already parsed Rust tag
/// streams: image description, physical pixel sizes, objective fields,
/// detector/channel fields, and per-plane exposure/stage metadata.
#[derive(Default, Clone)]
struct ZviOmeInfo {
    image_description: Option<String>,
    physical_size_x: Option<f64>,
    physical_size_y: Option<f64>,
    physical_size_z: Option<f64>,
    objective_magnification: Option<f64>,
    objective_lens_na: Option<f64>,
    objective_immersion: Option<String>,
    objective_working_distance: Option<f64>,
    acquisition_date: Option<String>,
    experimenter_first_name: Option<String>,
    experimenter_last_name: Option<String>,
    experimenter_institution: Option<String>,
    objective_id: Option<String>,
    objective_correction: Option<String>,
    /// channel index -> detector gain
    detector_gain: HashMap<u32, f64>,
    /// channel index -> detector offset
    detector_offset: HashMap<u32, f64>,
    /// image item index -> exposure time (seconds)
    exposure_time: HashMap<usize, f64>,
    /// image item index -> camera acquisition timestamp, in Java/ZVI Excel days.
    camera_time: HashMap<usize, f64>,
    /// image item index -> stage X/Y in reference-frame units.
    stage_x: HashMap<usize, f64>,
    stage_y: HashMap<usize, f64>,
    /// channel index -> name
    channel_names: HashMap<u32, String>,
    /// channel index -> emission wavelength (nm)
    emission: HashMap<u32, f64>,
    /// channel index -> excitation wavelength (nm)
    excitation: HashMap<u32, f64>,
    /// channel index -> Java packed false color.
    channel_colors: HashMap<u32, i32>,
    rois: Vec<OmeROI>,
}

struct ZviPlane {
    /// Stream path inside the CFB, e.g. "/Image/Item(1)/CONTENTS"
    stream_path: String,
    /// Original Item(N) index, used to attach per-item Tags to sorted planes.
    image_num: usize,
    z: u32,
    c: u32,
    t: u32,
    /// Tile (mosaic) index — maps to the Bio-Formats series.
    tile: u32,
    /// Byte offset of pixel data within the item stream.
    data_offset: usize,
    is_zlib: bool,
    is_jpeg: bool,
}

/// The immediate parent directory component of a "…/<dir>/Contents" path.
///
/// Java derives `dirName` this way (the directory directly containing the
/// CONTENTS stream) and dispatches on it, so the image-item test must look only
/// at the parent dir, not the whole path — otherwise unrelated nested "Item(n)"
/// directories (Layers, RootFolder Locations, …) would also match.
fn parent_dir(p: &str) -> &str {
    let trimmed = p.strip_suffix('/').unwrap_or(p);
    let last_slash = match trimmed.rfind('/') {
        Some(i) => i,
        None => return "",
    };
    let dir_path = &trimmed[..last_slash];
    match dir_path.rfind('/') {
        Some(i) => &dir_path[i + 1..],
        None => dir_path,
    }
}

/// Port of `MetadataTools.makeSaneDimensionOrder`: keep only XYZCT characters,
/// then append any missing axis in the fixed precedence X, Y, C, Z, T and drop
/// duplicate occurrences. Maps the resulting 5-char string to a [`DimensionOrder`].
fn make_sane_dimension_order(input: &str) -> DimensionOrder {
    let mut order = String::new();
    for ch in input.to_ascii_uppercase().chars() {
        if matches!(ch, 'X' | 'Y' | 'Z' | 'C' | 'T') && !order.contains(ch) {
            order.push(ch);
        }
    }
    for axis in ['X', 'Y', 'C', 'Z', 'T'] {
        if !order.contains(axis) {
            order.push(axis);
        }
    }
    match order.as_str() {
        "XYCTZ" => DimensionOrder::XYCTZ,
        "XYCZT" => DimensionOrder::XYCZT,
        "XYTCZ" => DimensionOrder::XYTCZ,
        "XYTZC" => DimensionOrder::XYTZC,
        "XYZCT" => DimensionOrder::XYZCT,
        "XYZTC" => DimensionOrder::XYZTC,
        // makeSaneDimensionOrder can only yield the six XY-prefixed permutations
        // above; anything else means a logic error, so fall back to XYCZT.
        _ => DimensionOrder::XYCZT,
    }
}

/// The Z/C/T axis characters of a [`DimensionOrder`], in order (X, Y omitted).
fn dimension_order_axes(order: DimensionOrder) -> Vec<char> {
    let s = match order {
        DimensionOrder::XYCTZ => "CTZ",
        DimensionOrder::XYCZT => "CZT",
        DimensionOrder::XYTCZ => "TCZ",
        DimensionOrder::XYTZC => "TZC",
        DimensionOrder::XYZCT => "ZCT",
        DimensionOrder::XYZTC => "ZTC",
    };
    s.chars().collect()
}

fn zvi_tag_name(tag_id: u32) -> &'static str {
    match tag_id {
        515 => "ImageWidth",
        516 => "ImageHeight",
        518 => "PixelType",
        769 => "Scale Factor for X",
        770 => "Scale Unit for X",
        772 => "Scale Factor for Y",
        773 => "Scale Unit for Y",
        1025 | 1047 => "Camera Acquisition Time",
        1284 => "Channel Name",
        1537 => "Title",
        1538 => "Author",
        1540 => "Comments",
        1553 => "Filename",
        1793 => "Acquisition Date",
        1801 => "User Name",
        _ => "Unknown",
    }
}

/// Port of `DataTools.stripString`: drop NUL characters (ZVI strings are stored
/// UTF-16LE, so bytes decode to interleaved NULs) and trim surrounding
/// whitespace. This turns e.g. "C\0y\05" back into "Cy5".
fn strip_string(raw: &[u8]) -> String {
    String::from_utf8_lossy(raw)
        .chars()
        .filter(|&c| c != '\0')
        .collect::<String>()
        .trim()
        .to_string()
}

fn read_zvi_variant(data: &[u8], offset: &mut usize) -> Option<String> {
    let ty = u16::from_le_bytes(data.get(*offset..*offset + 2)?.try_into().ok()?);
    *offset += 2;
    let value = match ty {
        0 | 1 => String::new(),
        2 => {
            let v = i16::from_le_bytes(data.get(*offset..*offset + 2)?.try_into().ok()?);
            *offset += 2;
            v.to_string()
        }
        3 | 22 => {
            let v = i32::from_le_bytes(data.get(*offset..*offset + 4)?.try_into().ok()?);
            *offset += 4;
            v.to_string()
        }
        4 => {
            let v = f32::from_le_bytes(data.get(*offset..*offset + 4)?.try_into().ok()?);
            *offset += 4;
            v.to_string()
        }
        5 | 7 => {
            let v = f64::from_le_bytes(data.get(*offset..*offset + 8)?.try_into().ok()?);
            *offset += 8;
            v.to_string()
        }
        8 | 69 => {
            let len = u32::from_le_bytes(data.get(*offset..*offset + 4)?.try_into().ok()?) as usize;
            *offset += 4;
            let raw = data.get(*offset..*offset + len)?;
            *offset += len;
            strip_string(raw)
        }
        11 => {
            let v = u16::from_le_bytes(data.get(*offset..*offset + 2)?.try_into().ok()?) != 0;
            *offset += 2;
            v.to_string()
        }
        19 | 23 => {
            let v = u32::from_le_bytes(data.get(*offset..*offset + 4)?.try_into().ok()?);
            *offset += 4;
            v.to_string()
        }
        20 | 21 => {
            let v = u64::from_le_bytes(data.get(*offset..*offset + 8)?.try_into().ok()?);
            *offset += 8;
            v.to_string()
        }
        66 => {
            let len = u16::from_le_bytes(data.get(*offset..*offset + 2)?.try_into().ok()?) as usize;
            *offset += 2;
            let raw = data.get(*offset..*offset + len)?;
            *offset += len;
            strip_string(raw)
        }
        _ => return None,
    };
    Some(value)
}

fn read_zero_padded<R: Read>(reader: &mut R, out: &mut [u8]) -> std::io::Result<()> {
    let mut filled = 0usize;
    while filled < out.len() {
        let n = reader.read(&mut out[filled..])?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    Ok(())
}

fn parse_f64_tag(value: &str) -> Option<f64> {
    value.trim().parse::<f64>().ok().filter(|v| v.is_finite())
}

fn zvi_immersion_from_tag(value: &str) -> Option<String> {
    match value.trim().parse::<i32>().ok()? {
        2 => Some("Oil".to_string()),
        3 => Some("Water".to_string()),
        _ => Some("Other".to_string()),
    }
}

fn parse_zvi_objective_name(value: &str) -> (Option<String>, Option<f64>, Option<f64>) {
    let tokens: Vec<&str> = value.split_whitespace().collect();
    for (i, token) in tokens.iter().enumerate() {
        let Some(slash) = token.find('/') else {
            continue;
        };
        if slash == 0 {
            continue;
        }
        let mag = token[..slash].trim_end_matches('x').parse::<f64>().ok();
        let na = token[slash + 1..].parse::<f64>().ok();
        let correction = i
            .checked_sub(1)
            .and_then(|prev| tokens.get(prev))
            .map(|s| (*s).to_string());
        return (correction, mag, na);
    }
    (None, None, None)
}

fn zvi_timestamp_to_iso8601(value: &str) -> Option<String> {
    let dstamp = value.trim().parse::<f64>().ok()?;
    if !dstamp.is_finite() {
        return None;
    }

    let mut days = dstamp.floor() as i64 - 1;
    if days > 60 {
        // Match BaseZeissReader.parseTimestamp's Excel leap-year correction.
        days -= 1;
    }
    let millis_in_day = 24.0 * 60.0 * 60.0 * 1000.0;
    let mut millis = ((dstamp - dstamp.floor()) * millis_in_day + 0.5).floor() as i64;
    if millis >= 86_400_000 {
        days += 1;
        millis -= 86_400_000;
    }

    let (year, month, day) = civil_from_days(days);
    let hour = millis / 3_600_000;
    let minute = (millis / 60_000) % 60;
    let second = (millis / 1000) % 60;
    let millis = millis % 1000;
    Some(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}"
    ))
}

fn civil_from_days(days_since_1900_01_01: i64) -> (i64, u32, u32) {
    // Howard Hinnant's civil-from-days algorithm, with Unix day 0 shifted to
    // Java's ZVI epoch of 1900-01-01.
    let z = days_since_1900_01_01 - 25_567 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    (y + if m <= 2 { 1 } else { 0 }, m as u32, d as u32)
}

/// Harvest OME-relevant tags from one Tags stream, mirroring
/// BaseZeissReader.parseMainTags. Reads each (value, tagID) record in stream
/// order, tracking the current channel index (tag 2820 "Image Channel Index")
/// so that channel name (1284), emission (16777489), and excitation (16777488)
/// are attributed to the right channel. Physical sizes come from tags 769/772/775
/// ("Scale Factor for X/Y/Z"); Java keeps the first value seen for each.
/// Per-image tags such as exposure time and stage X/Y are attached when
/// `image_num` is the enclosing Item(N), matching ZeissZVIReader.java:246-257
/// and BaseZeissReader.java:934-1014.
///
/// `c_index` is threaded across all item streams to match Java's stateful field.
fn harvest_zvi_ome_tags(
    data: &[u8],
    info: &mut ZviOmeInfo,
    c_index: &mut i32,
    image_num: Option<usize>,
) {
    const TAG_SCALE_X: u32 = 769;
    const TAG_SCALE_Y: u32 = 772;
    const TAG_SCALE_Z: u32 = 775;
    const TAG_CHANNEL_COLOR: u32 = 1282;
    const TAG_CHANNEL_NAME: u32 = 1284;
    const TAG_COMMENTS: u32 = 1540;
    const TAG_ACQUISITION_DATE: u32 = 1793;
    const TAG_USER_COMPANY: u32 = 1795;
    const TAG_USER_NAME: u32 = 1801;
    const TAG_OBJECTIVE_NAME: u32 = 2049;
    const TAG_OBJECTIVE_MAGNIFICATION: u32 = 1412;
    const TAG_OBJECTIVE_MAGNIFICATION_ALT: u32 = 2076;
    const TAG_OBJECTIVE_NA: u32 = 1413;
    const TAG_OBJECTIVE_NA_ALT: u32 = 2077;
    const TAG_OBJECTIVE_WORKING_DISTANCE: u32 = 1415;
    const TAG_OBJECTIVE_WORKING_DISTANCE_ALT: u32 = 2151;
    const TAG_OBJECTIVE_IMMERSION: u32 = 1416;
    const TAG_OBJECTIVE_IMMERSION_ALT: u32 = 2105;
    const TAG_OBJECTIVE_ID: u32 = 2261;
    const TAG_CAMERA_ACQUISITION_TIME: u32 = 1025;
    const TAG_EXPOSURE_TIME_MS: u32 = 2564;
    const TAG_ORCA_ANALOG_GAIN: u32 = 65_633;
    const TAG_ORCA_ANALOG_OFFSET: u32 = 65_634;
    const TAG_STAGE_X: u32 = 16777218;
    const TAG_STAGE_Y: u32 = 16777219;
    const TAG_CHANNEL_INDEX: u32 = 2820;
    const TAG_EXCITATION: u32 = 16_777_488;
    const TAG_EMISSION: u32 = 16_777_489;

    if data.len() < 12 {
        return;
    }
    let count = u32::from_le_bytes([data[8], data[9], data[10], data[11]]) as usize;
    let mut offset = 12;
    for _ in 0..count {
        if offset + 2 >= data.len() {
            break;
        }
        let Some(value) = read_zvi_variant(data, &mut offset) else {
            break;
        };
        if offset + 12 > data.len() {
            break;
        }
        offset += 2;
        let tag_id = u32::from_le_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]);
        offset += 10;

        match tag_id {
            TAG_CHANNEL_INDEX => {
                if let Ok(v) = value.trim().parse::<i32>() {
                    *c_index = v;
                }
            }
            TAG_SCALE_X => {
                if info.physical_size_x.is_none() {
                    if let Some(v) = parse_f64_tag(&value) {
                        info.physical_size_x = Some(v);
                    }
                }
            }
            TAG_SCALE_Y => {
                if info.physical_size_y.is_none() {
                    if let Some(v) = parse_f64_tag(&value) {
                        info.physical_size_y = Some(v);
                    }
                }
            }
            TAG_SCALE_Z => {
                if info.physical_size_z.is_none() {
                    if let Some(v) = parse_f64_tag(&value) {
                        info.physical_size_z = Some(v);
                    }
                }
            }
            TAG_COMMENTS => {
                let value = value.trim();
                if !value.is_empty() {
                    info.image_description = Some(value.to_string());
                }
            }
            TAG_ACQUISITION_DATE => {
                info.acquisition_date = zvi_timestamp_to_iso8601(&value);
            }
            TAG_USER_NAME => {
                let parts: Vec<&str> = value.split_whitespace().collect();
                if parts.len() >= 2 {
                    info.experimenter_first_name = Some(parts[0].to_string());
                    info.experimenter_last_name = Some(parts[parts.len() - 1].to_string());
                }
            }
            TAG_USER_COMPANY => {
                let value = value.trim();
                if !value.is_empty() {
                    info.experimenter_institution = Some(value.to_string());
                }
            }
            TAG_OBJECTIVE_NAME => {
                let (correction, mag, na) = parse_zvi_objective_name(&value);
                if let Some(v) = mag.filter(|v| *v > 0.0) {
                    info.objective_magnification = Some(v);
                }
                if let Some(v) = na.filter(|v| *v > 0.0) {
                    info.objective_lens_na = Some(v);
                }
                if let Some(v) = correction {
                    info.objective_correction = Some(v);
                }
            }
            TAG_OBJECTIVE_MAGNIFICATION | TAG_OBJECTIVE_MAGNIFICATION_ALT => {
                info.objective_magnification = parse_f64_tag(&value).filter(|v| *v > 0.0);
            }
            TAG_OBJECTIVE_ID => {
                let value = value.trim();
                if !value.is_empty() {
                    info.objective_id = Some(format!("Objective:{value}"));
                }
            }
            TAG_OBJECTIVE_NA | TAG_OBJECTIVE_NA_ALT => {
                info.objective_lens_na = parse_f64_tag(&value).filter(|v| *v > 0.0);
            }
            TAG_OBJECTIVE_WORKING_DISTANCE | TAG_OBJECTIVE_WORKING_DISTANCE_ALT => {
                info.objective_working_distance = parse_f64_tag(&value).filter(|v| *v > 0.0);
            }
            TAG_OBJECTIVE_IMMERSION | TAG_OBJECTIVE_IMMERSION_ALT => {
                info.objective_immersion = zvi_immersion_from_tag(&value);
            }
            TAG_CHANNEL_NAME => {
                if *c_index != -1 {
                    info.channel_names
                        .insert(*c_index as u32, value.trim().to_string());
                }
            }
            TAG_CHANNEL_COLOR => {
                if *c_index != -1 {
                    if let Ok(v) = value.trim().parse::<i32>() {
                        info.channel_colors.insert(*c_index as u32, v);
                    }
                }
            }
            TAG_EXPOSURE_TIME_MS => {
                if let (Some(image_num), Some(v)) = (image_num, parse_f64_tag(&value)) {
                    info.exposure_time.entry(image_num).or_insert(v / 1000.0);
                }
            }
            TAG_CAMERA_ACQUISITION_TIME => {
                if let (Some(image_num), Some(v)) = (image_num, parse_f64_tag(&value)) {
                    info.camera_time.insert(image_num, v);
                }
            }
            TAG_STAGE_X => {
                if let (Some(image_num), Some(v)) = (image_num, parse_f64_tag(&value)) {
                    info.stage_x.insert(image_num, v);
                }
            }
            TAG_STAGE_Y => {
                if let (Some(image_num), Some(v)) = (image_num, parse_f64_tag(&value)) {
                    info.stage_y.insert(image_num, v);
                }
            }
            TAG_ORCA_ANALOG_GAIN => {
                if *c_index != -1 {
                    if let Some(v) = parse_f64_tag(&value) {
                        info.detector_gain.insert(*c_index as u32, v);
                    }
                }
            }
            TAG_ORCA_ANALOG_OFFSET => {
                if *c_index != -1 {
                    if let Some(v) = parse_f64_tag(&value) {
                        info.detector_offset.insert(*c_index as u32, v);
                    }
                }
            }
            TAG_EMISSION => {
                if *c_index != -1 {
                    if let Some(v) = parse_f64_tag(&value) {
                        if v > 0.0 {
                            info.emission.insert(*c_index as u32, v);
                        }
                    }
                }
            }
            TAG_EXCITATION => {
                if *c_index != -1 {
                    if let Some(v) = parse_f64_tag(&value) {
                        if v > 0.0 {
                            info.excitation.insert(*c_index as u32, v);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

fn parse_zvi_tag_stream(data: &[u8], image_num: usize) -> HashMap<String, MetadataValue> {
    let mut map = HashMap::new();
    if data.len() < 12 {
        return map;
    }
    let count = u32::from_le_bytes([data[8], data[9], data[10], data[11]]) as usize;
    let mut offset = 12;
    for i in 0..count {
        let Some(value) = read_zvi_variant(data, &mut offset) else {
            break;
        };
        if offset + 12 > data.len() {
            break;
        }
        offset += 2;
        let tag_id = u32::from_le_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]);
        offset += 10;
        map.insert(
            format!("zvi.image.{image_num}.tag.{tag_id}"),
            MetadataValue::String(value.clone()),
        );
        let name = zvi_tag_name(tag_id);
        if name != "Unknown" {
            map.insert(
                format!("zvi.image.{image_num}.{name}"),
                MetadataValue::String(value),
            );
        }
        map.insert(
            format!("zvi.image.{image_num}.tag.{i}.id"),
            MetadataValue::Int(tag_id as i64),
        );
    }
    map
}

impl ZeissZviReader {
    pub fn new() -> Self {
        ZeissZviReader {
            path: None,
            comp: None,
            meta: None,
            planes: Vec::new(),
            bytes_per_pixel: 1,
            is_rgb: false,
            tile_count: 1,
            current_series: 0,
            ome_info: ZviOmeInfo::default(),
            metadata_options: MetadataOptions::default(),
        }
    }
}

impl Default for ZeissZviReader {
    fn default() -> Self {
        Self::new()
    }
}

/// A simple little-endian byte cursor over an in-memory stream, mirroring the
/// subset of RandomAccessInputStream behaviour used by ZeissZVIReader.
struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Cursor { data, pos: 0 }
    }

    fn len(&self) -> usize {
        self.data.len()
    }

    fn skip(&mut self, n: usize) {
        self.pos = self.pos.saturating_add(n);
    }

    fn seek(&mut self, pos: usize) {
        self.pos = pos;
    }

    fn read_i16(&mut self) -> Option<i16> {
        let b = self.data.get(self.pos..self.pos + 2)?;
        self.pos += 2;
        Some(i16::from_le_bytes([b[0], b[1]]))
    }

    fn read_u16(&mut self) -> Option<u16> {
        let b = self.data.get(self.pos..self.pos + 2)?;
        self.pos += 2;
        Some(u16::from_le_bytes([b[0], b[1]]))
    }

    fn read_i32(&mut self) -> Option<i32> {
        let b = self.data.get(self.pos..self.pos + 4)?;
        self.pos += 4;
        Some(i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn read_u64(&mut self) -> Option<u64> {
        let b = self.data.get(self.pos..self.pos + 8)?;
        self.pos += 8;
        Some(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    fn read_f64(&mut self) -> Option<f64> {
        let b = self.data.get(self.pos..self.pos + 8)?;
        self.pos += 8;
        Some(f64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    fn read_bytes(&mut self, len: usize) -> Option<&'a [u8]> {
        let b = self.data.get(self.pos..self.pos + len)?;
        self.pos += len;
        Some(b)
    }

    fn read_string(&mut self, len: usize) {
        // We only need to advance past the string for layout purposes.
        self.pos = self.pos.saturating_add(len);
    }
}

/// Port of ZeissZVIReader.getNextTag — advances the cursor past one VARIANT-typed
/// tag value. We only need the side effect on the cursor position, not the value.
fn skip_next_tag(s: &mut Cursor) {
    let ty = match s.read_i16() {
        Some(t) => t,
        None => return,
    };
    match ty {
        0 | 1 => {} // VT_EMPTY / VT_NULL
        2 | 11 => {
            s.skip(2);
        } // VT_I2 / VT_BOOL (readShort)
        3 | 22 | 19 | 23 | 4 => {
            s.skip(4);
        } // VT_I4/INT/UI4/UINT/R4
        5 | 7 | 20 | 21 => {
            s.skip(8);
        } // VT_R8/DATE/I8/UI8
        8 | 69 => {
            // VT_BSTR / VT_STORED_OBJECT: int length then string
            let len = s.read_i32().unwrap_or(0).max(0) as usize;
            s.read_string(len);
        }
        9 | 13 => {
            s.skip(16);
        } // VT_DISPATCH / VT_UNKNOWN
        63 | 65 => {
            // VT_BLOB: int length then skip
            let len = s.read_i32().unwrap_or(0).max(0) as usize;
            s.skip(len);
        }
        66 => {
            // VT_STREAM: short length then string
            let len = s.read_i16().unwrap_or(0).max(0) as usize;
            s.read_string(len);
        }
        _ => {
            // Unknown: scan forward until a short value of 3 (VT_I4) is found.
            let old = s.pos;
            while s.len() >= s.pos + 2 {
                if s.read_i16() == Some(3) {
                    break;
                }
            }
            let fp = s.pos.saturating_sub(2);
            s.pos = old.saturating_sub(2);
            s.read_string(fp.saturating_sub(old).saturating_add(2));
        }
    }
}

const ZVI_ROI_SIGNATURE: u64 = 0x21fff6977547000d;

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ZviFeatureType {
    Unknown,
    Point,
    Points,
    Line,
    Caliper,
    Distance,
    MultipleCaliper,
    MultipleDistance,
    Angle3,
    Angle4,
    Circle,
    ScaleBar,
    PolylineOpen,
    AlignedRectangle,
    Rectangle,
    Ellipse,
    PolylineClosed,
    Text,
    Length,
    SplineOpen,
    SplineClosed,
    Lut,
    MeasProfile,
    MeasPoint,
    MeasPoints,
    MeasLine,
    MeasCaliper,
    MeasDistance,
    MeasMultipleCaliper,
    MeasMultipleDistance,
    MeasAngle3,
    MeasAngle4,
    MeasCircle,
    MeasPolylineOpen,
    MeasAlignedRectangle,
    MeasRectangle,
    MeasEllipse,
    MeasPolylineClosed,
    MeasLength,
    MeasSplineOpen,
    MeasSplineClosed,
}

impl ZviFeatureType {
    fn get(value: i32) -> Self {
        match value {
            0 => Self::Point,
            1 => Self::Points,
            2 => Self::Line,
            3 => Self::Caliper,
            4 => Self::Distance,
            5 => Self::MultipleDistance,
            7 => Self::Angle3,
            8 => Self::Angle4,
            9 => Self::Circle,
            10 => Self::ScaleBar,
            12 => Self::PolylineOpen,
            13 => Self::AlignedRectangle,
            14 => Self::Rectangle,
            15 => Self::Ellipse,
            16 => Self::PolylineClosed,
            17 => Self::Text,
            18 => Self::Length,
            19 => Self::SplineOpen,
            20 => Self::SplineClosed,
            21 => Self::Lut,
            28 | 284 => Self::MeasProfile,
            32 => Self::MeasPoint,
            33 => Self::MeasPoints,
            34 => Self::MeasLine,
            35 => Self::MeasCaliper,
            36 => Self::MeasDistance,
            37 => Self::MeasMultipleCaliper,
            38 => Self::MeasMultipleDistance,
            39 => Self::MeasAngle3,
            40 => Self::MeasAngle4,
            41 => Self::MeasCircle,
            42 => Self::MeasPolylineOpen,
            43 => Self::MeasAlignedRectangle,
            44 => Self::MeasRectangle,
            45 => Self::MeasEllipse,
            46 => Self::MeasPolylineClosed,
            48 => Self::MeasLength,
            49 => Self::MeasSplineOpen,
            50 => Self::MeasSplineClosed,
            _ => Self::Unknown,
        }
    }
}

struct ZviParsedShape {
    ty: ZviFeatureType,
    name: Option<String>,
    text: Option<String>,
    points: Vec<(f64, f64)>,
}

fn parse_zvi_roi_string(s: &mut Cursor) -> Option<String> {
    while s.pos < s.len().saturating_sub(4) {
        if s.read_u16()? == 8 {
            break;
        }
    }
    if s.pos >= s.len().saturating_sub(8) {
        return None;
    }
    let strlen = s.read_i32()?.max(0) as usize;
    if strlen.checked_add(s.pos)? > s.len() {
        return None;
    }
    if strlen >= 2 {
        let raw = s.read_bytes(strlen - 2)?;
        s.skip(2);
        let utf16: Vec<u16> = raw
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .take_while(|&ch| ch != 0)
            .collect();
        Some(String::from_utf16_lossy(&utf16))
    } else {
        s.skip(strlen);
        Some(String::new())
    }
}

fn zvi_points_to_shape(shape: &ZviParsedShape) -> Vec<OmeShape> {
    let p = &shape.points;
    let none = (None, None, None);
    match shape.ty {
        ZviFeatureType::Point
        | ZviFeatureType::Points
        | ZviFeatureType::MeasPoint
        | ZviFeatureType::MeasPoints => p
            .iter()
            .map(|&(x, y)| OmeShape::Point {
                x,
                y,
                the_z: none.0,
                the_t: none.1,
                the_c: none.2,
            })
            .collect(),
        ZviFeatureType::Line | ZviFeatureType::MeasLine | ZviFeatureType::MeasProfile
            if p.len() >= 2 =>
        {
            vec![OmeShape::Line {
                x1: p[0].0,
                y1: p[0].1,
                x2: p[1].0,
                y2: p[1].1,
                the_z: none.0,
                the_t: none.1,
                the_c: none.2,
            }]
        }
        ZviFeatureType::Circle | ZviFeatureType::MeasCircle if p.len() >= 2 => {
            let radius = ((p[0].0 - p[1].0).powi(2) + (p[0].1 - p[1].1).powi(2)).sqrt();
            vec![
                OmeShape::Ellipse {
                    x: p[0].0,
                    y: p[0].1,
                    radius_x: radius,
                    radius_y: radius,
                    the_z: none.0,
                    the_t: none.1,
                    the_c: none.2,
                },
                OmeShape::Line {
                    x1: p[0].0,
                    y1: p[0].1,
                    x2: p[1].0,
                    y2: p[1].1,
                    the_z: none.0,
                    the_t: none.1,
                    the_c: none.2,
                },
            ]
        }
        ZviFeatureType::ScaleBar if p.len() >= 2 => vec![
            OmeShape::Line {
                x1: p[0].0,
                y1: p[0].1,
                x2: p[1].0,
                y2: p[1].1,
                the_z: none.0,
                the_t: none.1,
                the_c: none.2,
            },
            OmeShape::Point {
                x: p[0].0,
                y: p[0].1,
                the_z: none.0,
                the_t: none.1,
                the_c: none.2,
            },
            OmeShape::Rectangle {
                x: p[0].0,
                y: p[0].1,
                width: p[1].0 - p[0].0,
                height: p[1].1 - p[0].1,
                the_z: none.0,
                the_t: none.1,
                the_c: none.2,
            },
        ],
        ZviFeatureType::PolylineOpen
        | ZviFeatureType::MeasPolylineOpen
        | ZviFeatureType::SplineOpen
        | ZviFeatureType::MeasSplineOpen
            if p.len() >= 2 =>
        {
            vec![OmeShape::Polyline {
                points: p.clone(),
                the_z: none.0,
                the_t: none.1,
                the_c: none.2,
            }]
        }
        ZviFeatureType::PolylineClosed
        | ZviFeatureType::MeasPolylineClosed
        | ZviFeatureType::SplineClosed
        | ZviFeatureType::MeasSplineClosed
            if p.len() >= 2 =>
        {
            vec![OmeShape::Polygon {
                points: p.clone(),
                the_z: none.0,
                the_t: none.1,
                the_c: none.2,
            }]
        }
        ZviFeatureType::AlignedRectangle
        | ZviFeatureType::MeasAlignedRectangle
        | ZviFeatureType::Text
            if p.len() >= 3 =>
        {
            vec![
                OmeShape::Point {
                    x: p[0].0,
                    y: p[0].1,
                    the_z: none.0,
                    the_t: none.1,
                    the_c: none.2,
                },
                OmeShape::Rectangle {
                    x: p[0].0,
                    y: p[0].1,
                    width: p[2].0 - p[0].0,
                    height: p[2].1 - p[0].1,
                    the_z: none.0,
                    the_t: none.1,
                    the_c: none.2,
                },
            ]
        }
        ZviFeatureType::Rectangle | ZviFeatureType::MeasRectangle if p.len() >= 4 => {
            vec![OmeShape::Polygon {
                points: p.iter().take(4).copied().collect(),
                the_z: none.0,
                the_t: none.1,
                the_c: none.2,
            }]
        }
        ZviFeatureType::Ellipse | ZviFeatureType::MeasEllipse if p.len() >= 3 => {
            vec![OmeShape::Ellipse {
                x: (p[0].0 + p[2].0) / 2.0,
                y: (p[0].1 + p[2].1) / 2.0,
                radius_x: (p[2].0 - p[0].0) / 2.0,
                radius_y: (p[2].1 - p[0].1) / 2.0,
                the_z: none.0,
                the_t: none.1,
                the_c: none.2,
            }]
        }
        ZviFeatureType::Caliper
        | ZviFeatureType::MeasCaliper
        | ZviFeatureType::Distance
        | ZviFeatureType::MeasDistance
        | ZviFeatureType::MultipleCaliper
        | ZviFeatureType::MeasMultipleCaliper
        | ZviFeatureType::MultipleDistance
        | ZviFeatureType::MeasMultipleDistance => {
            let mut point_count = p.len();
            if matches!(
                shape.ty,
                ZviFeatureType::Caliper | ZviFeatureType::MeasCaliper
            ) {
                point_count = point_count.saturating_sub(2);
            }
            p.windows(2)
                .take(point_count.saturating_sub(1))
                .map(|w| OmeShape::Line {
                    x1: w[0].0,
                    y1: w[0].1,
                    x2: w[1].0,
                    y2: w[1].1,
                    the_z: none.0,
                    the_t: none.1,
                    the_c: none.2,
                })
                .collect()
        }
        ZviFeatureType::Angle3
        | ZviFeatureType::MeasAngle3
        | ZviFeatureType::Angle4
        | ZviFeatureType::MeasAngle4
            if p.len() >= 4 =>
        {
            vec![
                OmeShape::Line {
                    x1: p[0].0,
                    y1: p[0].1,
                    x2: p[1].0,
                    y2: p[1].1,
                    the_z: none.0,
                    the_t: none.1,
                    the_c: none.2,
                },
                OmeShape::Line {
                    x1: p[2].0,
                    y1: p[2].1,
                    x2: p[3].0,
                    y2: p[3].1,
                    the_z: none.0,
                    the_t: none.1,
                    the_c: none.2,
                },
            ]
        }
        ZviFeatureType::Length | ZviFeatureType::MeasLength if p.len() >= 6 => p
            .windows(2)
            .take(5)
            .step_by(2)
            .map(|w| OmeShape::Line {
                x1: w[0].0,
                y1: w[0].1,
                x2: w[1].0,
                y2: w[1].1,
                the_z: none.0,
                the_t: none.1,
                the_c: none.2,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn parse_zvi_roi_stream(data: &[u8]) -> Vec<OmeROI> {
    let mut s = Cursor::new(data);
    if s.len() < 18 {
        return Vec::new();
    }

    // ZeissZVIReader.parseROIs seeks to byte 2, reads the layer version, then
    // optionally reads a UTF-16LE BSTR layer name before the shape count.
    s.seek(2);
    let _layer_version = match s.read_i32() {
        Some(v) => v,
        None => return Vec::new(),
    };
    let tmp = s.pos;
    let layer_name = if s.read_u16() == Some(8) {
        s.seek(tmp);
        parse_zvi_roi_string(&mut s)
    } else {
        None
    };
    s.skip(8);
    let Some(roi_count) = s.read_u16() else {
        return Vec::new();
    };

    let mut shapes = Vec::new();
    while shapes.len() < roi_count as usize && s.pos < s.len().saturating_sub(8) {
        let mut signature = match s.read_u64() {
            Some(v) => v,
            None => break,
        };
        while signature != ZVI_ROI_SIGNATURE {
            if s.pos >= s.len() {
                break;
            }
            s.seek(s.pos.saturating_sub(6));
            signature = match s.read_u64() {
                Some(v) => v,
                None => break,
            };
        }
        if s.pos >= s.len() {
            break;
        }

        let roi_offset = s.pos.saturating_sub(8);
        s.seek(roi_offset + 26);
        let Some(length) = s.read_i32() else {
            break;
        };
        s.skip(length.max(0) as usize + 6);

        let Some(shape_attr_length) = s.read_i32() else {
            break;
        };
        let Some(shape_type) = s.read_i32() else {
            break;
        };
        let ty = ZviFeatureType::get(shape_type);
        if shape_attr_length < 32 {
            break;
        }

        s.skip(8);
        let (Some(_x1), Some(_y1), Some(_x2), Some(_y2)) =
            (s.read_i32(), s.read_i32(), s.read_i32(), s.read_i32())
        else {
            break;
        };
        if shape_attr_length >= 72 {
            s.skip(16 + 7 * 4);
        }
        if shape_attr_length >= 100 {
            s.skip(5 * 4);
        }
        if shape_attr_length >= 148 {
            s.skip(36 + 4 * 4);
        }
        if shape_attr_length >= 152 {
            s.skip(4);
        }
        if shape_attr_length >= 156 {
            s.skip(4);
        }

        let tmp = s.pos;
        let text = if s.read_u16() == Some(8) {
            s.seek(tmp);
            parse_zvi_roi_string(&mut s)
        } else if s.read_u16() == Some(8) {
            s.seek(tmp + 2);
            parse_zvi_roi_string(&mut s)
        } else {
            None
        };

        if s.pos + 8 > s.len() {
            break;
        }
        s.skip(4);
        let _tag_id = s.read_i32();

        if parse_zvi_roi_string(&mut s).is_none() {
            break;
        }
        let name = match parse_zvi_roi_string(&mut s) {
            Some(v) => (!v.is_empty()).then_some(v),
            None => break,
        };

        if s.pos + 20 > s.len() {
            break;
        }
        s.skip(4);
        let _handle_size = s.read_i32();
        s.skip(2);
        let Some(point_count) = s.read_i32() else {
            break;
        };
        s.skip(6);
        if point_count < 0 || s.pos + (16 * point_count as usize) > s.len() {
            break;
        }

        let mut points = Vec::with_capacity(point_count as usize);
        for _ in 0..point_count {
            let (Some(x), Some(y)) = (s.read_f64(), s.read_f64()) else {
                points.clear();
                break;
            };
            points.push((x, y));
        }
        if points.len() == point_count as usize {
            shapes.push(ZviParsedShape {
                ty,
                name,
                text,
                points,
            });
        }
    }

    let mut rois = Vec::new();
    for shape in shapes {
        if matches!(shape.ty, ZviFeatureType::Unknown | ZviFeatureType::Lut) {
            continue;
        }
        let ome_shapes = zvi_points_to_shape(&shape);
        if !ome_shapes.is_empty() {
            rois.push(OmeROI {
                id: Some(create_lsid("ROI", &[rois.len()])),
                name: shape.name.or(shape.text),
                shapes: ome_shapes,
            });
        }
    }
    if rois.is_empty() && layer_name.is_some() {
        return Vec::new();
    }
    rois
}

/// Result of parsing a single ZVI item (image) stream.
struct ParsedItem {
    z: u32,
    c: u32,
    t: u32,
    tile: u32,
    size_x: u32,
    size_y: u32,
    bpp: u32,
    data_offset: usize,
    is_zlib: bool,
    is_jpeg: bool,
}

/// Parse one ZVI item ("/Image/Item(N)/CONTENTS") stream.
///
/// Port of the per-image parsing in ZeissZVIReader.fillMetadataPass1.
fn parse_zvi_item(data: &[u8], stream_len: usize) -> Result<Option<ParsedItem>> {
    // Image streams smaller than this are metadata-only and skipped by Java.
    if stream_len <= 1024 {
        return Ok(None);
    }

    let mut s = Cursor::new(data);

    // 11 leading tags.
    for _ in 0..11 {
        skip_next_tag(&mut s);
    }

    s.skip(2);
    let Some(len_raw) = s.read_i32() else {
        return Ok(None);
    };
    let len = len_raw - 20;
    s.skip(8);

    let Some(zidx) = s.read_i32() else {
        return Ok(None);
    };
    let Some(cidx) = s.read_i32() else {
        return Ok(None);
    };
    let Some(tidx) = s.read_i32() else {
        return Ok(None);
    };
    s.skip(4);
    let Some(tile_index) = s.read_i32() else {
        return Ok(None);
    };

    // skipBytes(len - 8)
    let skip_len = (len - 8).max(0) as usize;
    s.skip(skip_len);

    // 5 more tags.
    for _ in 0..5 {
        skip_next_tag(&mut s);
    }

    s.skip(4);
    let Some(size_x) = s.read_i32() else {
        return Ok(None);
    };
    let Some(size_y) = s.read_i32() else {
        return Ok(None);
    };
    s.skip(4);
    let Some(bpp) = s.read_i32() else {
        return Ok(None);
    };
    if size_x <= 0 || size_y <= 0 {
        return Err(BioFormatsError::Format(format!(
            "ZVI: invalid non-positive image dimensions {size_x}x{size_y}"
        )));
    }
    // Java only uses this field to initialize the global bpp once. Later item
    // streams still carry a 4-byte field here, but Bio-Formats skips it without
    // validation; some real Zeiss stacked/mosaic files contain non-bpp values in
    // those later streams.
    // Java skips exactly one 4-byte field here (ZeissZVIReader.java:311) before
    // reading `valid`. Our pixel-data offset = filePointer - 4 depends on this
    // being a single skip; a second skip would push the offset 4 bytes too far.
    s.skip(4);

    let Some(valid) = s.read_i32() else {
        return Ok(None);
    };
    let check_bytes = data.get(s.pos..s.pos + 4).unwrap_or(&[]);
    let check = String::from_utf8_lossy(check_bytes).trim().to_string();
    s.skip(4);

    let is_zlib = (valid == 0 || valid == 1) && check == "WZL";
    let is_jpeg = (valid == 0 || valid == 1) && !is_zlib;

    // Pixel data offset = filePointer - 4 (+8 for zlib).
    let mut data_offset = s.pos.saturating_sub(4);
    if is_zlib {
        data_offset += 8;
    }

    if !is_zlib && !is_jpeg {
        // Validate the offset is in range, but tolerate a plane that is a few
        // bytes short of the declared size: ZeissZVIReader.openBytes reads into
        // a pre-zeroed buffer via readPlane, so a stream that ends a few bytes
        // early (seen in real Zeiss exports) simply leaves the tail zero rather
        // than failing. open_bytes mirrors this by zero-padding and uses the
        // global bpp chosen from the first valid image stream; later item streams
        // may contain non-bpp values in the same field.
        if data_offset > stream_len {
            return Err(BioFormatsError::InvalidData(
                "ZVI: pixel data offset is past end of stream".into(),
            ));
        }
    }

    Ok(Some(ParsedItem {
        z: zidx.max(0) as u32,
        c: cidx.max(0) as u32,
        t: tidx.max(0) as u32,
        tile: tile_index.max(0) as u32,
        size_x: size_x.max(0) as u32,
        size_y: size_y.max(0) as u32,
        bpp: bpp.max(0) as u32,
        data_offset,
        is_zlib,
        is_jpeg,
    }))
}

fn parse_zvi_item_stream<R: Read + Seek>(stream: &mut R) -> Result<Option<ParsedItem>> {
    let stream_len = stream.seek(SeekFrom::End(0)).map_err(BioFormatsError::Io)? as usize;
    stream
        .seek(SeekFrom::Start(0))
        .map_err(BioFormatsError::Io)?;

    let initial_len = stream_len.min(64 * 1024);
    let mut data = vec![0u8; initial_len];
    stream.read_exact(&mut data).map_err(BioFormatsError::Io)?;
    match parse_zvi_item(&data, stream_len) {
        Ok(Some(item)) => Ok(Some(item)),
        Ok(None) if initial_len < stream_len => {
            stream
                .seek(SeekFrom::Start(0))
                .map_err(BioFormatsError::Io)?;
            data.resize(stream_len, 0);
            stream.read_exact(&mut data).map_err(BioFormatsError::Io)?;
            parse_zvi_item(&data, stream_len)
        }
        other => other,
    }
}

fn parse_zvi(
    path: &Path,
    parse_overlays: bool,
) -> Result<(
    ImageMetadata,
    Vec<ZviPlane>,
    usize,
    bool,
    usize,
    ZviOmeInfo,
    cfb::CompoundFile<File>,
)> {
    let mut comp =
        cfb::open(path).map_err(|e| BioFormatsError::Format(format!("ZVI CFB open error: {e}")))?;

    // ── Enumerate image item streams ─────────────────────────────────────────
    // ZeissZVIReader matches stream names case-insensitively: it uppercases the
    // path and keeps those ending in "CONTENTS" that live under an "ITEM(n)"
    // directory (ZeissZVIReader.java:393-404). The cfb container preserves the
    // original on-disk casing (e.g. ".../Item(0)/Contents"), so we must match
    // without regard to case.
    let item_num = |s: &str| -> u32 {
        // Extract the index from the parent "Item(n)" directory, mirroring
        // getImageNumber (case-insensitive "ITEM").
        let dir = parent_dir(s);
        let upper = dir.to_ascii_uppercase();
        if upper.contains("ITEM") {
            if let Some(open) = dir.find('(') {
                let after = &dir[open + 1..];
                return after
                    .split(')')
                    .next()
                    .and_then(|n| n.trim().parse().ok())
                    .unwrap_or(0);
            }
        }
        0
    };
    let is_item_contents = |p: &str| -> bool {
        // relPath must be exactly CONTENTS (case-insensitive) and the immediate
        // parent dir must be "Image" or contain "ITEM" (ZeissZVIReader:240-266).
        let upper = p.to_ascii_uppercase();
        if !upper.ends_with("/CONTENTS") {
            return false;
        }
        let dir = parent_dir(p).to_ascii_uppercase();
        dir == "IMAGE" || dir.contains("ITEM")
    };
    let mut item_paths: Vec<String> = comp
        .walk()
        .filter_map(|entry| {
            if !entry.is_stream() {
                return None;
            }
            let p = entry.path().to_string_lossy().to_string();
            if is_item_contents(&p) {
                Some(p)
            } else {
                None
            }
        })
        .collect();

    // Numeric sort by item index.
    item_paths.sort_by_key(|p| item_num(p));

    let mut planes: Vec<ZviPlane> = Vec::with_capacity(item_paths.len());
    let mut series_metadata = HashMap::new();
    let mut ome_info = ZviOmeInfo::default();
    let mut c_index: i32 = -1;
    let mut bpp: u32 = 0;
    let mut size_x: u32 = 0;
    let mut size_y: u32 = 0;
    let mut is_jpeg_global = false;

    for stream_path in item_paths {
        let mut stream = match comp.open_stream(&stream_path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let item = match parse_zvi_item_stream(&mut stream) {
            Ok(Some(item)) => item,
            Ok(None) => continue,
            Err(_) => {
                let mut stream = match comp.open_stream(&stream_path) {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let mut data = Vec::new();
                if stream.read_to_end(&mut data).is_err() {
                    continue;
                }
                match parse_zvi_item(&data, data.len())? {
                    Some(item) => item,
                    None => continue,
                }
            }
        };
        let item = if item.data_offset > 64 * 1024 {
            // Extremely large item headers are rare; reopen and parse the whole
            // stream so the result is still derived from the same bytes as the
            // original full-read implementation.
            let mut stream = match comp.open_stream(&stream_path) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let mut data = Vec::new();
            if stream.read_to_end(&mut data).is_err() {
                continue;
            }
            let Some(item) = parse_zvi_item(&data, data.len())? else {
                continue;
            };
            item
        } else {
            item
        };

        // bpp / sizeX / sizeY are taken from the first valid image stream.
        if bpp == 0 {
            if !matches!(item.bpp, 1 | 2 | 3 | 6) {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "ZVI: unsupported bytes-per-pixel value {}",
                    item.bpp
                )));
            }
            bpp = item.bpp;
        }
        if size_x == 0 {
            size_x = item.size_x;
        }
        if size_y == 0 {
            size_y = item.size_y;
        }
        if item.is_jpeg {
            is_jpeg_global = true;
        }

        // Keep every image stream, including tiles. ZeissZVIReader records the
        // tile index in coordinates[i][3] and exposes each tile as a series
        // rather than stitching them into a single plane.
        let image_num = item_num(&stream_path) as usize;
        planes.push(ZviPlane {
            stream_path,
            image_num,
            z: item.z,
            c: item.c,
            t: item.t,
            tile: item.tile,
            data_offset: item.data_offset,
            is_zlib: item.is_zlib,
            is_jpeg: item.is_jpeg,
        });
    }

    for plane in &planes {
        let image_num = item_num(&plane.stream_path) as usize;
        // Derive the sibling Tags stream from the item's own "…/Contents" path,
        // preserving on-disk casing (e.g. ".../Item(0)/Tags/Contents").
        let sp = &plane.stream_path;
        let tag_path = match sp.rfind('/') {
            Some(slash) => {
                let (dir, contents) = sp.split_at(slash);
                format!("{dir}/Tags{contents}")
            }
            None => continue,
        };
        if let Ok(mut stream) = comp.open_stream(&tag_path) {
            let mut data = Vec::new();
            if stream.read_to_end(&mut data).is_ok() {
                series_metadata.extend(parse_zvi_tag_stream(&data, image_num));
                harvest_zvi_ome_tags(&data, &mut ome_info, &mut c_index, Some(image_num));
            }
        }
    }

    // Physical pixel sizes live in the top-level "/Image/Tags/Contents" stream
    // (dirName "Tags"), not the per-item Tags. BaseZeissReader parses that stream
    // too; harvest it for the Scale Factor tags. Use a throwaway channel index so
    // any "Image Channel Index" tag here cannot disturb the per-channel mapping.
    if let Ok(mut stream) = comp.open_stream("/Image/Tags/Contents") {
        let mut data = Vec::new();
        if stream.read_to_end(&mut data).is_ok() {
            let mut scratch_index: i32 = -1;
            harvest_zvi_ome_tags(&data, &mut ome_info, &mut scratch_index, None);
        }
    }

    if parse_overlays {
        let mut shape_paths: Vec<String> = comp
            .walk()
            .filter_map(|entry| {
                if !entry.is_stream() {
                    return None;
                }
                let p = entry.path().to_string_lossy().to_string();
                if p.to_ascii_uppercase().ends_with("/CONTENTS")
                    && parent_dir(&p).eq_ignore_ascii_case("Shapes")
                    && p.to_ascii_uppercase().contains("ITEM")
                {
                    Some(p)
                } else {
                    None
                }
            })
            .collect();
        shape_paths.sort_by_key(|p| item_num(p));
        for shape_path in shape_paths {
            if let Ok(mut stream) = comp.open_stream(&shape_path) {
                let mut data = Vec::new();
                if stream.read_to_end(&mut data).is_ok() {
                    ome_info.rois.extend(parse_zvi_roi_stream(&data));
                }
            }
        }
        for (ri, roi) in ome_info.rois.iter_mut().enumerate() {
            roi.id = Some(create_lsid("ROI", &[ri]));
        }
    }

    if planes.is_empty() {
        return Err(BioFormatsError::Format("ZVI: no image planes found".into()));
    }

    // ── Pixel type from bpp (BaseZeissReader.fillMetadataPass6) ───────────────
    //   bpp 1|3 -> UINT8, bpp 2|6 -> UINT16; isJPEG forces UINT8.
    //   RGB when bpp % 3 == 0.
    let is_rgb = bpp != 0 && bpp % 3 == 0;
    let pixel_type = if is_jpeg_global {
        PixelType::Uint8
    } else if bpp == 1 || bpp == 3 {
        PixelType::Uint8
    } else if bpp == 2 || bpp == 6 {
        PixelType::Uint16
    } else {
        PixelType::Uint8
    };
    let bytes_per_sample = pixel_type.bytes_per_sample();
    // Stored bytes per pixel including RGB channels (matches Java `bpp`).
    let bytes_per_pixel = if is_rgb {
        bytes_per_sample * 3
    } else {
        bytes_per_sample
    };

    // ── Derive dimension sizes from distinct indices ──────────────────────────
    // BaseZeissReader.fillMetadataPass2: sizeZ/sizeT/sizeC = the number of
    // distinct z/t/channel index values (collected across all tiles, since the
    // per-tile coordinate sets are identical).
    let distinct = |sel: &dyn Fn(&ZviPlane) -> u32| -> u32 {
        let mut v: Vec<u32> = planes.iter().map(sel).collect();
        v.sort_unstable();
        v.dedup();
        v.len() as u32
    };
    let size_z = distinct(&|p| p.z);
    let logical_c = distinct(&|p| p.c);
    let size_t = distinct(&|p| p.t);
    let mut size_c = logical_c;
    if is_rgb {
        size_c *= 3;
    }

    // Number of tiles = total planes / per-tile plane count, with each tile a
    // separate series (ZeissZVIReader: totalTiles = offsets.length/imageCount).
    let image_count = size_z * logical_c * size_t;
    let tile_count = if image_count > 0 {
        (planes.len() as u32 / image_count).max(1) as usize
    } else {
        1
    };

    // ── Dimension order (BaseZeissReader.fillMetadataPass4:236-255) ───────────
    // Java builds the order from the per-plane coordinate deltas, walked in the
    // original (item-number) stream order: start with "XY", prepend 'C' for RGB,
    // then append Z/C/T the first time consecutive planes increase along that
    // axis, and finally run makeSaneDimensionOrder to fill any missing axes.
    // `planes` is currently in item-number order, matching `coordinates`.
    let mut order = String::from("XY");
    if is_rgb {
        order.push('C');
    }
    for w in planes.windows(2) {
        let (a, b) = (&w[0], &w[1]);
        if b.z > a.z && !order.contains('Z') {
            order.push('Z');
        }
        if b.c > a.c && !order.contains('C') {
            order.push('C');
        }
        if b.t > a.t && !order.contains('T') {
            order.push('T');
        }
    }
    let dimension_order = make_sane_dimension_order(&order);

    // Sort planes so each tile's planes form a contiguous block ordered by the
    // derived dimension order (fastest-varying axis last in the sort key), so
    // `plane_index` maps to the same plane Java resolves via getZCTCoords.
    let axis_key = |p: &ZviPlane, axis: char| -> u32 {
        match axis {
            'Z' => p.z,
            'C' => p.c,
            'T' => p.t,
            _ => 0,
        }
    };
    // Build the (outer..inner) axis list from the order string (skip X, Y).
    let axes: Vec<char> = dimension_order_axes(dimension_order);
    planes.sort_by(|a, b| {
        let mut ord = a.tile.cmp(&b.tile);
        // outer-most axis first; the last axis in `axes` varies fastest, so
        // compare from outermost (axes.last) to innermost (axes.first) by
        // iterating the reversed slice as major→minor.
        for &axis in axes.iter().rev() {
            ord = ord.then_with(|| axis_key(a, axis).cmp(&axis_key(b, axis)));
        }
        ord
    });

    let meta = ImageMetadata {
        size_x,
        size_y,
        size_z,
        size_c,
        size_t,
        pixel_type,
        bits_per_pixel: (bytes_per_sample * 8) as u16,
        image_count,
        dimension_order,
        is_rgb,
        is_interleaved: true,
        is_indexed: !is_rgb && !ome_info.channel_colors.is_empty(),
        is_little_endian: true,
        resolution_count: 1,
        thumbnail: false,
        series_metadata,
        lookup_table: None,
        modulo_z: None,
        modulo_c: None,
        modulo_t: None,
    };

    Ok((
        meta,
        planes,
        bytes_per_pixel,
        is_rgb,
        tile_count,
        ome_info,
        comp,
    ))
}

/// Decode pixel data from a ZVI plane stream starting at `data_offset`.
///
/// Port of ZeissZVIReader.openBytes pixel-decode dispatch: the pixel data offset
/// is the precomputed `offsets[index]` (already advanced past the zlib WZL
/// sub-header when `is_zlib`), and the compression flags select the codec.
fn decode_plane_data(data: &[u8], plane: &ZviPlane) -> Result<Vec<u8>> {
    let payload = data.get(plane.data_offset..).ok_or_else(|| {
        BioFormatsError::Format("ZVI: pixel data offset is past end of stream".into())
    })?;

    if plane.is_jpeg {
        return crate::common::codec::decompress_jpeg(payload)
            .map_err(|e| BioFormatsError::Format(format!("ZVI JPEG decode: {e}")));
    }

    if plane.is_zlib {
        let mut decoder = flate2::read::ZlibDecoder::new(payload);
        let mut out = Vec::new();
        decoder
            .read_to_end(&mut out)
            .map_err(|e| BioFormatsError::Format(format!("ZVI zlib decode: {e}")))?;
        return Ok(out);
    }

    // Raw uncompressed.
    Ok(payload.to_vec())
}

impl FormatReader for ZeissZviReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("zvi"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // Java ZeissZVIReader.isThisType(RandomAccessInputStream) reads a
        // big-endian int and compares only the first four OLE2 magic bytes.
        matches!(header.get(..4), Some([0xd0, 0xcf, 0x11, 0xe0]))
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let parse_overlays = self.metadata_options.level == MetadataLevel::All;
        let (meta, planes, bpp, is_rgb, tile_count, ome_info, comp) =
            parse_zvi(path, parse_overlays)?;
        self.meta = Some(meta);
        self.planes = planes;
        self.comp = Some(comp);
        self.path = Some(path.to_path_buf());
        self.bytes_per_pixel = bpp;
        self.is_rgb = is_rgb;
        self.tile_count = tile_count.max(1);
        self.current_series = 0;
        self.ome_info = ome_info;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.comp = None;
        self.meta = None;
        self.planes.clear();
        self.tile_count = 1;
        self.current_series = 0;
        self.ome_info = ZviOmeInfo::default();
        Ok(())
    }

    fn set_metadata_options(&mut self, options: MetadataOptions) {
        self.metadata_options = options;
    }

    fn series_count(&self) -> usize {
        if self.meta.is_some() {
            self.tile_count.max(1)
        } else {
            0
        }
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if s >= self.series_count() {
            return Err(BioFormatsError::SeriesOutOfRange(s));
        }
        self.current_series = s;
        Ok(())
    }

    fn series(&self) -> usize {
        self.current_series
    }

    fn metadata(&self) -> &ImageMetadata {
        self.meta
            .as_ref()
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn resolution_count(&self) -> usize {
        1
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if level != 0 {
            Err(BioFormatsError::Format(format!(
                "ZVI: resolution {level} out of range"
            )))
        } else {
            Ok(())
        }
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self
            .meta
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?
            .clone();
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }

        // Planes are stored contiguously per tile (series), so the active
        // series offsets into the global plane list. This mirrors how
        // ZeissZVIReader resolves the plane by matching coordinates[i][3]
        // (the tile index) against getSeries().
        let image_count = meta.image_count;
        let global_index = (self.current_series as u32)
            .checked_mul(image_count)
            .and_then(|base| base.checked_add(plane_index))
            .ok_or(BioFormatsError::PlaneOutOfRange(plane_index))?;

        let plane = self
            .planes
            .get(global_index as usize)
            .ok_or(BioFormatsError::PlaneOutOfRange(plane_index))?;
        let stream_path = plane.stream_path.clone();
        let plane = ZviPlane {
            stream_path: stream_path.clone(),
            image_num: plane.image_num,
            z: plane.z,
            c: plane.c,
            t: plane.t,
            tile: plane.tile,
            data_offset: plane.data_offset,
            is_zlib: plane.is_zlib,
            is_jpeg: plane.is_jpeg,
        };

        let comp = self.comp.as_mut().ok_or(BioFormatsError::NotInitialized)?;
        let mut stream = comp
            .open_stream(&stream_path)
            .map_err(|e| BioFormatsError::Format(format!("ZVI stream {stream_path}: {e}")))?;
        let mut data = Vec::new();
        stream.read_to_end(&mut data).map_err(BioFormatsError::Io)?;

        let mut pixels = decode_plane_data(&data, &plane)?;

        // Normalise to exactly one plane's worth of bytes. Java reads
        // sizeX * sizeY * pixel bytes into a pre-zeroed buffer via readPlane, so
        // a stream that ends a few bytes early leaves the tail zero rather than
        // failing; mirror that by zero-padding a short decode.
        let plane_bytes = meta.size_x as usize * meta.size_y as usize * self.bytes_per_pixel;
        if pixels.len() > plane_bytes {
            pixels.truncate(plane_bytes);
        } else if pixels.len() < plane_bytes {
            pixels.resize(plane_bytes, 0);
        }

        // BGR storage: reverse channel bytes in groups for RGB images (but not
        // for JPEG, which the codec already returns in RGB order). Matches
        // ZeissZVIReader.openBytes: swap the first sample with the third per
        // pixel, where each sample is `bytes` wide and the pixel stride is bpp.
        if self.is_rgb && !plane.is_jpeg && self.bytes_per_pixel >= 3 {
            let bpp = self.bytes_per_pixel;
            let bytes = bpp / 3;
            let mut i = 0;
            while i + bpp <= pixels.len() {
                for k in 0..bytes {
                    pixels.swap(i + k, i + 2 * bytes + k);
                }
                i += bpp;
            }
        }

        Ok(pixels)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let meta = self
            .meta
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?
            .clone();
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let bps = meta.pixel_type.bytes_per_sample();
        let samples_per_pixel = self
            .bytes_per_pixel
            .checked_div(bps)
            .filter(|samples| {
                *samples > 0 && samples.checked_mul(bps) == Some(self.bytes_per_pixel)
            })
            .ok_or_else(|| BioFormatsError::Format("ZVI pixel size is inconsistent".into()))?;
        let x2 = x
            .checked_add(w)
            .ok_or_else(|| BioFormatsError::Format("ZVI region width overflows".into()))?;
        let y2 = y
            .checked_add(h)
            .ok_or_else(|| BioFormatsError::Format("ZVI region height overflows".into()))?;
        if w == 0 || h == 0 || x2 > meta.size_x || y2 > meta.size_y {
            return Err(BioFormatsError::Format(
                "ZVI region is outside image bounds".into(),
            ));
        }

        let image_count = meta.image_count;
        let global_index = (self.current_series as u32)
            .checked_mul(image_count)
            .and_then(|base| base.checked_add(plane_index))
            .ok_or(BioFormatsError::PlaneOutOfRange(plane_index))?;
        let plane = self
            .planes
            .get(global_index as usize)
            .ok_or(BioFormatsError::PlaneOutOfRange(plane_index))?;

        if !plane.is_jpeg && !plane.is_zlib {
            let comp = self.comp.as_mut().ok_or(BioFormatsError::NotInitialized)?;
            let mut stream = comp.open_stream(&plane.stream_path).map_err(|e| {
                BioFormatsError::Format(format!("ZVI stream {}: {e}", plane.stream_path))
            })?;

            let src_row_bytes = meta.size_x as usize * self.bytes_per_pixel;
            let dst_row_bytes = w as usize * self.bytes_per_pixel;
            let plane_bytes = src_row_bytes
                .checked_mul(meta.size_y as usize)
                .ok_or_else(|| BioFormatsError::Format("ZVI plane byte count overflows".into()))?;
            let mut out = vec![0u8; dst_row_bytes * h as usize];

            for row in 0..h as usize {
                let src = plane
                    .data_offset
                    .checked_add((y as usize + row) * src_row_bytes)
                    .and_then(|off| off.checked_add(x as usize * self.bytes_per_pixel))
                    .ok_or_else(|| BioFormatsError::Format("ZVI region offset overflows".into()))?;
                if src < plane.data_offset + plane_bytes {
                    let remaining = plane.data_offset + plane_bytes - src;
                    let to_read = dst_row_bytes.min(remaining);
                    stream
                        .seek(SeekFrom::Start(src as u64))
                        .map_err(BioFormatsError::Io)?;
                    let dst = row * dst_row_bytes;
                    read_zero_padded(&mut stream, &mut out[dst..dst + to_read])
                        .map_err(BioFormatsError::Io)?;
                }
            }

            if self.is_rgb && self.bytes_per_pixel >= 3 {
                let bpp = self.bytes_per_pixel;
                let bytes = bpp / 3;
                let mut i = 0;
                while i + bpp <= out.len() {
                    for k in 0..bytes {
                        out.swap(i + k, i + 2 * bytes + k);
                    }
                    i += bpp;
                }
            }
            return Ok(out);
        }

        let full = self.open_bytes(plane_index)?;
        crop_full_plane("ZVI", &full, &meta, samples_per_pixel, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        use crate::common::ome_metadata::OmeMetadata;
        let meta = self.meta.as_ref()?;
        let mut ome = OmeMetadata::from_image_metadata(meta);
        let info = &self.ome_info;
        let img = ome.images.get_mut(0)?;

        // Image name: BaseZeissReader only sets an explicit name ("Tile #N") for
        // multi-series files; for a single series Java falls back to the file's
        // base name (with extension), e.g. "fig3d_wt_sting_cd31.zvi".
        if self.tile_count > 1 {
            img.name = Some(format!("Tile #{}", self.current_series + 1));
        } else if let Some(path) = &self.path {
            img.name = path
                .file_name()
                .and_then(|n| n.to_str())
                .map(str::to_string);
        }

        img.description = info.image_description.clone();
        img.acquisition_date = info.acquisition_date.clone();
        img.physical_size_x = info.physical_size_x;
        img.physical_size_y = info.physical_size_y;
        img.physical_size_z = info.physical_size_z;

        // Per-channel name / emission / excitation. The raw channel-index values
        // ("Image Channel Index" tags) need not be 0..N-1 (this file uses 0,2,3),
        // so — like BaseZeissReader — OME channel i takes the i-th value when the
        // recorded channel-name keys are sorted ascending (channelKeys[i]).
        let mut channel_keys: Vec<u32> = info.channel_names.keys().copied().collect();
        channel_keys.extend(info.channel_colors.keys().copied());
        channel_keys.extend(info.emission.keys().copied());
        channel_keys.extend(info.excitation.keys().copied());
        channel_keys.extend(info.detector_gain.keys().copied());
        channel_keys.extend(info.detector_offset.keys().copied());
        channel_keys.sort_unstable();
        channel_keys.dedup();
        for (ci, ch) in img.channels.iter_mut().enumerate() {
            let Some(&key) = channel_keys.get(ci) else {
                break;
            };
            if let Some(name) = info.channel_names.get(&key) {
                ch.name = Some(name.clone());
            }
            ch.emission_wavelength = info.emission.get(&key).copied();
            ch.excitation_wavelength = info.excitation.get(&key).copied();
            if let Some(&color) = info.channel_colors.get(&key) {
                let red = (color & 0xff) as u8;
                let green = ((color >> 8) & 0xff) as u8;
                let blue = ((color >> 16) & 0xff) as u8;
                ch.color = Some(u32::from_be_bytes([red, green, blue, 0xff]) as i32);
            }
            ch.detector_ref = Some(create_lsid("Detector", &[0, ci]));
            ch.detector_settings_gain = info.detector_gain.get(&key).copied();
            ch.detector_settings_offset = info.detector_offset.get(&key).copied();
        }

        img.planes.clear();
        let base = self.current_series.checked_mul(meta.image_count as usize)?;
        let first_camera_time = self
            .planes
            .get(base)
            .and_then(|plane| info.camera_time.get(&plane.image_num))
            .copied()
            .or_else(|| info.camera_time.values().copied().min_by(f64::total_cmp));
        for plane_index in 0..meta.image_count {
            let plane = self.planes.get(base + plane_index as usize)?;
            let image_num = plane.image_num;
            let exposure_time = info.exposure_time.get(&image_num).copied();
            let delta_t = match (info.camera_time.get(&image_num).copied(), first_camera_time) {
                (Some(stamp), Some(first)) => Some((stamp - first) * 86_400.0),
                _ => None,
            };
            let position_x = info.stage_x.get(&image_num).copied();
            let position_y = info.stage_y.get(&image_num).copied();
            if exposure_time.is_some()
                || delta_t.is_some()
                || position_x.is_some()
                || position_y.is_some()
            {
                img.planes.push(OmePlane {
                    the_z: plane.z,
                    the_c: plane.c,
                    the_t: plane.t,
                    delta_t,
                    exposure_time,
                    position_x,
                    position_y,
                    position_z: None,
                });
            }
        }

        let has_objective = info.objective_magnification.is_some()
            || info.objective_lens_na.is_some()
            || info.objective_immersion.is_some()
            || info.objective_working_distance.is_some()
            || info.objective_id.is_some()
            || info.objective_correction.is_some();
        // BaseZeissReader creates one detector per logical ZVI channel. Gain and
        // offset tags only enrich those detector records when present.
        let has_detector = !img.channels.is_empty();
        if has_objective || has_detector {
            if ome.instruments.is_empty() {
                ome.instruments.push(OmeInstrument {
                    id: Some(create_lsid("Instrument", &[0])),
                    ..Default::default()
                });
            }
            img.instrument_ref = Some(0);
            if has_objective {
                if ome.instruments[0].objectives.is_empty() {
                    ome.instruments[0].objectives.push(Default::default());
                }
                let objective = &mut ome.instruments[0].objectives[0];
                objective.id = info
                    .objective_id
                    .clone()
                    .or_else(|| Some(create_lsid("Objective", &[0, 0])));
                objective.nominal_magnification = info.objective_magnification;
                objective.lens_na = info.objective_lens_na;
                objective.immersion = info.objective_immersion.clone();
                objective.correction = info
                    .objective_correction
                    .clone()
                    .or_else(|| Some("Other".to_string()));
                objective.working_distance = info.objective_working_distance;
                img.objective_ref = Some(0);
            }
            if has_detector {
                let detector_count = img.channels.len();
                let detectors = &mut ome.instruments[0].detectors;
                while detectors.len() < detector_count {
                    let di = detectors.len();
                    let key = channel_keys.get(di).copied().unwrap_or(di as u32);
                    detectors.push(OmeDetector {
                        id: Some(create_lsid("Detector", &[0, di])),
                        detector_type: Some("Other".to_string()),
                        gain: info.detector_gain.get(&key).copied(),
                        offset: info.detector_offset.get(&key).copied(),
                        ..Default::default()
                    });
                }
                for (ci, channel) in img.channels.iter_mut().enumerate() {
                    if channel.detector_ref.is_none() {
                        channel.detector_ref = Some(create_lsid("Detector", &[0, ci]));
                    }
                }
            }
        }

        if info.experimenter_first_name.is_some()
            || info.experimenter_last_name.is_some()
            || info.experimenter_institution.is_some()
        {
            ome.experimenters.push(OmeExperimenter {
                id: Some(create_lsid("Experimenter", &[0])),
                first_name: info.experimenter_first_name.clone(),
                last_name: info.experimenter_last_name.clone(),
                institution: info.experimenter_institution.clone(),
                ..Default::default()
            });
        }

        ome.rois.extend(info.rois.clone());

        Some(ome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("bioformats_zvi_{nanos}_{name}.zvi"))
    }

    #[test]
    fn zvi_byte_detection_matches_java_ole2_prefix() {
        let reader = ZeissZviReader::new();
        assert!(reader.is_this_type_by_bytes(&[0xd0, 0xcf, 0x11, 0xe0]));
        assert!(reader.is_this_type_by_bytes(&[0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1,]));
        assert!(!reader.is_this_type_by_bytes(&[0xd0, 0xcf, 0x11]));
        assert!(!reader.is_this_type_by_bytes(&[0xe0, 0x11, 0xcf, 0xd0]));
    }

    /// Build one ZVI item ("/Image/Item(N)/CONTENTS") stream carrying the given
    /// z/c/t/tile indices and a single uncompressed 1x1 UINT8 pixel value. The
    /// byte layout matches `parse_zvi_item` (and the Java reference).
    fn build_item_with_bpp(
        z: i32,
        c: i32,
        t: i32,
        tile: i32,
        pixel: u8,
        pad: i32,
        bpp: i32,
    ) -> Vec<u8> {
        let mut item: Vec<u8> = Vec::new();
        // 11 leading VT_EMPTY tags (type 0, 2 bytes each).
        item.extend_from_slice(&[0u8; 22]);
        // skip(2)
        item.extend_from_slice(&[0u8; 2]);
        // len = readInt() - 20; pad skip(len-8) past the 1024-byte cutoff.
        let len_raw: i32 = pad + 28;
        item.extend_from_slice(&len_raw.to_le_bytes());
        // skip(8)
        item.extend_from_slice(&[0u8; 8]);
        item.extend_from_slice(&z.to_le_bytes());
        item.extend_from_slice(&c.to_le_bytes());
        item.extend_from_slice(&t.to_le_bytes());
        item.extend_from_slice(&[0u8; 4]); // skip(4)
        item.extend_from_slice(&tile.to_le_bytes());
        item.extend_from_slice(&vec![0u8; pad as usize]); // skip(len - 8)
                                                          // 5 more VT_EMPTY tags.
        item.extend_from_slice(&[0u8; 10]);
        // skip(4)
        item.extend_from_slice(&[0u8; 4]);
        item.extend_from_slice(&1i32.to_le_bytes()); // sizeX
        item.extend_from_slice(&1i32.to_le_bytes()); // sizeY
        item.extend_from_slice(&[0u8; 4]); // skip(4)
        item.extend_from_slice(&bpp.to_le_bytes()); // bpp field
        item.extend_from_slice(&[0u8; 4]); // skip(4) before valid
        item.extend_from_slice(&2i32.to_le_bytes()); // valid=2 -> uncompressed
        item.extend_from_slice(&[pixel, 0, 0, 0]); // check / first-pixel region
        item
    }

    fn build_item_with_pad(z: i32, c: i32, t: i32, tile: i32, pixel: u8, pad: i32) -> Vec<u8> {
        build_item_with_bpp(z, c, t, tile, pixel, pad, 1)
    }

    fn build_item(z: i32, c: i32, t: i32, tile: i32, pixel: u8) -> Vec<u8> {
        build_item_with_pad(z, c, t, tile, pixel, 1100)
    }

    fn tag_i32(tag_id: u32, value: i32) -> Vec<u8> {
        let mut tag = Vec::new();
        tag.extend_from_slice(&3u16.to_le_bytes());
        tag.extend_from_slice(&value.to_le_bytes());
        tag.extend_from_slice(&[0u8; 2]);
        tag.extend_from_slice(&tag_id.to_le_bytes());
        tag.extend_from_slice(&[0u8; 6]);
        tag
    }

    fn tag_f64(tag_id: u32, value: f64) -> Vec<u8> {
        let mut tag = Vec::new();
        tag.extend_from_slice(&5u16.to_le_bytes());
        tag.extend_from_slice(&value.to_le_bytes());
        tag.extend_from_slice(&[0u8; 2]);
        tag.extend_from_slice(&tag_id.to_le_bytes());
        tag.extend_from_slice(&[0u8; 6]);
        tag
    }

    fn tag_string(tag_id: u32, value: &str) -> Vec<u8> {
        let mut tag = Vec::new();
        tag.extend_from_slice(&8u16.to_le_bytes());
        tag.extend_from_slice(&(value.len() as u32).to_le_bytes());
        tag.extend_from_slice(value.as_bytes());
        tag.extend_from_slice(&[0u8; 2]);
        tag.extend_from_slice(&tag_id.to_le_bytes());
        tag.extend_from_slice(&[0u8; 6]);
        tag
    }

    fn build_tag_stream(tags: Vec<Vec<u8>>) -> Vec<u8> {
        let mut stream = vec![0u8; 8];
        stream.extend_from_slice(&(tags.len() as u32).to_le_bytes());
        for tag in tags {
            stream.extend_from_slice(&tag);
        }
        stream
    }

    struct OneByteReads {
        data: Vec<u8>,
        pos: usize,
    }

    impl Read for OneByteReads {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.pos >= self.data.len() || buf.is_empty() {
                return Ok(0);
            }
            buf[0] = self.data[self.pos];
            self.pos += 1;
            Ok(1)
        }
    }

    fn roi_string(value: &str) -> Vec<u8> {
        let utf16: Vec<u16> = value.encode_utf16().collect();
        let mut out = Vec::new();
        out.extend_from_slice(&8u16.to_le_bytes());
        out.extend_from_slice(&((utf16.len() * 2 + 2) as i32).to_le_bytes());
        for ch in utf16 {
            out.extend_from_slice(&ch.to_le_bytes());
        }
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    fn build_roi_stream(shape_type: i32, name: &str, points: &[(f64, f64)]) -> Vec<u8> {
        let mut stream = Vec::new();
        stream.extend_from_slice(&3u16.to_le_bytes());
        stream.extend_from_slice(&0x04100010i32.to_le_bytes());
        stream.extend_from_slice(&0u16.to_le_bytes());
        stream.extend_from_slice(&[0u8; 8]);
        stream.extend_from_slice(&1u16.to_le_bytes());
        stream.extend_from_slice(&ZVI_ROI_SIGNATURE.to_le_bytes());
        stream.extend_from_slice(&[0u8; 18]);
        stream.extend_from_slice(&0i32.to_le_bytes());
        stream.extend_from_slice(&[0u8; 6]);
        stream.extend_from_slice(&32i32.to_le_bytes());
        stream.extend_from_slice(&shape_type.to_le_bytes());
        stream.extend_from_slice(&[0u8; 8]);
        stream.extend_from_slice(&0i32.to_le_bytes());
        stream.extend_from_slice(&0i32.to_le_bytes());
        stream.extend_from_slice(&100i32.to_le_bytes());
        stream.extend_from_slice(&100i32.to_le_bytes());
        stream.extend_from_slice(&[0u8; 8]);
        stream.extend_from_slice(&0i32.to_le_bytes());
        stream.extend_from_slice(&roi_string("Arial"));
        stream.extend_from_slice(&roi_string(name));
        stream.extend_from_slice(&[0u8; 4]);
        stream.extend_from_slice(&5i32.to_le_bytes());
        stream.extend_from_slice(&[0u8; 2]);
        stream.extend_from_slice(&(points.len() as i32).to_le_bytes());
        stream.extend_from_slice(&[0u8; 6]);
        for &(x, y) in points {
            stream.extend_from_slice(&x.to_le_bytes());
            stream.extend_from_slice(&y.to_le_bytes());
        }
        stream
    }

    #[test]
    fn zvi_zero_padded_read_loops_until_eof() {
        let mut reader = OneByteReads {
            data: vec![1, 2, 3],
            pos: 0,
        };
        let mut out = vec![0; 5];
        read_zero_padded(&mut reader, &mut out).unwrap();
        assert_eq!(out, vec![1, 2, 3, 0, 0]);
    }

    #[test]
    fn zvi_timestamp_conversion_matches_java_excel_epoch() {
        assert_eq!(
            zvi_timestamp_to_iso8601("1.0").as_deref(),
            Some("1900-01-01T00:00:00.000")
        );
        assert_eq!(
            zvi_timestamp_to_iso8601("43831.5").as_deref(),
            Some("2020-01-01T12:00:00.000")
        );
    }

    #[test]
    fn zvi_exposes_each_tile_as_a_separate_series() {
        // Two tiles, each a single (z=c=t=0) 1x1 plane. ZeissZVIReader records
        // the tile index per plane and treats each tile as its own series
        // (totalTiles = offsets.length / getImageCount()).
        let path = temp_path("two_tiles");
        {
            let mut comp = cfb::create(&path).unwrap();
            comp.create_storage_all("/Image/Item(1)").unwrap();
            comp.create_storage_all("/Image/Item(2)").unwrap();
            comp.create_stream("/Image/Item(1)/CONTENTS")
                .unwrap()
                .write_all(&build_item(0, 0, 0, 0, 11))
                .unwrap();
            comp.create_stream("/Image/Item(2)/CONTENTS")
                .unwrap()
                .write_all(&build_item(0, 0, 0, 1, 22))
                .unwrap();
        }

        let mut reader = ZeissZviReader::new();
        reader.set_id(&path).unwrap();

        assert_eq!(reader.series_count(), 2);
        let meta = reader.metadata();
        assert_eq!(meta.image_count, 1);
        assert_eq!((meta.size_x, meta.size_y), (1, 1));

        // Series 0 -> tile 0.
        assert_eq!(reader.series(), 0);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![11]);

        // Series 1 -> tile 1.
        reader.set_series(1).unwrap();
        assert_eq!(reader.open_bytes(0).unwrap(), vec![22]);
        assert!(reader.open_bytes_region(0, 1, 0, 1, 1).is_err());

        assert!(reader.set_series(2).is_err());

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn zvi_single_tile_is_one_series() {
        let path = temp_path("one_tile");
        {
            let mut comp = cfb::create(&path).unwrap();
            comp.create_storage_all("/Image/Item(1)").unwrap();
            comp.create_stream("/Image/Item(1)/CONTENTS")
                .unwrap()
                .write_all(&build_item(0, 0, 0, 0, 99))
                .unwrap();
        }

        let mut reader = ZeissZviReader::new();
        reader.set_id(&path).unwrap();
        assert_eq!(reader.series_count(), 1);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![99]);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn zvi_validates_only_first_item_bpp_like_java() {
        let path = temp_path("later_item_bogus_bpp");
        {
            let mut comp = cfb::create(&path).unwrap();
            comp.create_storage_all("/Image/Item(1)").unwrap();
            comp.create_storage_all("/Image/Item(2)").unwrap();
            comp.create_stream("/Image/Item(1)/CONTENTS")
                .unwrap()
                .write_all(&build_item_with_bpp(0, 0, 0, 0, 11, 1100, 1))
                .unwrap();
            comp.create_stream("/Image/Item(2)/CONTENTS")
                .unwrap()
                .write_all(&build_item_with_bpp(0, 0, 0, 1, 22, 1100, 2_687_024))
                .unwrap();
        }

        let mut reader = ZeissZviReader::new();
        reader.set_id(&path).unwrap();
        assert_eq!(reader.series_count(), 2);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![11]);
        reader.set_series(1).unwrap();
        assert_eq!(reader.open_bytes(0).unwrap(), vec![22]);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn zvi_zero_pads_a_short_raw_plane() {
        // ZeissZVIReader.openBytes reads into a pre-zeroed buffer, so a stream
        // that ends a few bytes short of a full plane leaves the tail zero
        // rather than failing. Here the single uncompressed pixel byte (99) is
        // truncated away, so the decoded plane must be a single zero byte.
        let path = temp_path("short_plane");
        let mut item = build_item(0, 0, 0, 0, 99);
        item.truncate(item.len() - 4);
        {
            let mut comp = cfb::create(&path).unwrap();
            comp.create_storage_all("/Image/Item(1)").unwrap();
            comp.create_stream("/Image/Item(1)/CONTENTS")
                .unwrap()
                .write_all(&item)
                .unwrap();
        }

        let mut reader = ZeissZviReader::new();
        reader.set_id(&path).unwrap();
        assert_eq!(reader.open_bytes(0).unwrap(), vec![0]);
        assert_eq!(reader.open_bytes_region(0, 0, 0, 1, 1).unwrap(), vec![0]);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn zvi_large_item_header_still_initializes_metadata() {
        let path = temp_path("large_header");
        {
            let mut comp = cfb::create(&path).unwrap();
            comp.create_storage_all("/Image/Item(1)").unwrap();
            comp.create_stream("/Image/Item(1)/CONTENTS")
                .unwrap()
                .write_all(&build_item_with_pad(0, 0, 0, 0, 77, 70_000))
                .unwrap();
        }

        let mut reader = ZeissZviReader::new();
        reader.set_id(&path).unwrap();
        let meta = reader.metadata();
        assert_eq!((meta.size_x, meta.size_y, meta.image_count), (1, 1, 1));
        assert_eq!(meta.pixel_type, PixelType::Uint8);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![77]);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn zvi_ome_metadata_projects_java_parse_main_tags_subset() {
        let path = temp_path("ome_tag_projection");
        {
            let mut comp = cfb::create(&path).unwrap();
            comp.create_storage_all("/Image/Item(1)/Tags").unwrap();
            comp.create_storage_all("/Image/Item(2)/Tags").unwrap();
            comp.create_storage_all("/Image/Tags").unwrap();
            comp.create_stream("/Image/Item(1)/CONTENTS")
                .unwrap()
                .write_all(&build_item(0, 4, 0, 0, 55))
                .unwrap();
            comp.create_stream("/Image/Item(2)/CONTENTS")
                .unwrap()
                .write_all(&build_item(0, 5, 0, 0, 56))
                .unwrap();
            comp.create_stream("/Image/Item(1)/Tags/Contents")
                .unwrap()
                .write_all(&build_tag_stream(vec![
                    tag_i32(2820, 4),             // Image Channel Index
                    tag_string(1284, "DAPI"),     // Channel Name
                    tag_f64(16_777_488, 405.0),   // Excitation Wavelength
                    tag_f64(16_777_489, 450.0),   // Emission Wavelength
                    tag_i32(1282, 0x00_22_44_66), // MultiChannel Color
                    tag_f64(1025, 43831.5),       // Camera Acquisition Time
                    tag_f64(2564, 12.5),          // Exposure Time [ms]
                    tag_f64(16_777_218, 123.0),   // Stage Position X
                    tag_f64(16_777_219, 456.0),   // Stage Position Y
                    tag_f64(65_633, 1.25),        // Orca Analog Gain
                    tag_f64(65_634, -3.5),        // Orca Analog Offset
                ]))
                .unwrap();
            comp.create_stream("/Image/Item(2)/Tags/Contents")
                .unwrap()
                .write_all(&build_tag_stream(vec![
                    tag_i32(2820, 5),         // Image Channel Index
                    tag_string(1284, "FITC"), // Channel Name
                ]))
                .unwrap();
            comp.create_stream("/Image/Tags/Contents")
                .unwrap()
                .write_all(&build_tag_stream(vec![
                    tag_f64(769, 0.11),                              // Scale Factor for X
                    tag_f64(772, 0.22),                              // Scale Factor for Y
                    tag_f64(775, 0.33),                              // Scale Factor for Z
                    tag_string(1540, "note"),                        // Comments
                    tag_f64(1793, 43831.5),                          // Acquisition Date
                    tag_string(1795, "Analytical Engines"),          // User Company
                    tag_string(1801, "Ada Lovelace"),                // User Name
                    tag_string(2049, "Plan-Apochromat 63x/1.4 Oil"), // Objective Name
                    tag_f64(1412, 63.0),                             // Objective Magnification
                    tag_f64(1413, 1.4),                              // Objective N.A.
                    tag_f64(1415, 210.0),                            // Objective Working Distance
                    tag_i32(1416, 2),      // Objective Immersion Type -> Oil
                    tag_string(2261, "7"), // Objective ID
                ]))
                .unwrap();
        }

        let mut reader = ZeissZviReader::new();
        reader.set_id(&path).unwrap();
        let ome = reader.ome_metadata().unwrap();
        let img = &ome.images[0];
        assert_eq!(img.description.as_deref(), Some("note"));
        assert_eq!(
            img.acquisition_date.as_deref(),
            Some("2020-01-01T12:00:00.000")
        );
        assert_eq!(img.physical_size_x, Some(0.11));
        assert_eq!(img.physical_size_y, Some(0.22));
        assert_eq!(img.physical_size_z, Some(0.33));
        assert_eq!(img.channels[0].name.as_deref(), Some("DAPI"));
        assert_eq!(img.channels[0].excitation_wavelength, Some(405.0));
        assert_eq!(img.channels[0].emission_wavelength, Some(450.0));
        assert_eq!(img.channels[0].detector_settings_gain, Some(1.25));
        assert_eq!(img.channels[0].detector_settings_offset, Some(-3.5));
        assert_eq!(img.channels[1].name.as_deref(), Some("FITC"));
        assert_eq!(
            img.channels[1].detector_ref.as_deref(),
            Some("Detector:0:1")
        );
        assert_eq!(img.channels[1].detector_settings_gain, None);
        assert_eq!(img.channels[1].detector_settings_offset, None);
        assert_eq!(img.planes.len(), 1);
        assert_eq!(img.planes[0].delta_t, Some(0.0));
        assert_eq!(img.planes[0].exposure_time, Some(0.0125));
        assert_eq!(img.planes[0].position_x, Some(123.0));
        assert_eq!(img.planes[0].position_y, Some(456.0));

        assert_eq!(ome.instruments.len(), 1);
        assert_eq!(ome.instruments[0].detectors.len(), 2);
        assert_eq!(ome.instruments[0].detectors[0].gain, Some(1.25));
        assert_eq!(ome.instruments[0].detectors[0].offset, Some(-3.5));
        assert_eq!(ome.instruments[0].detectors[1].gain, None);
        assert_eq!(ome.instruments[0].detectors[1].offset, None);
        assert_eq!(ome.instruments[0].objectives.len(), 1);
        let objective = &ome.instruments[0].objectives[0];
        assert_eq!(objective.id.as_deref(), Some("Objective:7"));
        assert_eq!(objective.nominal_magnification, Some(63.0));
        assert_eq!(objective.lens_na, Some(1.4));
        assert_eq!(objective.correction.as_deref(), Some("Plan-Apochromat"));
        assert_eq!(objective.immersion.as_deref(), Some("Oil"));
        assert_eq!(objective.working_distance, Some(210.0));
        assert_eq!(ome.experimenters.len(), 1);
        assert_eq!(ome.experimenters[0].id.as_deref(), Some("Experimenter:0"));
        assert_eq!(ome.experimenters[0].first_name.as_deref(), Some("Ada"));
        assert_eq!(ome.experimenters[0].last_name.as_deref(), Some("Lovelace"));
        assert_eq!(
            ome.experimenters[0].institution.as_deref(),
            Some("Analytical Engines")
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn zvi_shapes_stream_projects_representable_ome_rois() {
        let path = temp_path("shapes_roi");
        {
            let mut comp = cfb::create(&path).unwrap();
            comp.create_storage_all("/Image/Item(1)/Shapes").unwrap();
            comp.create_stream("/Image/Item(1)/CONTENTS")
                .unwrap()
                .write_all(&build_item(0, 0, 0, 0, 33))
                .unwrap();
            comp.create_stream("/Image/Item(1)/Shapes/Contents")
                .unwrap()
                .write_all(&build_roi_stream(
                    12,
                    "polyline roi",
                    &[(1.0, 2.0), (3.0, 4.0), (5.0, 6.0)],
                ))
                .unwrap();
        }

        let mut reader = ZeissZviReader::new();
        reader.set_id(&path).unwrap();
        let ome = reader.ome_metadata().unwrap();

        assert_eq!(ome.rois.len(), 1);
        assert_eq!(ome.rois[0].id.as_deref(), Some("ROI:0"));
        assert_eq!(ome.rois[0].name.as_deref(), Some("polyline roi"));
        assert_eq!(ome.rois[0].shapes.len(), 1);
        match &ome.rois[0].shapes[0] {
            OmeShape::Polyline { points, .. } => {
                assert_eq!(points, &vec![(1.0, 2.0), (3.0, 4.0), (5.0, 6.0)]);
            }
            other => panic!("expected polyline ROI, got {other:?}"),
        }

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn zvi_shapes_stream_respects_no_overlays_metadata_level() {
        let path = temp_path("shapes_no_overlays");
        {
            let mut comp = cfb::create(&path).unwrap();
            comp.create_storage_all("/Image/Item(1)/Shapes").unwrap();
            comp.create_stream("/Image/Item(1)/CONTENTS")
                .unwrap()
                .write_all(&build_item(0, 0, 0, 0, 44))
                .unwrap();
            comp.create_stream("/Image/Item(1)/Shapes/Contents")
                .unwrap()
                .write_all(&build_roi_stream(0, "point roi", &[(7.0, 8.0)]))
                .unwrap();
        }

        let mut reader = ZeissZviReader::new();
        reader.set_metadata_options(MetadataOptions {
            level: MetadataLevel::NoOverlays,
            original_metadata: true,
        });
        reader.set_id(&path).unwrap();
        assert!(reader.ome_metadata().unwrap().rois.is_empty());

        let _ = std::fs::remove_file(path);
    }
}
