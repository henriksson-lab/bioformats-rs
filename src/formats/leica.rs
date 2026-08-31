//! Leica LEI confocal format reader.
//!
//! A Leica dataset consists of one `.lei` file plus one or more companion
//! `.tif` files holding the pixel data. All Leica TIFFs carry the private tag
//! `LEICA_MAGIC_TAG = 33923`.
//!
//! The `.lei` file is a custom binary container (not a flat pixel blob): it
//! begins with four endianness marker bytes, then a linked list of header
//! "IFD"-like blocks keyed by integer tags (SERIES=10, IMAGES=15,
//! DIMDESCR=20, ...). The IMAGES block lists the companion TIFF filenames
//! (stored as UTF-16) and the DIMDESCR block describes the Z/C/T dimensions
//! and dimension order. Pixel data is then read from the referenced TIFFs.
//!
//! This is a faithful (if partial) port of the upstream Java `LeicaReader`.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::path::confined_join;
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::tiff::TiffReader;

/// All Leica TIFFs carry this private IFD tag.
const LEICA_MAGIC_TAG: u16 = 33923;

// Header block (pseudo-IFD) tags.
const SERIES: i32 = 10;
const IMAGES: i32 = 15;
const DIMDESCR: i32 = 20;
const TIMEINFO: i32 = 40;
const FILTERSET: i32 = 30;
const SCANNERSET: i32 = 50;
const EXPERIMENT: i32 = 60;
const LUTDESC: i32 = 70;
const CHANDESC: i32 = 80;
const SEQ_SCANNERSET: i32 = 200;
const SEQ_FILTERSET: i32 = 700;
const SEQ_SCANNERSET_END: i32 = 300;
const SEQ_FILTERSET_END: i32 = 800;

/// Maps the Leica dimension id to an axis kind.
fn dimension_name(id: i32) -> &'static str {
    match id {
        120 => "x",
        121 => "y",
        122 => "z",
        116 => "t",
        6815843 => "channel",
        _ => "",
    }
}

// ── Little/big endian byte readers over an in-memory buffer ───────────────────

struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
    little: bool,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8], little: bool) -> Self {
        Cursor {
            data,
            pos: 0,
            little,
        }
    }
    fn seek(&mut self, p: usize) {
        self.pos = p.min(self.data.len());
    }
    fn skip(&mut self, n: usize) {
        self.pos = (self.pos + n).min(self.data.len());
    }
    fn read_i32(&mut self) -> i32 {
        if self.pos + 4 > self.data.len() {
            self.pos = self.data.len();
            return 0;
        }
        let b = &self.data[self.pos..self.pos + 4];
        self.pos += 4;
        if self.little {
            i32::from_le_bytes([b[0], b[1], b[2], b[3]])
        } else {
            i32::from_be_bytes([b[0], b[1], b[2], b[3]])
        }
    }
    fn read_i16(&mut self) -> i16 {
        if self.pos + 2 > self.data.len() {
            self.pos = self.data.len();
            return 0;
        }
        let b = &self.data[self.pos..self.pos + 2];
        self.pos += 2;
        if self.little {
            i16::from_le_bytes([b[0], b[1]])
        } else {
            i16::from_be_bytes([b[0], b[1]])
        }
    }
    fn read_f32(&mut self) -> f32 {
        if self.pos + 4 > self.data.len() {
            self.pos = self.data.len();
            return 0.0;
        }
        let b = &self.data[self.pos..self.pos + 4];
        self.pos += 4;
        let bits = if self.little {
            u32::from_le_bytes([b[0], b[1], b[2], b[3]])
        } else {
            u32::from_be_bytes([b[0], b[1], b[2], b[3]])
        };
        f32::from_bits(bits)
    }
    fn read_f64(&mut self) -> f64 {
        if self.pos + 8 > self.data.len() {
            self.pos = self.data.len();
            return 0.0;
        }
        let b = &self.data[self.pos..self.pos + 8];
        self.pos += 8;
        let bits = if self.little {
            u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
        } else {
            u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
        };
        f64::from_bits(bits)
    }
    fn read_u8(&mut self) -> u8 {
        if self.pos >= self.data.len() {
            return 0;
        }
        let value = self.data[self.pos];
        self.pos += 1;
        value
    }
    /// Read `len` bytes and strip null bytes, mirroring DataTools.stripString
    /// over a UTF-16 buffer (keeps only the non-null bytes as ASCII).
    fn read_string(&mut self, len: usize) -> String {
        let end = (self.pos + len).min(self.data.len());
        let slice = &self.data[self.pos..end];
        self.pos = end;
        let bytes: Vec<u8> = slice.iter().copied().filter(|&c| c != 0).collect();
        String::from_utf8_lossy(&bytes).to_string()
    }
}

/// Per-series parsed state.
struct LeiSeries {
    meta: ImageMetadata,
    /// Companion TIFF file paths in raster order.
    files: Vec<PathBuf>,
}

/// A parsed header block: tag -> file pointer (position just past the size word).
type HeaderIfd = HashMap<i32, usize>;

#[derive(Clone, Default)]
struct LeiDetector {
    id: i32,
    name: String,
    active: bool,
    index: i32,
    offset: Option<f64>,
    voltage: Option<f64>,
}

#[derive(Default)]
struct LeiInstrumentMetadata {
    channel_names: Vec<Option<String>>,
    detectors: Vec<LeiDetector>,
    filters: Vec<LeiFilter>,
    objectives: Vec<LeiObjective>,
    original_metadata: Vec<(String, String)>,
    channel_detector_refs: Vec<Option<usize>>,
    channel_filter_refs: Vec<Option<usize>>,
    pinhole_um: Option<f64>,
    exposure_time: Option<f64>,
    stage_position_x: Option<f64>,
    stage_position_y: Option<f64>,
    stage_position_z: Option<f64>,
}

#[derive(Clone, Default)]
struct LeiFilter {
    channel: usize,
    model: String,
    cut_in: Option<f64>,
    cut_out: Option<f64>,
}

#[derive(Clone, Default)]
struct LeiObjective {
    index: usize,
    model: Option<String>,
    magnification: Option<f64>,
    lens_na: Option<f64>,
    immersion: Option<String>,
    correction: Option<String>,
    serial_number: Option<String>,
    refractive_index: Option<f64>,
}

fn is_leica_instrument_block(tag: i32) -> bool {
    tag == FILTERSET
        || tag == SCANNERSET
        || tag == SEQ_SCANNERSET
        || tag == SEQ_FILTERSET
        || (tag > SEQ_SCANNERSET && tag < SEQ_SCANNERSET_END)
        || (tag > SEQ_FILTERSET && tag < SEQ_FILTERSET_END)
}

#[cfg(test)]
fn parse_leica_instrument_channel_names(
    data: &[u8],
    little: bool,
    ifd: &HeaderIfd,
    effective_size_c: usize,
) -> Vec<Option<String>> {
    parse_leica_instrument_metadata(data, little, ifd, effective_size_c).channel_names
}

fn parse_leica_instrument_metadata(
    data: &[u8],
    little: bool,
    ifd: &HeaderIfd,
    effective_size_c: usize,
) -> LeiInstrumentMetadata {
    let mut keys: Vec<i32> = ifd.keys().copied().collect();
    keys.sort_unstable();
    let sequential = keys.contains(&SEQ_SCANNERSET);

    let mut instrument = LeiInstrumentMetadata {
        channel_detector_refs: vec![None; effective_size_c],
        channel_filter_refs: vec![None; effective_size_c],
        ..Default::default()
    };
    let mut detectors: Vec<LeiDetector> = Vec::new();
    let mut active_channel_indices: Vec<i32> = Vec::new();
    let mut block_num = 1usize;

    for key in keys {
        if !is_leica_instrument_block(key) {
            continue;
        }
        if sequential && (key == FILTERSET || key == SCANNERSET) {
            continue;
        }
        let Some(&offset) = ifd.get(&key) else {
            continue;
        };
        parse_leica_instrument_block(
            data,
            little,
            offset,
            &mut instrument,
            &mut detectors,
            &mut active_channel_indices,
            effective_size_c,
            block_num,
        );
        block_num += 1;
    }

    let mut active_detectors = Vec::new();
    let mut next_channel = 0usize;
    for detector in detectors.iter().filter(|detector| detector.active) {
        if next_channel >= effective_size_c {
            break;
        }
        if instrument.channel_names.len() <= next_channel {
            instrument.channel_names.resize(next_channel + 1, None);
        }
        let replace = instrument.channel_names[next_channel]
            .as_deref()
            .map(|name| name.trim().is_empty() || name == "None")
            .unwrap_or(true);
        if replace {
            instrument.channel_names[next_channel] = Some(detector.name.clone());
        }

        active_detectors.push(detector.clone());
        let detector_index = active_detectors.len() - 1;
        if detector_index == 0 {
            instrument.channel_detector_refs.fill(Some(detector_index));
        }
        instrument.channel_detector_refs[next_channel] = Some(detector_index);
        next_channel += 1;
    }
    instrument.detectors = active_detectors;

    for filter_index in 0..instrument.filters.len() {
        let source_channel = instrument.filters[filter_index].channel as i32;
        if let Some(logical_channel) = active_channel_indices
            .iter()
            .position(|&channel| channel == source_channel)
        {
            if logical_channel < effective_size_c {
                instrument.channel_filter_refs[logical_channel] = Some(filter_index);
            }
        }
    }

    instrument
}

fn parse_leica_instrument_block(
    data: &[u8],
    little: bool,
    offset: usize,
    instrument: &mut LeiInstrumentMetadata,
    detectors: &mut Vec<LeiDetector>,
    active_channel_indices: &mut Vec<i32>,
    effective_size_c: usize,
    block_num: usize,
) {
    let mut c = Cursor::new(data, little);
    c.seek(offset);
    c.skip(4);
    let cb_elements = c.read_i32();
    c.skip(8);
    let n_elements = c.read_i32();
    c.skip(4);
    if cb_elements <= 0 || n_elements <= 0 {
        return;
    }
    let initial_offset = c.pos;

    for j in 0..n_elements as usize {
        let Some(element_offset) =
            initial_offset.checked_add(j.saturating_mul(cb_elements as usize))
        else {
            break;
        };
        if element_offset >= data.len() {
            break;
        }
        c.seek(element_offset);
        let content_id = c.read_string(128);
        let _description = c.read_string(64);
        let mut value = c.read_string(64);
        let data_type = c.read_i16();
        c.skip(6);
        value = match data_type {
            2 => c.read_i16().to_string(),
            3 => c.read_i32().to_string(),
            4 => c.read_f32().to_string(),
            5 => c.read_f64().to_string(),
            7 | 11 => {
                if c.read_u8() == 0 {
                    "false".to_string()
                } else {
                    "true".to_string()
                }
            }
            17 => c.read_string(1),
            _ => value,
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        instrument
            .original_metadata
            .push((format!("Block {block_num} {content_id}"), value.to_string()));

        if content_id == "dblPinhole" {
            if let Ok(pinhole_m) = value.parse::<f64>() {
                if pinhole_m > 0.0 {
                    instrument.pinhole_um = Some(pinhole_m * 1_000_000.0);
                }
            }
            continue;
        }
        if content_id.starts_with("nDelayTime") {
            if let Ok(mut exposure) = value.parse::<f64>() {
                if content_id.ends_with("_ms") {
                    exposure /= 1000.0;
                }
                if exposure > 0.0 {
                    instrument.exposure_time = Some(exposure);
                }
            }
            continue;
        }

        let tokens: Vec<&str> = content_id.split('|').collect();
        if tokens.len() < 3 {
            continue;
        }
        if tokens[0].starts_with("CDetectionUnit") && tokens[1].starts_with("PMT") {
            if tokens.len() < 4 {
                continue;
            }
            let id = tokens[3].parse::<i32>().unwrap_or(0);
            let detector_index = detectors
                .iter()
                .position(|detector| detector.id == id)
                .unwrap_or_else(|| {
                    detectors.push(LeiDetector {
                        id,
                        name: tokens[1].to_string(),
                        active: false,
                        index: -1,
                        offset: None,
                        voltage: None,
                    });
                    detectors.len() - 1
                });
            let detector = &mut detectors[detector_index];
            detector.id = id;
            detector.name = tokens[1].to_string();
            if tokens[2] == "VideoOffset" {
                detector.offset = value.parse::<f64>().ok();
            } else if tokens[2] == "HighVoltage" {
                detector.voltage = value.parse::<f64>().ok();
            } else if tokens[2] == "State" {
                detector.active = value == "Active";
                detector.index = tokens[1]
                    .rsplit(' ')
                    .next()
                    .and_then(|index| index.parse::<i32>().ok())
                    .map(|index| index - 1)
                    .unwrap_or(-1);
                if detector.active
                    && detector.index >= 0
                    && !active_channel_indices.contains(&detector.index)
                {
                    active_channel_indices.push(detector.index);
                }
            }
        } else if tokens[0].starts_with("CTurret") && tokens.len() >= 4 {
            let objective_index = tokens[3].parse::<usize>().unwrap_or(0);
            let objective_slot = instrument
                .objectives
                .iter()
                .position(|objective| objective.index == objective_index)
                .unwrap_or_else(|| {
                    instrument.objectives.push(LeiObjective {
                        index: objective_index,
                        ..Default::default()
                    });
                    instrument.objectives.len() - 1
                });
            let objective = &mut instrument.objectives[objective_slot];
            if tokens[2] == "NumericalAperture" {
                objective.lens_na = value.parse::<f64>().ok();
            } else if tokens[2] == "Objective" {
                parse_leica_objective(value, objective);
            } else if tokens[2] == "OrderNumber" {
                objective.serial_number = Some(value.to_string());
            } else if tokens[2] == "RefractionIndex" {
                objective.refractive_index = value.parse::<f64>().ok();
            }
        } else if tokens[0].starts_with("CSpectrophotometerUnit") && tokens[2] == "Wavelength" {
            let channel = tokens[1]
                .rsplit(' ')
                .next()
                .and_then(|index| index.parse::<usize>().ok())
                .map(|index| index.saturating_sub(1))
                .unwrap_or(0);
            let filter_slot = instrument
                .filters
                .iter()
                .position(|filter| filter.channel == channel)
                .unwrap_or_else(|| {
                    instrument.filters.push(LeiFilter {
                        channel,
                        model: tokens[1].to_string(),
                        ..Default::default()
                    });
                    instrument.filters.len() - 1
                });
            let filter = &mut instrument.filters[filter_slot];
            filter.model = tokens[1].to_string();
            if let Ok(wavelength) = value.parse::<f64>() {
                if tokens.get(3) == Some(&"0") && filter.cut_in.is_none() {
                    filter.cut_in = Some(wavelength);
                } else if tokens.get(3) == Some(&"1") && filter.cut_out.is_none() {
                    filter.cut_out = Some(wavelength);
                }
            }
        } else if tokens[0].starts_with("CSpectrophotometerUnit") && tokens[2] == "Stain" {
            let channel = tokens[1]
                .rsplit(' ')
                .next()
                .and_then(|index| index.parse::<i32>().ok())
                .map(|index| index - 1)
                .unwrap_or(-1);
            if active_channel_indices.contains(&channel) {
                let previous = instrument
                    .channel_names
                    .iter()
                    .rev()
                    .find_map(|name| name.as_deref())
                    .unwrap_or("");
                if previous != value {
                    instrument.channel_names.push(Some(value.to_string()));
                }
            }
        } else if tokens[0].starts_with("CXYZStage") {
            if tokens[2] == "XPos" {
                instrument.stage_position_x = value.parse::<f64>().ok();
            } else if tokens[2] == "YPos" {
                instrument.stage_position_y = value.parse::<f64>().ok();
            } else if tokens[2] == "ZPos" {
                instrument.stage_position_z = value.parse::<f64>().ok();
            }
        } else if tokens[0] == "CScanActuator"
            && tokens[1] == "Z Scan Actuator"
            && tokens[2] == "Position"
        {
            if let Ok(position_m) = value.parse::<f64>() {
                instrument.stage_position_z = Some(position_m * 1_000_000.0);
            }
        }
    }

    instrument
        .channel_detector_refs
        .resize(effective_size_c, None);
    instrument
        .channel_filter_refs
        .resize(effective_size_c, None);
}

fn parse_leica_objective(value: &str, objective: &mut LeiObjective) {
    let mut model = Vec::new();
    let mut magnification = None;
    let mut lens_na = None;
    let mut correction = None;
    let mut immersion = None;

    for word in value.split_whitespace() {
        if magnification.is_none() && lens_na.is_none() {
            if let Some(x_index) = word.find('x') {
                magnification = word[..x_index].trim().parse::<f64>().ok();
                lens_na = word[x_index + 1..].trim().parse::<f64>().ok();
                continue;
            }
            model.push(word);
        } else if correction.is_none() {
            correction = Some(word.to_string());
        } else if immersion.is_none() {
            immersion = Some(word.to_string());
        }
    }

    if !model.is_empty() {
        objective.model = Some(model.join(" "));
    }
    objective.magnification = magnification;
    if lens_na.is_some() {
        objective.lens_na = lens_na;
    }
    objective.correction = correction;
    objective.immersion = immersion;
}

fn parse_leica_lut(data: &[u8], little: bool, offset: usize) -> Vec<Option<i32>> {
    let mut c = Cursor::new(data, little);
    c.seek(offset);
    let n_channels = c.read_i32();
    if n_channels <= 0 {
        return Vec::new();
    }
    c.skip(4);
    let mut colors = Vec::with_capacity(n_channels as usize);
    for _ in 0..n_channels as usize {
        c.skip(4);
        c.skip(1);
        let description_len = c.read_i32().max(0) as usize * 2;
        c.skip(description_len);
        let filename_len = c.read_i32().max(0) as usize * 2;
        c.skip(filename_len);
        let lut_len = c.read_i32().max(0) as usize * 2;
        let lut = c.read_string(lut_len);
        colors.push(leica_lut_color(&lut));
        c.skip(8);
    }
    colors
}

fn leica_lut_color(lut: &str) -> Option<i32> {
    let rgba = match lut.trim().to_ascii_lowercase().as_str() {
        "red" => [255, 0, 0, 255],
        "green" => [0, 255, 0, 255],
        "blue" => [0, 0, 255, 255],
        "yellow" => [255, 255, 0, 255],
        "cyan" => [0, 255, 255, 255],
        "magenta" => [255, 0, 255, 255],
        _ => [255, 255, 255, 255],
    };
    Some(u32::from_be_bytes(rgba) as i32)
}

fn insert_float_metadata(meta_map: &mut HashMap<String, MetadataValue>, key: String, value: f64) {
    if value.is_finite() {
        meta_map.insert(key, MetadataValue::Float(value));
    }
}

fn insert_string_metadata(meta_map: &mut HashMap<String, MetadataValue>, key: String, value: &str) {
    if !value.trim().is_empty() {
        meta_map.insert(key, MetadataValue::String(value.to_string()));
    }
}

fn read_leica_length_string(c: &mut Cursor<'_>, double_length: bool) -> String {
    let mut len = c.read_i32().max(0) as usize;
    if double_length {
        len = len.saturating_mul(2);
    }
    c.read_string(len)
}

fn parse_leica_experiment_metadata(
    data: &[u8],
    little: bool,
    offset: usize,
    meta_map: &mut HashMap<String, MetadataValue>,
) {
    let mut c = Cursor::new(data, little);
    c.seek(offset);
    c.skip(8);
    for key in [
        "Image Description",
        "Main file extension",
        "Image format identifier",
        "Single image extension",
    ] {
        let value = read_leica_length_string(&mut c, true);
        insert_string_metadata(meta_map, key.into(), &value);
    }
}

fn parse_leica_channel_metadata(
    data: &[u8],
    little: bool,
    offset: usize,
    meta_map: &mut HashMap<String, MetadataValue>,
) {
    let mut c = Cursor::new(data, little);
    c.seek(offset);
    let n_bands = c.read_i32().max(0) as usize;
    for band in 0..n_bands {
        let prefix = format!("Band #{} ", band + 1);
        insert_float_metadata(meta_map, format!("{prefix}Lower wavelength"), c.read_f64());
        c.skip(4);
        insert_float_metadata(meta_map, format!("{prefix}Higher wavelength"), c.read_f64());
        c.skip(4);
        insert_float_metadata(meta_map, format!("{prefix}Gain"), c.read_f64());
        insert_float_metadata(meta_map, format!("{prefix}Offset"), c.read_f64());
    }
}

fn insert_leica_instrument_metadata(
    meta_map: &mut HashMap<String, MetadataValue>,
    instrument: &LeiInstrumentMetadata,
    channel_colors: &[Option<i32>],
    effective_size_c: usize,
) {
    for (channel, name) in instrument.channel_names.iter().enumerate() {
        if channel >= effective_size_c {
            break;
        }
        if let Some(name) = name {
            if !name.trim().is_empty() && name != "None" {
                meta_map.insert(
                    format!("channel.{channel}.name"),
                    MetadataValue::String(name.clone()),
                );
            }
        }
    }

    for (key, value) in &instrument.original_metadata {
        insert_string_metadata(meta_map, key.clone(), value);
    }

    for channel in 0..effective_size_c {
        if let Some(pinhole) = instrument.pinhole_um {
            insert_float_metadata(meta_map, format!("channel.{channel}.pinhole_size"), pinhole);
        }
        if let Some(Some(color)) = channel_colors.get(channel) {
            meta_map.insert(
                format!("channel.{channel}.color"),
                MetadataValue::Int(*color as i64),
            );
        }
        if let Some(Some(detector_index)) = instrument.channel_detector_refs.get(channel) {
            meta_map.insert(
                format!("channel.{channel}.detector_ref"),
                MetadataValue::String(format!("Detector:0:{detector_index}")),
            );
            if let Some(detector) = instrument.detectors.get(*detector_index) {
                if let Some(offset) = detector.offset {
                    insert_float_metadata(
                        meta_map,
                        format!("channel.{channel}.detector_settings_offset"),
                        offset,
                    );
                }
                if let Some(voltage) = detector.voltage {
                    insert_float_metadata(
                        meta_map,
                        format!("channel.{channel}.detector_settings_voltage"),
                        voltage,
                    );
                }
            }
        }
        if let Some(Some(filter_index)) = instrument.channel_filter_refs.get(channel) {
            meta_map.insert(
                format!("channel.{channel}.emission_filter_ref"),
                MetadataValue::String(format!("Filter:0:{filter_index}")),
            );
        }
    }

    for (index, detector) in instrument.detectors.iter().enumerate() {
        let prefix = format!("instrument.detector.{index}");
        meta_map.insert(
            format!("{prefix}.id"),
            MetadataValue::String(format!("Detector:0:{index}")),
        );
        meta_map.insert(
            format!("{prefix}.type"),
            MetadataValue::String("PMT".into()),
        );
        insert_string_metadata(meta_map, format!("{prefix}.model"), &detector.name);
        if let Some(offset) = detector.offset {
            insert_float_metadata(meta_map, format!("{prefix}.offset"), offset);
        }
        if let Some(voltage) = detector.voltage {
            insert_float_metadata(meta_map, format!("{prefix}.voltage"), voltage);
        }
    }

    for (index, filter) in instrument.filters.iter().enumerate() {
        let prefix = format!("instrument.filter.{index}");
        meta_map.insert(
            format!("{prefix}.id"),
            MetadataValue::String(format!("Filter:0:{index}")),
        );
        insert_string_metadata(meta_map, format!("{prefix}.model"), &filter.model);
        if let Some(cut_in) = filter.cut_in {
            insert_float_metadata(meta_map, format!("{prefix}.cut_in"), cut_in);
        }
        if let Some(cut_out) = filter.cut_out {
            insert_float_metadata(meta_map, format!("{prefix}.cut_out"), cut_out);
        }
    }

    for objective in &instrument.objectives {
        let prefix = format!("instrument.objective.{}", objective.index);
        meta_map.insert(
            format!("{prefix}.id"),
            MetadataValue::String(format!("Objective:0:{}", objective.index)),
        );
        if let Some(model) = &objective.model {
            insert_string_metadata(meta_map, format!("{prefix}.model"), model);
        }
        if let Some(magnification) = objective.magnification {
            insert_float_metadata(
                meta_map,
                format!("{prefix}.nominal_magnification"),
                magnification,
            );
        }
        if let Some(lens_na) = objective.lens_na {
            insert_float_metadata(meta_map, format!("{prefix}.lens_na"), lens_na);
        }
        if let Some(immersion) = &objective.immersion {
            insert_string_metadata(meta_map, format!("{prefix}.immersion"), immersion);
        }
        if let Some(correction) = &objective.correction {
            insert_string_metadata(meta_map, format!("{prefix}.correction"), correction);
        }
        if let Some(serial) = &objective.serial_number {
            insert_string_metadata(meta_map, format!("{prefix}.serial_number"), serial);
        }
        if let Some(refractive_index) = objective.refractive_index {
            insert_float_metadata(
                meta_map,
                format!("{prefix}.refractive_index"),
                refractive_index,
            );
        }
    }

    if let Some(position_z) = instrument.stage_position_z {
        meta_map.insert(
            "image.stage_label.name".into(),
            MetadataValue::String("Position".into()),
        );
        insert_float_metadata(meta_map, "image.stage_label.z".into(), position_z);
    }
}

fn insert_leica_plane_metadata(
    meta_map: &mut HashMap<String, MetadataValue>,
    instrument: &LeiInstrumentMetadata,
    image_count: u32,
) {
    for plane in 0..image_count {
        if let Some(exposure_time) = instrument.exposure_time {
            insert_float_metadata(
                meta_map,
                format!("plane.{plane}.exposure_time"),
                exposure_time,
            );
        }
        if let Some(position_x) = instrument.stage_position_x {
            insert_float_metadata(meta_map, format!("plane.{plane}.position_x"), position_x);
        }
        if let Some(position_y) = instrument.stage_position_y {
            insert_float_metadata(meta_map, format!("plane.{plane}.position_y"), position_y);
        }
    }
}

fn parse_leica_time_metadata(
    data: &[u8],
    little: bool,
    offset: usize,
    meta_map: &mut HashMap<String, MetadataValue>,
    image_count: u32,
) {
    let mut c = Cursor::new(data, little);
    c.seek(offset);
    let n_dims = c.read_i32();
    meta_map.insert(
        "Number of time-stamped dimensions".into(),
        MetadataValue::Int(i64::from(n_dims)),
    );
    meta_map.insert(
        "Time-stamped dimension".into(),
        MetadataValue::Int(i64::from(c.read_i32())),
    );

    for j in 0..n_dims.max(0) as usize {
        let dim_prefix = format!("Dimension {j}");
        meta_map.insert(
            format!("{dim_prefix} ID"),
            MetadataValue::Int(i64::from(c.read_i32())),
        );
        meta_map.insert(
            format!("{dim_prefix} size"),
            MetadataValue::Int(i64::from(c.read_i32())),
        );
        meta_map.insert(
            format!("{dim_prefix} distance"),
            MetadataValue::Int(i64::from(c.read_i32())),
        );
    }

    let num_stamps = c.read_i32().max(0) as usize;
    meta_map.insert(
        "Number of time-stamps".into(),
        MetadataValue::Int(num_stamps as i64),
    );
    let mut first_seconds = None;
    for j in 0..num_stamps {
        let stamp = c.read_string(64);
        insert_string_metadata(meta_map, format!("Timestamp.{j}"), &stamp);
        let Some(seconds) = leica_timestamp_seconds(&stamp) else {
            continue;
        };
        if first_seconds.is_none() {
            first_seconds = Some(seconds);
            if let Some(iso) = leica_timestamp_to_iso8601(&stamp) {
                meta_map.insert("acquisition_date".into(), MetadataValue::String(iso));
            }
        }
        if (j as u32) < image_count {
            insert_float_metadata(
                meta_map,
                format!("plane.{j}.delta_t"),
                (seconds - first_seconds.unwrap()) as f64,
            );
        }
    }
}

fn leica_timestamp_to_iso8601(stamp: &str) -> Option<String> {
    let (year, month, day, hour, minute, second) = parse_leica_timestamp_parts(stamp)?;
    Some(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}"
    ))
}

fn leica_timestamp_seconds(stamp: &str) -> Option<i64> {
    let (year, month, day, hour, minute, second) = parse_leica_timestamp_parts(stamp)?;
    let days = days_from_civil(year, month, day)?;
    Some(days * 86_400 + i64::from(hour) * 3_600 + i64::from(minute) * 60 + i64::from(second))
}

fn parse_leica_timestamp_parts(stamp: &str) -> Option<(i32, u32, u32, u32, u32, u32)> {
    let clean = stamp.trim_matches(char::from(0)).trim();
    let mut parts = clean.split([',', ':']);
    let year = parts.next()?.parse::<i32>().ok()?;
    let month = parts.next()?.parse::<u32>().ok()?;
    let day = parts.next()?.parse::<u32>().ok()?;
    let hour = parts.next()?.parse::<u32>().ok()?;
    let minute = parts.next()?.parse::<u32>().ok()?;
    let second = parts.next()?.parse::<u32>().ok()?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    Some((year, month, day, hour, minute, second))
}

fn days_from_civil(year: i32, month: u32, day: u32) -> Option<i64> {
    const DAYS_BEFORE_MONTH: [u32; 12] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    let month_index = month.checked_sub(1)? as usize;
    let mut days = i64::from(day.checked_sub(1)?);
    days += i64::from(DAYS_BEFORE_MONTH[month_index]);
    if month > 2 && is_leap_year(year) {
        days += 1;
    }
    for y in 1970..year {
        days += if is_leap_year(y) { 366 } else { 365 };
    }
    Some(days)
}

fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// Locate the .lei file for a given entry path.
///
/// - `.lei` entry: returns it directly.
/// - `.tif`/`.tiff` entry: first honors Java's ImageDescription `Series Name`
///   hint, then falls back to a sibling `.lei`.
/// - `.raw` entry: looks for a sibling `<prefix>.lei`, trimming `_` suffixes.
fn find_lei_file(path: &Path) -> Option<PathBuf> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    if ext.as_deref() == Some("lei") {
        return Some(path.to_path_buf());
    }
    let parent = path.parent()?;
    if matches!(ext.as_deref(), Some("tif") | Some("tiff")) {
        if let Some(hinted) = lei_from_tiff_series_name_hint(path, parent) {
            return Some(hinted);
        }
    }
    if matches!(ext.as_deref(), Some("tif") | Some("tiff") | Some("raw")) {
        let mut prefix = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        loop {
            for cand in [format!("{prefix}.lei"), format!("{prefix}.LEI")] {
                let p = parent.join(&cand);
                if p.exists() {
                    return Some(p);
                }
            }
            match prefix.rfind('_') {
                Some(i) => prefix.truncate(i),
                None => break,
            }
        }
        if matches!(ext.as_deref(), Some("tif") | Some("tiff")) {
            let mut listing: Vec<PathBuf> = std::fs::read_dir(parent)
                .map(|rd| {
                    rd.filter_map(|e| e.ok())
                        .map(|e| e.path())
                        .filter(|p| {
                            p.extension()
                                .and_then(|e| e.to_str())
                                .map(|e| e.eq_ignore_ascii_case("lei"))
                                .unwrap_or(false)
                        })
                        .collect()
                })
                .unwrap_or_default();
            listing.sort();
            return listing.into_iter().next();
        }
    }
    None
}

fn lei_from_tiff_series_name_hint(path: &Path, parent: &Path) -> Option<PathBuf> {
    let mut tiff = TiffReader::new();
    tiff.set_id(path).ok()?;
    let description = match tiff.metadata().series_metadata.get("ImageDescription") {
        Some(MetadataValue::String(value)) => value,
        _ => return None,
    };
    let mut suffix = String::new();
    for line in description
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .lines()
    {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            continue;
        }
        let Some((key, value)) = trimmed.split_once('=') else {
            continue;
        };
        if key.trim().starts_with("Series Name") {
            suffix.push_str(value.trim());
        }
    }
    if suffix.trim().is_empty() {
        return None;
    }

    let direct = confined_join(parent, suffix.trim())?;
    if direct.exists() {
        return Some(direct);
    }

    let ext = direct
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    if !matches!(ext.as_deref(), Some("lei")) {
        for candidate in [direct.with_extension("lei"), direct.with_extension("LEI")] {
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }

    None
}

/// Parse the .lei binary container into a list of series.
fn parse_lei(lei_path: &Path) -> Result<Vec<LeiSeries>> {
    let mut f = File::open(lei_path).map_err(BioFormatsError::Io)?;
    let mut data = Vec::new();
    f.read_to_end(&mut data).map_err(BioFormatsError::Io)?;

    if data.len() < 12 {
        return Err(BioFormatsError::Format("LEI: file too small".into()));
    }

    // Endianness: the four marker bytes are all 0x49 ('I') for little-endian.
    let little = data[0] == 0x49 && data[1] == 0x49 && data[2] == 0x49 && data[3] == 0x49;

    let mut c = Cursor::new(&data, little);
    c.seek(4);
    c.skip(8);
    let mut addr = c.read_i32();

    // Walk the linked list of header IFD blocks.
    let mut header_ifds: Vec<HeaderIfd> = Vec::new();
    let mut guard = 0;
    while addr != 0 && guard < 4096 {
        guard += 1;
        let mut ifd: HeaderIfd = HashMap::new();
        c.seek(addr as usize + 4);
        let mut tag = c.read_i32();
        let mut tag_guard = 0;
        while tag != 0 && tag_guard < 65536 {
            tag_guard += 1;
            let offset = c.read_i32();
            let pos = c.pos;
            c.seek(offset as usize + 12);
            let _size = c.read_i32();
            ifd.insert(tag, c.pos);
            c.seek(pos);
            tag = c.read_i32();
        }
        header_ifds.push(ifd);
        addr = c.read_i32();
    }

    if header_ifds.is_empty() {
        return Err(BioFormatsError::Format("LEI: no header blocks".into()));
    }

    let dir = lei_path.parent().unwrap_or_else(|| Path::new("."));
    let mut name_length = 0usize;
    let mut series: Vec<LeiSeries> = Vec::new();

    for ifd in &header_ifds {
        if let Some(&series_ptr) = ifd.get(&SERIES) {
            c.seek(series_ptr);
            c.skip(8);
            name_length = (c.read_i32() as usize).saturating_mul(2);
        }

        let images_ptr = match ifd.get(&IMAGES) {
            Some(&p) => p,
            None => continue,
        };

        // parseFilenames
        c.seek(images_ptr);
        let mut temp_images = c.read_i32();
        if (temp_images as i64).saturating_mul(name_length as i64) > data.len() as i64 {
            // wrong endianness guess for this count
            let other = !little;
            let mut c2 = Cursor::new(&data, other);
            c2.seek(images_ptr);
            temp_images = c2.read_i32();
        }
        if temp_images <= 0 {
            return Err(BioFormatsError::Format(
                "LEI: image count must be positive".into(),
            ));
        }
        let temp_images = temp_images as usize;

        let raw_size_x = c.read_i32();
        let raw_size_y = c.read_i32();
        if raw_size_x <= 0 || raw_size_y <= 0 {
            return Err(BioFormatsError::Format(format!(
                "LEI: invalid image dimensions {raw_size_x}x{raw_size_y}"
            )));
        }
        let mut size_x = raw_size_x as u32;
        let mut size_y = raw_size_y as u32;
        c.skip(4);
        let raw_samples_per_pixel = c.read_i32();
        if raw_samples_per_pixel <= 0 {
            return Err(BioFormatsError::Format(format!(
                "LEI: invalid samples per pixel {raw_samples_per_pixel}"
            )));
        }
        let samples_per_pixel = raw_samples_per_pixel as u32;
        let mut is_rgb = samples_per_pixel > 1;
        let mut size_c = samples_per_pixel;

        let mut files: Vec<PathBuf> = Vec::with_capacity(temp_images);
        if name_length > 0 {
            for _ in 0..temp_images {
                let name = c.read_string(name_length);
                if !name.is_empty() {
                    if let Some(path) = confined_join(dir, &name) {
                        files.push(path);
                    }
                }
            }
        }
        // Fall back to scanning the directory for TIFFs if names were not usable.
        if files.is_empty() {
            let mut listing: Vec<PathBuf> = std::fs::read_dir(dir)
                .map(|rd| {
                    rd.filter_map(|e| e.ok())
                        .map(|e| e.path())
                        .filter(|p| {
                            p.extension()
                                .and_then(|e| e.to_str())
                                .map(|e| {
                                    e.eq_ignore_ascii_case("tif") || e.eq_ignore_ascii_case("tiff")
                                })
                                .unwrap_or(false)
                        })
                        .collect()
                })
                .unwrap_or_default();
            listing.sort();
            files = listing;
        } else {
            files.sort();
        }

        let mut size_z = 1u32;
        let mut size_t = 1u32;
        let mut pixel_type = PixelType::Uint8;
        let mut bpp_bytes = 1u32;
        let mut effective_little_endian = little;
        let mut order_axes: Vec<char> = Vec::new();

        let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
        meta_map.insert("format".into(), MetadataValue::String("Leica LEI".into()));
        // physicalSizes[0..5] = X, Y, Z, C, T physical sizes (µm / s), mirroring
        // Java LeicaReader.physicalSizes[seriesIndex].
        let mut physical_sizes = [0.0f64; 5];

        // DIMDESCR block: pixel type and dimensions.
        if let Some(&dim_ptr) = ifd.get(&DIMDESCR) {
            c.seek(dim_ptr);
            c.skip(4); // version/unused
                       // ms.rgb = in.readInt() == 20
            let voxel = c.read_i32();
            if voxel == 20 {
                is_rgb = true;
            }
            let mut bpp = c.read_i32();
            if bpp <= 0 {
                return Err(BioFormatsError::Format(format!(
                    "LEI: invalid bytes per pixel {bpp}"
                )));
            }
            if bpp % 3 == 0 {
                size_c = 3;
                is_rgb = true;
                bpp /= 3;
            }
            bpp_bytes = bpp as u32;
            pixel_type = match bpp_bytes {
                1 => PixelType::Uint8,
                2 => PixelType::Uint16,
                4 => PixelType::Float32,
                _ => {
                    return Err(BioFormatsError::Format(format!(
                        "LEI: unsupported bytes per pixel {bpp_bytes}"
                    )))
                }
            };

            let _resolution = c.read_i32(); // bits per pixel / real-world resolution
                                            // Maximum/Minimum voxel intensity strings (getString(true)).
            for _ in 0..2 {
                let l = c.read_i32().max(0) as usize * 2;
                c.skip(l);
            }
            let len = c.read_i32().max(0) as usize;
            c.skip(len * 2 + 4);

            let dim_count = c.read_i32().max(0);
            for j in 0..dim_count {
                let dim_id = c.read_i32();
                let dim_type = dimension_name(dim_id);
                let raw_size = c.read_i32();
                if raw_size <= 0 {
                    return Err(BioFormatsError::Format(format!(
                        "LEI: invalid dimension size {raw_size}"
                    )));
                }
                let size = raw_size as u32;
                let distance = c.read_i32();
                let strlen = c.read_i32().max(0) as usize * 2;
                let size_data = c.read_string(strlen);

                // Java: sizeData.split(" "); physical = value / size; "m" -> µm.
                let mut parts = size_data.split_whitespace();
                let physical_str = parts.next().unwrap_or("");
                let unit = parts.next().unwrap_or("");
                let mut physical = physical_str.parse::<f64>().unwrap_or(0.0) / size.max(1) as f64;
                if unit == "m" {
                    physical *= 1_000_000.0;
                }

                match dim_type {
                    "x" => {
                        size_x = size;
                        physical_sizes[0] = physical;
                    }
                    "y" => {
                        size_y = size;
                        physical_sizes[1] = physical;
                    }
                    "channel" => {
                        if size_c == 0 {
                            size_c = 1;
                        }
                        size_c *= size;
                        if !order_axes.contains(&'C') {
                            order_axes.push('C');
                        }
                        physical_sizes[3] = physical;
                    }
                    "z" => {
                        size_z = size;
                        if !order_axes.contains(&'Z') {
                            order_axes.push('Z');
                        }
                        physical_sizes[2] = physical;
                    }
                    _ => {
                        size_t = size;
                        if !order_axes.contains(&'T') {
                            order_axes.push('T');
                        }
                        physical_sizes[4] = physical;
                    }
                }

                // Per-dimension original metadata (Java "Dim<j> ..." keys).
                let dim_prefix = format!("Dim{}", j);
                meta_map.insert(
                    format!("{dim_prefix} type"),
                    MetadataValue::String(dim_type.to_string()),
                );
                meta_map.insert(
                    format!("{dim_prefix} size"),
                    MetadataValue::Int(size as i64),
                );
                meta_map.insert(
                    format!("{dim_prefix} distance between sub-dimensions"),
                    MetadataValue::Int(distance as i64),
                );
                meta_map.insert(
                    format!("{dim_prefix} physical length"),
                    MetadataValue::String(format!("{physical_str} {unit}")),
                );
                // physical origin (getString(true)): length-prefixed UTF-16.
                let origin_len = c.read_i32().max(0) as usize * 2;
                let origin = c.read_string(origin_len);
                meta_map.insert(
                    format!("{dim_prefix} physical origin"),
                    MetadataValue::String(origin),
                );
            }

            // Series name and description (getString(false)).
            let name_len = c.read_i32().max(0) as usize * 2;
            let series_name = c.read_string(name_len);
            meta_map.insert(
                "Series name".into(),
                MetadataValue::String(series_name.clone()),
            );
            insert_string_metadata(&mut meta_map, "image.name".into(), &series_name);
            let descr_len = c.read_i32().max(0) as usize * 2;
            let series_descr = c.read_string(descr_len);
            meta_map.insert(
                "Series description".into(),
                MetadataValue::String(series_descr.clone()),
            );
            insert_string_metadata(&mut meta_map, "image.description".into(), &series_descr);
        }

        let effective_size_c = if is_rgb {
            (size_c / samples_per_pixel.max(1)).max(1)
        } else {
            size_c.max(1)
        } as usize;
        let instrument = parse_leica_instrument_metadata(&data, little, ifd, effective_size_c);
        let channel_colors = ifd
            .get(&LUTDESC)
            .map(|&offset| parse_leica_lut(&data, little, offset))
            .unwrap_or_default();
        insert_leica_instrument_metadata(
            &mut meta_map,
            &instrument,
            &channel_colors,
            effective_size_c,
        );

        // Record physical sizes (µm for X/Y/Z/C, seconds for T time increment).
        for (idx, key) in [
            "physicalSizeX",
            "physicalSizeY",
            "physicalSizeZ",
            "physicalSizeC",
            "timeIncrement",
        ]
        .iter()
        .enumerate()
        {
            if physical_sizes[idx] > 0.0 {
                meta_map.insert((*key).into(), MetadataValue::Float(physical_sizes[idx]));
            }
        }

        if size_z == 0 {
            size_z = 1;
        }
        if size_t == 0 {
            size_t = 1;
        }
        if size_c == 0 {
            size_c = 1;
        }

        // Complete the dimension order (Java appends remaining axes).
        for a in ['C', 'Z', 'T'] {
            if !order_axes.contains(&a) {
                order_axes.push(a);
            }
        }
        let dimension_order = match (order_axes.first(), order_axes.get(1), order_axes.get(2)) {
            (Some('C'), Some('Z'), Some('T')) => DimensionOrder::XYCZT,
            (Some('C'), Some('T'), Some('Z')) => DimensionOrder::XYCTZ,
            (Some('Z'), Some('C'), Some('T')) => DimensionOrder::XYZCT,
            (Some('Z'), Some('T'), Some('C')) => DimensionOrder::XYZTC,
            (Some('T'), Some('C'), Some('Z')) => DimensionOrder::XYTCZ,
            (Some('T'), Some('Z'), Some('C')) => DimensionOrder::XYTZC,
            _ => DimensionOrder::XYZCT,
        };

        if files.is_empty() {
            continue;
        }

        let image_count = (size_z * size_c * size_t).max(files.len() as u32);
        if let Some(&time_ptr) = ifd.get(&TIMEINFO) {
            parse_leica_time_metadata(&data, little, time_ptr, &mut meta_map, image_count);
        }
        if let Some(&experiment_ptr) = ifd.get(&EXPERIMENT) {
            parse_leica_experiment_metadata(&data, little, experiment_ptr, &mut meta_map);
        }
        if let Some(&channel_ptr) = ifd.get(&CHANDESC) {
            parse_leica_channel_metadata(&data, little, channel_ptr, &mut meta_map);
        }
        insert_leica_plane_metadata(&mut meta_map, &instrument, image_count);

        // Java LeicaReader reads the first companion TIFF IFD after parsing the
        // LEI metadata and lets the TIFF's dimensions, pixel type, and
        // multi-byte endianness override the values declared in the LEI.
        if let Some(first_file) = files.first().filter(|p| p.exists()) {
            let mut tiff = TiffReader::new();
            if tiff.set_id(first_file).is_ok() {
                let tiff_meta = tiff.metadata();
                if tiff_meta.size_x > 0 {
                    size_x = tiff_meta.size_x;
                }
                if tiff_meta.size_y > 0 {
                    size_y = tiff_meta.size_y;
                }
                pixel_type = tiff_meta.pixel_type;
                bpp_bytes = pixel_type.bytes_per_sample() as u32;
                if bpp_bytes > 1 {
                    effective_little_endian = tiff_meta.is_little_endian;
                }
            }
        }

        let meta = ImageMetadata {
            size_x,
            size_y,
            size_z,
            size_c,
            size_t,
            pixel_type,
            bits_per_pixel: (bpp_bytes * 8) as u16,
            image_count,
            dimension_order,
            is_rgb,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: effective_little_endian,
            resolution_count: 1,
            thumbnail: false,
            series_metadata: meta_map,
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        };

        series.push(LeiSeries { meta, files });
    }

    if series.is_empty() {
        return Err(BioFormatsError::Format(
            "LEI: no valid series / TIFF files found".into(),
        ));
    }

    Ok(series)
}

pub struct LeicaReader {
    path: Option<PathBuf>,
    series_list: Vec<LeiSeries>,
    series: usize,
}

impl LeicaReader {
    pub fn new() -> Self {
        LeicaReader {
            path: None,
            series_list: Vec::new(),
            series: 0,
        }
    }
}
impl Default for LeicaReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for LeicaReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("lei"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // A Leica TIFF carries the private tag LEICA_MAGIC_TAG (33923). Scan the
        // first IFD's tag list for that tag id.
        tiff_has_tag(header, LEICA_MAGIC_TAG)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let lei = find_lei_file(path)
            .ok_or_else(|| BioFormatsError::Format("LEI file not found".into()))?;
        self.series_list = parse_lei(&lei)?;
        self.series = 0;
        self.path = Some(lei);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.series_list.clear();
        self.series = 0;
        Ok(())
    }
    fn series_count(&self) -> usize {
        self.series_list.len()
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if s >= self.series_list.len() {
            Err(BioFormatsError::SeriesOutOfRange(s))
        } else {
            self.series = s;
            Ok(())
        }
    }
    fn series(&self) -> usize {
        self.series
    }
    fn metadata(&self) -> &ImageMetadata {
        self.series_list
            .get(self.series)
            .map(|series| &series.meta)
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let s = self
            .series_list
            .get(self.series)
            .ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= s.meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        // Java: fileIndex = no < files.size() ? no : 0;
        //       planeIndex = no < files.size() ? 0 : no;
        let (file_index, page) = if (plane_index as usize) < s.files.len() {
            (plane_index as usize, 0u32)
        } else {
            (0usize, plane_index)
        };
        let file = s
            .files
            .get(file_index)
            .ok_or(BioFormatsError::PlaneOutOfRange(plane_index))?;
        if !file.exists() {
            return Ok(blank_plane(&s.meta));
        }
        if is_raw_companion(file) {
            return read_raw_plane(file, &s.meta, page);
        }
        let mut r = TiffReader::new();
        if let Err(err) = r.set_id(file) {
            if file.exists() {
                return Err(err);
            }
            return Ok(blank_plane(&s.meta));
        }
        let inner = r.metadata().image_count.max(1);
        if page >= inner {
            return Err(BioFormatsError::Format(format!(
                "LEI: TIFF page {page} out of range for {} ({} pages)",
                file.display(),
                inner
            )));
        }
        r.open_bytes(page)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let full = self.open_bytes(plane_index)?;
        let meta = self.metadata();
        crop_region(&full, meta, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self
            .series_list
            .get(self.series)
            .map(|s| &s.meta)
            .ok_or(BioFormatsError::NotInitialized)?;
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = ((meta.size_x - tw) / 2, (meta.size_y - th) / 2);
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        use crate::common::ome_metadata::{
            OmeDetector, OmeFilter, OmeInstrument, OmeLightPath, OmeObjective,
        };

        let meta = self.metadata();
        if std::ptr::eq(meta, crate::common::reader::uninitialized_metadata()) {
            return None;
        }

        let mut ome = crate::common::ome_metadata::OmeMetadata::from_image_metadata(meta);
        let _ = ome.add_original_metadata_annotations(meta, 0);
        let image = ome.images.get_mut(0)?;
        image.name = metadata_string(meta, "image.name");
        image.description = metadata_string(meta, "image.description");
        image.acquisition_date = metadata_string(meta, "acquisition_date");
        image.physical_size_x = metadata_float(meta, "physicalSizeX");
        image.physical_size_y = metadata_float(meta, "physicalSizeY");
        image.physical_size_z = metadata_float(meta, "physicalSizeZ");
        image.time_increment = metadata_float(meta, "timeIncrement");
        for (channel_index, channel) in image.channels.iter_mut().enumerate() {
            if let Some(name) = metadata_string(meta, &format!("channel.{channel_index}.name")) {
                channel.name = Some(name);
            }
            channel.emission_wavelength = metadata_float(
                meta,
                &format!("channel.{channel_index}.emission_wavelength"),
            );
            channel.excitation_wavelength = metadata_float(
                meta,
                &format!("channel.{channel_index}.excitation_wavelength"),
            );
            channel.pinhole_size =
                metadata_float(meta, &format!("channel.{channel_index}.pinhole_size"));
            channel.color = metadata_int(meta, &format!("channel.{channel_index}.color"));
            channel.detector_ref =
                metadata_string(meta, &format!("channel.{channel_index}.detector_ref"));
            channel.detector_settings_offset = metadata_float(
                meta,
                &format!("channel.{channel_index}.detector_settings_offset"),
            );
            channel.detector_settings_voltage = metadata_float(
                meta,
                &format!("channel.{channel_index}.detector_settings_voltage"),
            );
        }

        if has_leica_instrument_metadata(meta) {
            let mut instrument = OmeInstrument {
                id: Some("Instrument:0".into()),
                ..Default::default()
            };

            for index in 0..64 {
                let prefix = format!("instrument.objective.{index}");
                if metadata_string(meta, &format!("{prefix}.id")).is_none() {
                    continue;
                }
                instrument.objectives.push(OmeObjective {
                    id: metadata_string(meta, &format!("{prefix}.id")),
                    model: metadata_string(meta, &format!("{prefix}.model")),
                    serial_number: metadata_string(meta, &format!("{prefix}.serial_number")),
                    nominal_magnification: metadata_float(
                        meta,
                        &format!("{prefix}.nominal_magnification"),
                    ),
                    lens_na: metadata_float(meta, &format!("{prefix}.lens_na")),
                    immersion: metadata_string(meta, &format!("{prefix}.immersion")),
                    correction: metadata_string(meta, &format!("{prefix}.correction")),
                    ..Default::default()
                });
            }

            for index in 0..64 {
                let prefix = format!("instrument.detector.{index}");
                if metadata_string(meta, &format!("{prefix}.id")).is_none() {
                    continue;
                }
                instrument.detectors.push(OmeDetector {
                    id: metadata_string(meta, &format!("{prefix}.id")),
                    model: metadata_string(meta, &format!("{prefix}.model")),
                    detector_type: metadata_string(meta, &format!("{prefix}.type")),
                    offset: metadata_float(meta, &format!("{prefix}.offset")),
                    ..Default::default()
                });
            }

            for index in 0..64 {
                let prefix = format!("instrument.filter.{index}");
                if metadata_string(meta, &format!("{prefix}.id")).is_none() {
                    continue;
                }
                instrument.filters.push(OmeFilter {
                    id: metadata_string(meta, &format!("{prefix}.id")),
                    model: metadata_string(meta, &format!("{prefix}.model")),
                    cut_in: metadata_float(meta, &format!("{prefix}.cut_in")),
                    cut_out: metadata_float(meta, &format!("{prefix}.cut_out")),
                    ..Default::default()
                });
            }

            image.instrument_ref = Some(0);
            if !instrument.objectives.is_empty()
                && meta
                    .series_metadata
                    .contains_key("instrument.objective.0.id")
            {
                image.objective_ref = Some(0);
                image.objective_settings_refractive_index =
                    metadata_float(meta, "instrument.objective.0.refractive_index");
            }
            image
                .light_paths
                .resize(image.channels.len(), OmeLightPath::default());
            for channel_index in 0..image.channels.len() {
                if let Some(filter_id) = metadata_string(
                    meta,
                    &format!("channel.{channel_index}.emission_filter_ref"),
                ) {
                    if !image.light_paths[channel_index]
                        .emission_filter_ids
                        .contains(&filter_id)
                    {
                        image.light_paths[channel_index]
                            .emission_filter_ids
                            .push(filter_id);
                    }
                }
            }
            ome.instruments.push(instrument);
        }
        Some(ome)
    }
}

fn is_raw_companion(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("raw"))
        .unwrap_or(false)
}

fn metadata_float(meta: &ImageMetadata, key: &str) -> Option<f64> {
    match meta.series_metadata.get(key) {
        Some(MetadataValue::Float(value)) if value.is_finite() && *value > 0.0 => Some(*value),
        _ => None,
    }
}

fn metadata_string(meta: &ImageMetadata, key: &str) -> Option<String> {
    match meta.series_metadata.get(key) {
        Some(MetadataValue::String(value)) if !value.trim().is_empty() => Some(value.clone()),
        _ => None,
    }
}

fn metadata_int(meta: &ImageMetadata, key: &str) -> Option<i32> {
    match meta.series_metadata.get(key) {
        Some(MetadataValue::Int(value)) => i32::try_from(*value).ok(),
        _ => None,
    }
}

fn has_leica_instrument_metadata(meta: &ImageMetadata) -> bool {
    meta.series_metadata
        .keys()
        .any(|key| key.starts_with("instrument."))
}

fn read_raw_plane(path: &Path, meta: &ImageMetadata, plane_index: u32) -> Result<Vec<u8>> {
    let plane_size = blank_plane(meta).len();
    let offset = (plane_index as u64)
        .checked_mul(plane_size as u64)
        .ok_or_else(|| BioFormatsError::Format("LEI: raw plane offset overflow".into()))?;
    let mut file = File::open(path).map_err(BioFormatsError::Io)?;
    file.seek(SeekFrom::Start(offset))
        .map_err(BioFormatsError::Io)?;
    let mut plane = vec![0; plane_size];
    file.read_exact(&mut plane).map_err(BioFormatsError::Io)?;
    Ok(plane)
}

fn blank_plane(meta: &ImageMetadata) -> Vec<u8> {
    let samples = if meta.is_rgb {
        meta.size_c.max(1) as usize
    } else {
        1
    };
    let len =
        meta.size_x as usize * meta.size_y as usize * samples * meta.pixel_type.bytes_per_sample();
    vec![0; len]
}

/// Clip an (x, y, w, h) region out of a full plane, with bounds validation.
pub(crate) fn crop_region(
    full: &[u8],
    meta: &ImageMetadata,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
) -> Result<Vec<u8>> {
    let bps = meta.pixel_type.bytes_per_sample();
    let samples = if meta.is_rgb {
        meta.size_c.max(1) as usize
    } else {
        1
    };
    let pixel = bps * samples;
    let full_w = meta.size_x as usize;
    let full_h = meta.size_y as usize;
    let row = full_w * pixel;

    // Validate that the requested region lies within the plane.
    if x.checked_add(w).is_none_or(|end| end as usize > full_w)
        || y.checked_add(h).is_none_or(|end| end as usize > full_h)
    {
        return Err(BioFormatsError::Format(format!(
            "region {}x{}+{}+{} exceeds plane {}x{}",
            w, h, x, y, full_w, full_h
        )));
    }
    let out_row = w as usize * pixel;
    let mut out = Vec::with_capacity(h as usize * out_row);
    for r in 0..h as usize {
        let row_start = (y as usize + r) * row;
        let start = row_start + x as usize * pixel;
        let end = start + out_row;
        if end > full.len() {
            return Err(BioFormatsError::Format(
                "region extends past available pixel data".into(),
            ));
        }
        out.extend_from_slice(&full[start..end]);
    }
    Ok(out)
}

/// Minimal TIFF IFD tag scan: returns true if the first IFD contains `target`.
fn tiff_has_tag(header: &[u8], target: u16) -> bool {
    if header.len() < 8 {
        return false;
    }
    let little = match &header[0..2] {
        [0x49, 0x49] => true,
        [0x4D, 0x4D] => false,
        _ => return false,
    };
    let rd16 = |b: &[u8]| -> u16 {
        if little {
            u16::from_le_bytes([b[0], b[1]])
        } else {
            u16::from_be_bytes([b[0], b[1]])
        }
    };
    let rd32 = |b: &[u8]| -> u32 {
        if little {
            u32::from_le_bytes([b[0], b[1], b[2], b[3]])
        } else {
            u32::from_be_bytes([b[0], b[1], b[2], b[3]])
        }
    };
    let ifd_off = rd32(&header[4..8]) as usize;
    if ifd_off + 2 > header.len() {
        return false;
    }
    let entries = rd16(&header[ifd_off..ifd_off + 2]) as usize;
    let mut p = ifd_off + 2;
    for _ in 0..entries {
        if p + 2 > header.len() {
            break;
        }
        let tag = rd16(&header[p..p + 2]);
        if tag == target {
            return true;
        }
        p += 12; // each IFD entry is 12 bytes
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ImageWriter;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("bioformats_lei_{nanos}_{name}"))
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = temp_path(name);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn push_i32(buf: &mut Vec<u8>, value: i32) {
        buf.extend_from_slice(&value.to_le_bytes());
    }

    fn push_f64(buf: &mut Vec<u8>, value: f64) {
        buf.extend_from_slice(&value.to_le_bytes());
    }

    fn push_i16(buf: &mut Vec<u8>, value: i16) {
        buf.extend_from_slice(&value.to_le_bytes());
    }

    fn push_fixed_string(buf: &mut Vec<u8>, value: &str, len: usize) {
        let mut bytes = vec![0; len];
        let src = value.as_bytes();
        bytes[..src.len().min(len)].copy_from_slice(&src[..src.len().min(len)]);
        buf.extend_from_slice(&bytes);
    }

    fn push_leica_instrument_record(
        buf: &mut Vec<u8>,
        content_id: &str,
        value: &str,
        data_type: i16,
    ) {
        push_fixed_string(buf, content_id, 128);
        push_fixed_string(buf, "", 64);
        push_fixed_string(buf, value, 64);
        push_i16(buf, data_type);
        buf.extend_from_slice(&[0; 6]);
    }

    fn put_i32(buf: &mut [u8], offset: usize, value: i32) {
        buf[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn push_utf16le_fixed_ascii(buf: &mut Vec<u8>, text: &str, chars: usize) {
        let bytes = text.as_bytes();
        for i in 0..chars {
            buf.push(bytes.get(i).copied().unwrap_or(0));
            buf.push(0);
        }
    }

    fn push_leica_len_string(buf: &mut Vec<u8>, text: &str) {
        push_i32(buf, text.len() as i32);
        push_utf16le_fixed_ascii(buf, text, text.len());
    }

    fn push_leica_fixed_utf16_string(buf: &mut Vec<u8>, text: &str, chars: usize) {
        push_utf16le_fixed_ascii(buf, text, chars);
    }

    fn append_leica_block(buf: &mut Vec<u8>, payload: &[u8]) -> i32 {
        let offset = buf.len();
        buf.resize(offset + 12, 0);
        push_i32(buf, payload.len() as i32);
        buf.extend_from_slice(payload);
        offset as i32
    }

    fn assert_metadata_string(
        meta_map: &HashMap<String, MetadataValue>,
        key: &str,
        expected: &str,
    ) {
        match meta_map.get(key) {
            Some(MetadataValue::String(value)) => assert_eq!(value, expected),
            other => panic!("unexpected {key}: {other:?}"),
        }
    }

    fn assert_metadata_float(meta_map: &HashMap<String, MetadataValue>, key: &str, expected: f64) {
        match meta_map.get(key) {
            Some(MetadataValue::Float(value)) => assert_eq!(*value, expected),
            other => panic!("unexpected {key}: {other:?}"),
        }
    }

    fn assert_metadata_int(meta_map: &HashMap<String, MetadataValue>, key: &str, expected: i64) {
        match meta_map.get(key) {
            Some(MetadataValue::Int(value)) => assert_eq!(*value, expected),
            other => panic!("unexpected {key}: {other:?}"),
        }
    }

    fn minimal_lei(filename: &str, declared_x: i32, declared_y: i32) -> Vec<u8> {
        let header_offset = 32usize;
        let file_length = 32usize;

        let mut data = vec![0; 64];
        data[0..4].copy_from_slice(b"IIII");
        put_i32(&mut data, 12, header_offset as i32);

        let mut series_payload = Vec::new();
        push_i32(&mut series_payload, 1);
        push_i32(&mut series_payload, 1);
        push_i32(&mut series_payload, file_length as i32);
        push_i32(&mut series_payload, 3);
        series_payload.extend_from_slice(b"t\0i\0f\0");
        let series_offset = append_leica_block(&mut data, &series_payload);

        let mut images_payload = Vec::new();
        push_i32(&mut images_payload, 1);
        push_i32(&mut images_payload, declared_x);
        push_i32(&mut images_payload, declared_y);
        push_i32(&mut images_payload, 8);
        push_i32(&mut images_payload, 1);
        push_utf16le_fixed_ascii(&mut images_payload, filename, file_length);
        let images_offset = append_leica_block(&mut data, &images_payload);

        let tag_base = header_offset + 4;
        put_i32(&mut data, tag_base, SERIES);
        put_i32(&mut data, tag_base + 4, series_offset);
        put_i32(&mut data, tag_base + 8, IMAGES);
        put_i32(&mut data, tag_base + 12, images_offset);
        put_i32(&mut data, tag_base + 16, 0);
        put_i32(&mut data, tag_base + 20, 0);
        data
    }

    #[test]
    fn lei_instrument_stain_projects_channel_name_like_java() {
        let mut data = Vec::new();
        push_i32(&mut data, 0);
        let cb_elements = 262;
        push_i32(&mut data, cb_elements);
        data.extend_from_slice(&[0; 8]);
        push_i32(&mut data, 2);
        push_i32(&mut data, 0);
        push_leica_instrument_record(&mut data, "CDetectionUnit|PMT 1|State|1", "Active", 0);
        push_leica_instrument_record(
            &mut data,
            "CSpectrophotometerUnit|Channel 1|Stain|0",
            "DAPI",
            0,
        );

        let mut ifd = HeaderIfd::new();
        ifd.insert(FILTERSET, 0);
        let names = parse_leica_instrument_channel_names(&data, true, &ifd, 1);
        assert_eq!(names.len(), 1);
        assert_eq!(names[0].as_deref(), Some("DAPI"));
    }

    #[test]
    fn lei_scan_actuator_z_position_is_projected_in_micrometers_like_java() {
        let mut data = Vec::new();
        push_i32(&mut data, 0);
        let cb_elements = 262;
        push_i32(&mut data, cb_elements);
        data.extend_from_slice(&[0; 8]);
        push_i32(&mut data, 1);
        push_i32(&mut data, 0);
        push_leica_instrument_record(
            &mut data,
            "CScanActuator|Z Scan Actuator|Position|0",
            "0.000012",
            0,
        );

        let mut ifd = HeaderIfd::new();
        ifd.insert(FILTERSET, 0);
        let instrument = parse_leica_instrument_metadata(&data, true, &ifd, 1);
        let mut meta_map = HashMap::new();
        insert_leica_instrument_metadata(&mut meta_map, &instrument, &[], 1);

        assert_metadata_string(&meta_map, "image.stage_label.name", "Position");
        assert_metadata_float(&meta_map, "image.stage_label.z", 12.0);
    }

    #[test]
    fn lei_instrument_blocks_retain_graph_pinhole_and_lut_color() {
        let mut data = Vec::new();
        push_i32(&mut data, 0);
        let cb_elements = 264;
        push_i32(&mut data, cb_elements);
        data.extend_from_slice(&[0; 8]);
        push_i32(&mut data, 13);
        push_i32(&mut data, 0);
        push_leica_instrument_record(&mut data, "CDetectionUnit|PMT 1|State|7", "Active", 0);
        push_leica_instrument_record(&mut data, "CDetectionUnit|PMT 1|VideoOffset|7", "12.5", 0);
        push_leica_instrument_record(&mut data, "CDetectionUnit|PMT 1|HighVoltage|7", "650", 0);
        push_leica_instrument_record(&mut data, "CXYZStage|Stage|XPos|0", "123.25", 0);
        push_leica_instrument_record(&mut data, "CXYZStage|Stage|YPos|0", "-45.5", 0);
        push_leica_instrument_record(&mut data, "CXYZStage|Stage|ZPos|0", "88.75", 0);
        push_leica_instrument_record(
            &mut data,
            "CSpectrophotometerUnit|Channel 1|Stain|0",
            "FITC",
            0,
        );
        push_leica_instrument_record(
            &mut data,
            "CSpectrophotometerUnit|Channel 1|Wavelength|0",
            "500",
            0,
        );
        push_leica_instrument_record(
            &mut data,
            "CSpectrophotometerUnit|Channel 1|Wavelength|1",
            "550",
            0,
        );
        push_leica_instrument_record(
            &mut data,
            "CTurret|Obj|Objective|0",
            "HC PL APO 40x1.30 Oil",
            0,
        );
        push_leica_instrument_record(&mut data, "CTurret|Obj|OrderNumber|0", "OBJ-123", 0);
        push_leica_instrument_record(&mut data, "CTurret|Obj|RefractionIndex|0", "1.515", 0);
        push_leica_instrument_record(&mut data, "dblPinhole", "0.00007", 0);

        let mut lut = Vec::new();
        push_i32(&mut lut, 1);
        push_i32(&mut lut, 6815843);
        push_i32(&mut lut, 1);
        lut.push(0);
        push_leica_len_string(&mut lut, "");
        push_leica_len_string(&mut lut, "");
        push_leica_len_string(&mut lut, "green");
        lut.extend_from_slice(&[0; 8]);

        let mut ifd = HeaderIfd::new();
        ifd.insert(FILTERSET, 0);
        let lut_offset = data.len();
        data.extend_from_slice(&lut);
        ifd.insert(LUTDESC, lut_offset);

        let instrument = parse_leica_instrument_metadata(&data, true, &ifd, 1);
        let colors = parse_leica_lut(&data, true, lut_offset);
        let mut meta_map = HashMap::new();
        insert_leica_instrument_metadata(&mut meta_map, &instrument, &colors, 1);
        insert_leica_plane_metadata(&mut meta_map, &instrument, 2);

        assert_metadata_string(&meta_map, "channel.0.name", "FITC");
        assert_metadata_float(&meta_map, "channel.0.pinhole_size", 70.0);
        assert_metadata_int(&meta_map, "channel.0.color", 0x00ff00ff);
        assert_metadata_string(&meta_map, "Block 1 CDetectionUnit|PMT 1|State|7", "Active");
        assert_metadata_string(
            &meta_map,
            "Block 1 CSpectrophotometerUnit|Channel 1|Stain|0",
            "FITC",
        );
        assert_metadata_string(&meta_map, "Block 1 dblPinhole", "0.00007");
        assert_metadata_string(&meta_map, "channel.0.detector_ref", "Detector:0:0");
        assert_metadata_float(&meta_map, "instrument.filter.0.cut_in", 500.0);
        assert_metadata_float(&meta_map, "instrument.filter.0.cut_out", 550.0);
        assert_metadata_float(
            &meta_map,
            "instrument.objective.0.nominal_magnification",
            40.0,
        );
        assert_metadata_string(&meta_map, "instrument.objective.0.serial_number", "OBJ-123");
        assert_metadata_float(&meta_map, "instrument.objective.0.refractive_index", 1.515);
        assert_metadata_float(&meta_map, "plane.0.position_x", 123.25);
        assert_metadata_float(&meta_map, "plane.0.position_y", -45.5);
        assert_metadata_float(&meta_map, "plane.1.position_x", 123.25);
        assert_metadata_float(&meta_map, "plane.1.position_y", -45.5);
        assert_metadata_string(&meta_map, "image.stage_label.name", "Position");
        assert_metadata_float(&meta_map, "image.stage_label.z", 88.75);

        let mut meta = ImageMetadata {
            size_x: 1,
            size_y: 1,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            image_count: 2,
            ..Default::default()
        };
        meta.series_metadata = meta_map;
        let ome = crate::common::ome_metadata::OmeMetadata::from_image_metadata(&meta);
        let stage_label = ome.images[0].stage_label.as_ref().unwrap();
        assert_eq!(stage_label.name.as_deref(), Some("Position"));
        assert_eq!(stage_label.z, Some(88.75));
    }

    #[test]
    fn lei_reads_header_chain_at_java_offset_and_uses_tiff_dimensions() {
        let dir = temp_dir("header_offset");
        let tiff = dir.join("plane0.tif");
        let lei = dir.join("sample.lei");
        let meta = ImageMetadata {
            size_x: 2,
            size_y: 3,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type: PixelType::Uint8,
            bits_per_pixel: 8,
            image_count: 1,
            ..Default::default()
        };
        let plane = vec![1, 2, 3, 4, 5, 6];
        ImageWriter::save(&tiff, &meta, std::slice::from_ref(&plane)).unwrap();
        std::fs::write(&lei, minimal_lei("plane0.tif", 99, 88)).unwrap();

        let mut reader = LeicaReader::new();
        reader.set_id(&lei).unwrap();

        assert_eq!(reader.series_count(), 1);
        assert_eq!(reader.metadata().size_x, 2);
        assert_eq!(reader.metadata().size_y, 3);
        assert_eq!(reader.open_bytes(0).unwrap(), plane);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn lei_missing_companion_returns_blank_plane_after_initialization() {
        let missing = temp_path("missing.tif");
        let meta = ImageMetadata {
            size_x: 2,
            size_y: 2,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type: PixelType::Uint16,
            bits_per_pixel: 16,
            image_count: 1,
            ..Default::default()
        };
        let mut reader = LeicaReader {
            path: None,
            series_list: vec![LeiSeries {
                meta,
                files: vec![missing],
            }],
            series: 0,
        };

        assert_eq!(reader.open_bytes(0).unwrap(), vec![0; 8]);
    }

    #[test]
    fn lei_companion_tiff_page_uses_exact_index() {
        let tiff = temp_path("single_page.tif");
        let meta = ImageMetadata {
            size_x: 1,
            size_y: 1,
            size_z: 2,
            size_c: 1,
            size_t: 1,
            pixel_type: PixelType::Uint8,
            bits_per_pixel: 8,
            image_count: 2,
            ..Default::default()
        };
        let tiff_meta = ImageMetadata {
            size_z: 1,
            image_count: 1,
            ..meta.clone()
        };
        ImageWriter::save(&tiff, &tiff_meta, &[vec![17]]).unwrap();
        let mut reader = LeicaReader {
            path: None,
            series_list: vec![LeiSeries {
                meta,
                files: vec![tiff.clone()],
            }],
            series: 0,
        };

        let err = reader.open_bytes(1).unwrap_err();

        assert!(
            matches!(err, BioFormatsError::Format(ref message) if message.contains("TIFF page 1 out of range")),
            "unexpected error: {err:?}"
        );
        let _ = std::fs::remove_file(tiff);
    }

    #[test]
    fn lei_raw_companion_reads_flat_planes_like_java() {
        let dir = temp_dir("raw_companion");
        let raw = dir.join("sample.raw");
        let lei = dir.join("sample.lei");
        std::fs::write(&raw, [1u8, 2, 3, 4, 5, 6, 7, 8]).unwrap();
        std::fs::write(&lei, minimal_lei("sample.raw", 2, 2)).unwrap();

        let mut reader = LeicaReader::new();
        reader.set_id(&raw).unwrap();

        assert_eq!(reader.series_count(), 1);
        assert_eq!(reader.metadata().size_x, 2);
        assert_eq!(reader.metadata().size_y, 2);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4]);
        assert_eq!(reader.open_bytes_region(0, 1, 0, 1, 2).unwrap(), vec![2, 4]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn lei_tiff_companion_uses_series_name_hint_before_stem_fallback_like_java() {
        let dir = temp_dir("tiff_series_name_hint");
        let tiff = dir.join("renamed.tif");
        let hinted = dir.join("hinted.lei");
        let stem = dir.join("renamed.lei");
        let meta = ImageMetadata {
            size_x: 1,
            size_y: 1,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type: PixelType::Uint8,
            bits_per_pixel: 8,
            image_count: 1,
            ..Default::default()
        };
        ImageWriter::save(&tiff, &meta, &[vec![42]]).unwrap();
        crate::tiff::overwrite_comment(&tiff, "[Acquisition Parameters]\nSeries Name=hinted.lei\n")
            .unwrap();
        std::fs::write(&hinted, minimal_lei("renamed.tif", 1, 1)).unwrap();
        std::fs::write(&stem, minimal_lei("renamed.tif", 9, 9)).unwrap();

        assert_eq!(find_lei_file(&tiff).as_deref(), Some(hinted.as_path()));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn lei_tiff_companion_falls_back_to_sibling_lei_like_java() {
        let dir = temp_dir("tiff_any_sibling_fallback");
        let tiff = dir.join("renamed.tif");
        let sibling = dir.join("dataset.lei");
        let meta = ImageMetadata {
            size_x: 1,
            size_y: 1,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type: PixelType::Uint8,
            bits_per_pixel: 8,
            image_count: 1,
            ..Default::default()
        };
        ImageWriter::save(&tiff, &meta, &[vec![42]]).unwrap();
        crate::tiff::overwrite_comment(&tiff, "plain TIFF").unwrap();
        std::fs::write(&sibling, minimal_lei("renamed.tif", 1, 1)).unwrap();

        assert_eq!(find_lei_file(&tiff).as_deref(), Some(sibling.as_path()));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn lei_ome_metadata_projects_parsed_physical_sizes_and_channel_names() {
        let mut series_metadata = HashMap::new();
        series_metadata.insert("physicalSizeX".into(), MetadataValue::Float(0.25));
        series_metadata.insert("physicalSizeY".into(), MetadataValue::Float(0.5));
        series_metadata.insert("physicalSizeZ".into(), MetadataValue::Float(1.5));
        series_metadata.insert("timeIncrement".into(), MetadataValue::Float(2.0));
        series_metadata.insert(
            "channel.0.name".into(),
            MetadataValue::String("DAPI".into()),
        );
        series_metadata.insert(
            "channel.0.emission_wavelength".into(),
            MetadataValue::Float(520.0),
        );
        series_metadata.insert(
            "channel.0.excitation_wavelength".into(),
            MetadataValue::Float(488.0),
        );
        series_metadata.insert("channel.0.pinhole_size".into(), MetadataValue::Float(70.0));
        series_metadata.insert("channel.0.color".into(), MetadataValue::Int(0x00ff00ff));
        series_metadata.insert(
            "channel.0.detector_ref".into(),
            MetadataValue::String("Detector:0:0".into()),
        );
        series_metadata.insert(
            "channel.0.detector_settings_offset".into(),
            MetadataValue::Float(12.5),
        );
        series_metadata.insert(
            "channel.0.detector_settings_voltage".into(),
            MetadataValue::Float(650.0),
        );
        series_metadata.insert(
            "channel.0.emission_filter_ref".into(),
            MetadataValue::String("Filter:0:0".into()),
        );
        series_metadata.insert(
            "instrument.detector.0.id".into(),
            MetadataValue::String("Detector:0:0".into()),
        );
        series_metadata.insert(
            "instrument.detector.0.type".into(),
            MetadataValue::String("PMT".into()),
        );
        series_metadata.insert(
            "instrument.detector.0.model".into(),
            MetadataValue::String("PMT 1".into()),
        );
        series_metadata.insert(
            "instrument.detector.0.offset".into(),
            MetadataValue::Float(12.5),
        );
        series_metadata.insert(
            "instrument.filter.0.id".into(),
            MetadataValue::String("Filter:0:0".into()),
        );
        series_metadata.insert(
            "instrument.filter.0.model".into(),
            MetadataValue::String("Channel 1".into()),
        );
        series_metadata.insert(
            "instrument.filter.0.cut_in".into(),
            MetadataValue::Float(500.0),
        );
        series_metadata.insert(
            "instrument.filter.0.cut_out".into(),
            MetadataValue::Float(550.0),
        );
        series_metadata.insert(
            "instrument.objective.0.id".into(),
            MetadataValue::String("Objective:0:0".into()),
        );
        series_metadata.insert(
            "instrument.objective.0.model".into(),
            MetadataValue::String("HC PL APO".into()),
        );
        series_metadata.insert(
            "instrument.objective.0.nominal_magnification".into(),
            MetadataValue::Float(40.0),
        );
        series_metadata.insert(
            "instrument.objective.0.lens_na".into(),
            MetadataValue::Float(1.3),
        );
        series_metadata.insert(
            "instrument.objective.0.serial_number".into(),
            MetadataValue::String("OBJ-123".into()),
        );
        series_metadata.insert(
            "instrument.objective.0.refractive_index".into(),
            MetadataValue::Float(1.515),
        );
        series_metadata.insert(
            "image.name".into(),
            MetadataValue::String("Series A".into()),
        );
        series_metadata.insert(
            "image.description".into(),
            MetadataValue::String("Leica description".into()),
        );
        series_metadata.insert(
            "acquisition_date".into(),
            MetadataValue::String("2024-01-02T03:04:05".into()),
        );
        series_metadata.insert("plane.0.delta_t".into(), MetadataValue::Float(0.0));
        series_metadata.insert("plane.0.exposure_time".into(), MetadataValue::Float(0.25));
        series_metadata.insert("plane.0.position_x".into(), MetadataValue::Float(123.25));
        series_metadata.insert("plane.0.position_y".into(), MetadataValue::Float(-45.5));
        let meta = ImageMetadata {
            size_x: 2,
            size_y: 2,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type: PixelType::Uint8,
            bits_per_pixel: 8,
            image_count: 1,
            series_metadata,
            ..Default::default()
        };
        let reader = LeicaReader {
            path: None,
            series_list: vec![LeiSeries {
                meta,
                files: vec![temp_path("unused.raw")],
            }],
            series: 0,
        };

        let ome = reader.ome_metadata().unwrap();
        let image = &ome.images[0];
        assert_eq!(image.name.as_deref(), Some("Series A"));
        assert_eq!(image.description.as_deref(), Some("Leica description"));
        assert_eq!(
            image.acquisition_date.as_deref(),
            Some("2024-01-02T03:04:05")
        );
        assert_eq!(image.physical_size_x, Some(0.25));
        assert_eq!(image.physical_size_y, Some(0.5));
        assert_eq!(image.physical_size_z, Some(1.5));
        assert_eq!(image.time_increment, Some(2.0));
        assert_eq!(image.planes[0].delta_t, Some(0.0));
        assert_eq!(image.planes[0].exposure_time, Some(0.25));
        assert_eq!(image.planes[0].position_x, Some(123.25));
        assert_eq!(image.planes[0].position_y, Some(-45.5));
        let channel = &image.channels[0];
        assert_eq!(channel.name.as_deref(), Some("DAPI"));
        assert_eq!(channel.emission_wavelength, Some(520.0));
        assert_eq!(channel.excitation_wavelength, Some(488.0));
        assert_eq!(channel.pinhole_size, Some(70.0));
        assert_eq!(channel.color, Some(0x00ff00ff));
        assert_eq!(channel.detector_ref.as_deref(), Some("Detector:0:0"));
        assert_eq!(channel.detector_settings_offset, Some(12.5));
        assert_eq!(channel.detector_settings_voltage, Some(650.0));
        assert_eq!(image.instrument_ref, Some(0));
        assert_eq!(image.objective_ref, Some(0));
        assert_eq!(
            image.light_paths[0].emission_filter_ids,
            vec!["Filter:0:0".to_string()]
        );
        let instrument = &ome.instruments[0];
        assert_eq!(
            instrument.detectors[0].detector_type.as_deref(),
            Some("PMT")
        );
        assert_eq!(instrument.detectors[0].model.as_deref(), Some("PMT 1"));
        assert_eq!(instrument.detectors[0].offset, Some(12.5));
        assert_eq!(instrument.filters[0].cut_in, Some(500.0));
        assert_eq!(instrument.filters[0].cut_out, Some(550.0));
        assert_eq!(instrument.objectives[0].model.as_deref(), Some("HC PL APO"));
        assert_eq!(
            instrument.objectives[0].serial_number.as_deref(),
            Some("OBJ-123")
        );
        assert_eq!(instrument.objectives[0].nominal_magnification, Some(40.0));
        assert_eq!(instrument.objectives[0].lens_na, Some(1.3));
        assert_eq!(image.objective_settings_refractive_index, Some(1.515));
        let xml = ome.to_ome_xml(reader.metadata());
        assert!(xml.contains(r#"<InstrumentRef ID="Instrument:0"/>"#));
        assert!(xml.contains(r#"SerialNumber="OBJ-123""#));
        assert!(xml.contains(r#"<ObjectiveSettings ID="Objective:0:0" RefractiveIndex="1.515"/>"#));
        assert!(
            xml.contains(r#"<DetectorSettings ID="Detector:0:0" Offset="12.5" Voltage="650"/>"#)
        );
        assert!(xml.contains(
            r#"<Plane TheZ="0" TheC="0" TheT="0" DeltaT="0" ExposureTime="0.25" PositionX="123.25" PositionY="-45.5"/>"#
        ));
        assert!(xml.contains(r#"<EmissionFilterRef ID="Filter:0:0"/>"#));
        let original_metadata = ome
            .annotations
            .iter()
            .find_map(|annotation| match annotation {
                crate::common::ome_metadata::OmeAnnotation::MapAnnotation {
                    id,
                    namespace,
                    values,
                } if id.as_deref() == Some("Annotation:OriginalMetadata:0")
                    && namespace.as_deref() == Some("openmicroscopy.org/OriginalMetadata") =>
                {
                    Some(values)
                }
                _ => None,
            });
        let original_metadata = original_metadata.expect("Leica original metadata annotation");
        assert!(original_metadata.iter().any(|(key, value)| {
            key == "instrument.objective.0.serial_number" && value == "OBJ-123"
        }));
        assert!(original_metadata.iter().any(|(key, value)| {
            key == "instrument.objective.0.refractive_index" && value == "1.515"
        }));
    }

    #[test]
    fn lei_timeinfo_projects_java_acquisition_date_and_plane_delta_t() {
        let mut data = Vec::new();
        push_i32(&mut data, 1);
        push_i32(&mut data, 116);
        push_i32(&mut data, 116);
        push_i32(&mut data, 2);
        push_i32(&mut data, 1);
        push_i32(&mut data, 2);
        push_leica_fixed_utf16_string(&mut data, "2024:01:02,03:04:05", 32);
        push_leica_fixed_utf16_string(&mut data, "2024:01:02,03:04:08", 32);

        let mut meta_map = HashMap::new();
        parse_leica_time_metadata(&data, true, 0, &mut meta_map, 2);

        assert_eq!(
            meta_map
                .get("acquisition_date")
                .map(|v| v.to_string())
                .as_deref(),
            Some("2024-01-02T03:04:05")
        );
        assert!(matches!(
            meta_map.get("plane.0.delta_t"),
            Some(MetadataValue::Float(v)) if *v == 0.0
        ));
        assert!(matches!(
            meta_map.get("plane.1.delta_t"),
            Some(MetadataValue::Float(v)) if *v == 3.0
        ));
        assert_eq!(
            meta_map
                .get("Timestamp.1")
                .map(|v| v.to_string())
                .as_deref(),
            Some("2024:01:02,03:04:08")
        );
    }

    #[test]
    fn lei_experiment_and_channel_tags_preserve_java_original_metadata() {
        let mut experiment = Vec::new();
        experiment.extend_from_slice(&[0; 8]);
        push_leica_len_string(&mut experiment, "Experiment description");
        push_leica_len_string(&mut experiment, "lei");
        push_leica_len_string(&mut experiment, "LEICA");
        push_leica_len_string(&mut experiment, "tif");

        let mut channel = Vec::new();
        push_i32(&mut channel, 1);
        push_f64(&mut channel, 500.0);
        channel.extend_from_slice(&[0; 4]);
        push_f64(&mut channel, 550.0);
        channel.extend_from_slice(&[0; 4]);
        push_f64(&mut channel, 2.5);
        push_f64(&mut channel, -1.25);

        let mut meta_map = HashMap::new();
        parse_leica_experiment_metadata(&experiment, true, 0, &mut meta_map);
        parse_leica_channel_metadata(&channel, true, 0, &mut meta_map);

        assert_metadata_string(&meta_map, "Image Description", "Experiment description");
        assert_metadata_string(&meta_map, "Main file extension", "lei");
        assert_metadata_string(&meta_map, "Image format identifier", "LEICA");
        assert_metadata_string(&meta_map, "Single image extension", "tif");
        assert_metadata_float(&meta_map, "Band #1 Lower wavelength", 500.0);
        assert_metadata_float(&meta_map, "Band #1 Higher wavelength", 550.0);
        assert_metadata_float(&meta_map, "Band #1 Gain", 2.5);
        assert_metadata_float(&meta_map, "Band #1 Offset", -1.25);
    }
}
