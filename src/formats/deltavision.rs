//! Applied Precision DeltaVision (.dv / .r3d) format reader.
//!
//! DeltaVision uses the PRIISM image file format — a 1024-byte header (possibly
//! followed by an extended header) and then raw pixel planes.
//!
//! Magic: int16 at offset 96 == -16224 (bytes [0xA0, 0xC0] little-endian).

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;

const HEADER_SIZE: usize = 1024;
const DV_MAGIC_LE: i16 = -16224; // 0xC0A0 as signed int16 LE

fn r_i16(b: &[u8], off: usize, le: bool) -> i16 {
    let bytes = [b[off], b[off + 1]];
    if le {
        i16::from_le_bytes(bytes)
    } else {
        i16::from_be_bytes(bytes)
    }
}
fn r_u16(b: &[u8], off: usize, le: bool) -> u16 {
    let bytes = [b[off], b[off + 1]];
    if le {
        u16::from_le_bytes(bytes)
    } else {
        u16::from_be_bytes(bytes)
    }
}
fn r_i32(b: &[u8], off: usize, le: bool) -> i32 {
    let bytes = [b[off], b[off + 1], b[off + 2], b[off + 3]];
    if le {
        i32::from_le_bytes(bytes)
    } else {
        i32::from_be_bytes(bytes)
    }
}
fn r_f32(b: &[u8], off: usize, le: bool) -> f32 {
    let bytes = [b[off], b[off + 1], b[off + 2], b[off + 3]];
    if le {
        f32::from_le_bytes(bytes)
    } else {
        f32::from_be_bytes(bytes)
    }
}

fn positive_i32_dim(value: i32, label: &str) -> Result<u32> {
    if value <= 0 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "DeltaVision header has non-positive {label}"
        )));
    }
    Ok(value as u32)
}

/// Pixel type codes used in .dv files (matches Java DeltavisionReader.getPixelType)
fn dv_pixel_type(mode: i32) -> (PixelType, u8) {
    match mode {
        0 => (PixelType::Uint8, 8),
        1 => (PixelType::Int16, 16),
        2 => (PixelType::Float32, 32),
        3 => (PixelType::Int16, 16), // 16 bit complex — report as int16
        4 => (PixelType::Float32, 32), // 64 bit complex — report as float
        6 => (PixelType::Uint16, 16),
        7 => (PixelType::Int32, 32),
        8 => (PixelType::Float64, 64),
        _ => (PixelType::Uint8, 8),
    }
}

pub struct DeltavisionReader {
    path: Option<PathBuf>,
    series: Vec<ImageMetadata>,
    current_series: usize,
    data_offset: u64,
    image_sequence: String,
    samples_per_pixel: u32,
    split_positions: bool,
    positions_in_time: bool,
    stage_ordering: StageOrdering,
    extended_headers: Vec<DvExtendedHeader>,
    channel_emission_wavelengths: Vec<Option<f64>>,
    /// Per-channel neutral-density filter values (mirrors Java `ndFilters`).
    ///
    /// Java seeds each entry from the first plane's extended-header `ndFilter`
    /// for that channel (`if (ndFilters[w] == null) ndFilters[w] = hdr.ndFilter`)
    /// and lets the `.log` companion's "ND filter" lines override it. `None`
    /// entries (e.g. a "BLANK" log value) leave the channel without an NDFilter.
    nd_filters: Vec<Option<f64>>,
    /// Parsed metadata from the `.log` deconvolution companion file (if present).
    log_data: Option<LogFileData>,
}

impl DeltavisionReader {
    pub fn new() -> Self {
        DeltavisionReader {
            path: None,
            series: Vec::new(),
            current_series: 0,
            data_offset: HEADER_SIZE as u64,
            image_sequence: "ZTWP".to_string(),
            samples_per_pixel: 1,
            split_positions: false,
            positions_in_time: false,
            stage_ordering: StageOrdering::default(),
            extended_headers: Vec::new(),
            channel_emission_wavelengths: Vec::new(),
            nd_filters: Vec::new(),
            log_data: None,
        }
    }
}

/// Objective metadata projected from the `.log` companion (mirrors the
/// `store.setObjective*` calls in Java `DeltavisionReader.parseLogFile`).
#[derive(Debug, Clone, Default)]
struct LogObjective {
    manufacturer: Option<String>,
    nominal_magnification: Option<f64>,
    lens_na: Option<f64>,
    correction: Option<String>,
    immersion: Option<String>,
    model: Option<String>,
    id: Option<String>,
}

/// Detector metadata projected from the `.log` companion.
#[derive(Debug, Clone, Default)]
struct LogDetector {
    detector_type: Option<String>,
    model: Option<String>,
    gain: Option<f64>,
}

/// Result of parsing the `.log` deconvolution companion file. Mirrors the OME
/// projections performed by Java `DeltavisionReader.parseLogFile`.
#[derive(Debug, Clone, Default)]
struct LogFileData {
    objective: LogObjective,
    detector: LogDetector,
    physical_size_x: Option<f64>,
    physical_size_y: Option<f64>,
    physical_size_z: Option<f64>,
    acquisition_date: Option<String>,
    channel_names: Vec<String>,
    /// Distinct "ND filter" values from the log (Java `filters`), each divided
    /// by 100. A `None` entry mirrors a value Java could not parse (e.g. the
    /// "BLANK" default for deconvolved data), which still occupies a slot.
    nd_filters: Vec<Option<f64>>,
}

impl LogFileData {
    fn has_objective(&self) -> bool {
        let o = &self.objective;
        o.manufacturer.is_some()
            || o.nominal_magnification.is_some()
            || o.lens_na.is_some()
            || o.correction.is_some()
            || o.immersion.is_some()
            || o.model.is_some()
            || o.id.is_some()
    }

    fn has_detector(&self) -> bool {
        let d = &self.detector;
        d.detector_type.is_some() || d.model.is_some() || d.gain.is_some()
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct StageOrdering {
    x_tiles: u32,
    y_tiles: u32,
    backwards_x: bool,
    backwards_y: bool,
}

#[derive(Debug, Clone, Copy)]
struct DvExtendedHeader {
    photosensor_reading: f32,
    time_stamp_seconds: f32,
    stage_x: f32,
    stage_y: f32,
    stage_z: f32,
    min_intensity: f32,
    max_intensity: f32,
    exposure_time: f32,
    nd_filter: f32,
    excitation_wavelength: f32,
    emission_wavelength: f32,
    intensity_scaling: f32,
    energy_conversion_factor: f32,
}

impl DvExtendedHeader {
    fn from_floats(values: &[f32]) -> Self {
        let mut nd_filter = values[9];
        if nd_filter >= 1.0 {
            nd_filter /= 100.0;
        }
        Self {
            photosensor_reading: values[0],
            time_stamp_seconds: values[1],
            stage_x: values[2],
            stage_y: values[3],
            stage_z: values[4],
            min_intensity: values[5],
            max_intensity: values[6],
            exposure_time: values[8],
            nd_filter,
            excitation_wavelength: values[10],
            emission_wavelength: values[11],
            intensity_scaling: values[12],
            energy_conversion_factor: values[13],
        }
    }
}

fn dv_image_sequence(sequence: i32) -> &'static str {
    match sequence {
        0 => "ZTWP",
        1 => "WZTP",
        2 => "ZWTP",
        3 => "ZPWT",
        4 => "ZWPT",
        5 => "WZPT",
        6 => "WPTZ",
        7 => "PWTZ",
        8 => "PTWZ",
        9 => "PZWT",
        10 => "PWZT",
        11 => "WPZT",
        12 => "WTPZ",
        13 => "TWPZ",
        14 => "TPWZ",
        65536 => "WZTP",
        _ => "ZTWP",
    }
}

fn dv_dimension_order(image_sequence: &str) -> DimensionOrder {
    match image_sequence.replace('W', "C").replace('P', "").as_str() {
        "CTZ" => DimensionOrder::XYCTZ,
        "CZT" => DimensionOrder::XYCZT,
        "TCZ" => DimensionOrder::XYTCZ,
        "TZC" => DimensionOrder::XYTZC,
        "ZCT" => DimensionOrder::XYZCT,
        "ZTC" => DimensionOrder::XYZTC,
        _ => DimensionOrder::XYZCT,
    }
}

fn raster_to_zct(index: u32, meta: &ImageMetadata) -> (u32, u32, u32) {
    let dims: &[(char, u32)] = match meta.dimension_order {
        DimensionOrder::XYCTZ => &[('C', meta.size_c), ('T', meta.size_t), ('Z', meta.size_z)],
        DimensionOrder::XYCZT => &[('C', meta.size_c), ('Z', meta.size_z), ('T', meta.size_t)],
        DimensionOrder::XYTCZ => &[('T', meta.size_t), ('C', meta.size_c), ('Z', meta.size_z)],
        DimensionOrder::XYTZC => &[('T', meta.size_t), ('Z', meta.size_z), ('C', meta.size_c)],
        DimensionOrder::XYZCT => &[('Z', meta.size_z), ('C', meta.size_c), ('T', meta.size_t)],
        DimensionOrder::XYZTC => &[('Z', meta.size_z), ('T', meta.size_t), ('C', meta.size_c)],
    };

    let mut remaining = index;
    let mut z = 0;
    let mut c = 0;
    let mut t = 0;
    for (dim, len) in dims {
        let len = (*len).max(1);
        let value = remaining % len;
        remaining /= len;
        match dim {
            'Z' => z = value,
            'C' => c = value,
            'T' => t = value,
            _ => {}
        }
    }
    (z, c, t)
}

fn dv_plane_index(
    image_sequence: &str,
    z: u32,
    c: u32,
    t: u32,
    p: u32,
    panel_count: u32,
    meta: &ImageMetadata,
) -> u64 {
    dv_plane_index_for_sizes(
        image_sequence,
        z,
        c,
        t,
        p,
        panel_count,
        meta.size_z,
        meta.size_c,
        meta.size_t,
        meta.is_rgb,
    )
}

fn dv_plane_index_for_sizes(
    image_sequence: &str,
    z: u32,
    c: u32,
    t: u32,
    p: u32,
    panel_count: u32,
    size_z: u32,
    size_c: u32,
    size_t: u32,
    is_rgb: bool,
) -> u64 {
    let mut index = 0u64;
    let mut stride = 1u64;
    for dim in image_sequence.chars() {
        let (coord, len) = match dim {
            'Z' => (z as u64, size_z.max(1) as u64),
            'W' => {
                let len = if is_rgb { 1 } else { size_c.max(1) };
                let coord = if is_rgb { 0 } else { c };
                (coord as u64, len as u64)
            }
            'T' => (t as u64, size_t.max(1) as u64),
            'P' => (p as u64, panel_count.max(1) as u64),
            _ => (0, 1),
        };
        index += coord * stride;
        stride *= len;
    }
    index
}

fn dv_old_position_plane_index_for_sizes(
    image_sequence: &str,
    z: u32,
    c: u32,
    t: u32,
    series: u32,
    series_count: u32,
    size_z: u32,
    size_c: u32,
    size_t: u32,
    is_rgb: bool,
) -> u64 {
    let mut index = 0u64;
    let mut stride = 1u64;
    for dim in image_sequence.chars() {
        match dim {
            'Z' => {
                index += z as u64 * stride;
                stride *= size_z.max(1) as u64;
            }
            'W' => {
                let len = if is_rgb { 1 } else { size_c.max(1) };
                let coord = if is_rgb { 0 } else { c };
                index += coord as u64 * stride;
                stride *= len as u64;
            }
            'T' => {
                index += series as u64 * stride;
                stride *= series_count.max(1) as u64;
                index += t as u64 * stride;
                stride *= size_t.max(1) as u64;
            }
            'P' => {}
            _ => {}
        }
    }
    index
}

fn read_extended_headers(
    f: &mut File,
    le: bool,
    ext_hdr_size: u64,
    image_count: u32,
    ints_per_section: u16,
    floats_per_section: u16,
) -> Result<Vec<DvExtendedHeader>> {
    if ext_hdr_size == 0 || image_count == 0 || floats_per_section < 14 {
        return Ok(Vec::new());
    }
    let int_bytes = ints_per_section as u64 * 4;
    let section_bytes = (ints_per_section as u64 + floats_per_section as u64) * 4;
    if section_bytes == 0 {
        return Ok(Vec::new());
    }
    let complete_sections = ext_hdr_size / section_bytes;
    if complete_sections < image_count as u64 {
        return Ok(Vec::new());
    }

    let mut headers = Vec::with_capacity(image_count as usize);
    let mut raw = [0u8; 56];
    for i in 0..image_count as u64 {
        f.seek(SeekFrom::Start(
            HEADER_SIZE as u64 + int_bytes + i * section_bytes,
        ))
        .map_err(BioFormatsError::Io)?;
        f.read_exact(&mut raw).map_err(BioFormatsError::Io)?;
        let mut values = [0.0f32; 14];
        for (j, value) in values.iter_mut().enumerate() {
            let off = j * 4;
            let bytes = [raw[off], raw[off + 1], raw[off + 2], raw[off + 3]];
            *value = if le {
                f32::from_le_bytes(bytes)
            } else {
                f32::from_be_bytes(bytes)
            };
        }
        headers.push(DvExtendedHeader::from_floats(&values));
    }
    Ok(headers)
}

fn stage_key(hdr: &DvExtendedHeader) -> (u32, u32) {
    (hdr.stage_x.to_bits(), hdr.stage_y.to_bits())
}

fn stage_ordering(extended_headers: &[DvExtendedHeader], series_count: u32) -> StageOrdering {
    let mut unique_x = Vec::<u32>::new();
    let mut unique_y = Vec::<u32>::new();
    let mut x_values = Vec::<f32>::new();
    let mut y_values = Vec::<f32>::new();
    let mut has_zero_x = false;
    let mut has_zero_y = false;

    for h in extended_headers {
        if h.stage_x.abs() > f32::EPSILON {
            let bits = h.stage_x.to_bits();
            if !unique_x.contains(&bits) {
                unique_x.push(bits);
                x_values.push(h.stage_x);
            }
        } else {
            has_zero_x = true;
        }

        if h.stage_y.abs() > f32::EPSILON {
            let bits = h.stage_y.to_bits();
            if !unique_y.contains(&bits) {
                unique_y.push(bits);
                y_values.push(h.stage_y);
            }
        } else {
            has_zero_y = true;
        }
    }

    let mut ordering = StageOrdering {
        x_tiles: unique_x.len() as u32,
        y_tiles: unique_y.len() as u32,
        backwards_x: x_values
            .get(0..2)
            .is_some_and(|values| values[1] < values[0]),
        backwards_y: y_values
            .get(0..2)
            .is_some_and(|values| values[1] < values[0]),
    };

    if ordering.x_tiles > 1 || ordering.y_tiles > 1 {
        if has_zero_x {
            ordering.x_tiles += 1;
        }
        if has_zero_y {
            ordering.y_tiles += 1;
        }
    }

    normalize_stage_ordering(ordering, series_count)
}

fn normalize_stage_ordering(mut ordering: StageOrdering, series_count: u32) -> StageOrdering {
    if series_count <= 1 {
        return StageOrdering {
            x_tiles: 1,
            y_tiles: 1,
            backwards_x: false,
            backwards_y: false,
        };
    }

    let series_count = series_count.max(1);
    let tiles = ordering.x_tiles.saturating_mul(ordering.y_tiles);
    if tiles > series_count {
        if ordering.x_tiles == series_count {
            ordering.y_tiles = 1;
        } else if ordering.y_tiles == series_count {
            ordering.x_tiles = 1;
        } else {
            ordering.x_tiles = 1;
            ordering.y_tiles = 1;
            ordering.backwards_x = false;
            ordering.backwards_y = false;
        }
    } else if tiles < series_count && (ordering.backwards_x || ordering.backwards_y) {
        if ordering.backwards_x && ordering.y_tiles == 1 {
            ordering.x_tiles = series_count;
        } else if ordering.backwards_y && ordering.x_tiles == 1 {
            ordering.y_tiles = series_count;
        } else {
            ordering.backwards_x = false;
            ordering.backwards_y = false;
        }
    }

    ordering.x_tiles = ordering.x_tiles.max(1);
    ordering.y_tiles = ordering.y_tiles.max(1);
    ordering
}

fn older_position_series_count(
    extended_headers: &[DvExtendedHeader],
    image_sequence: &str,
    size_z: u32,
    size_c: u32,
    size_t: u32,
    is_rgb: bool,
) -> u32 {
    if extended_headers.is_empty() || size_t <= 1 {
        return 1;
    }

    let mut time_positions = Vec::with_capacity(size_t as usize);
    for t in 0..size_t {
        let first_idx = dv_plane_index_for_sizes(
            image_sequence,
            0,
            0,
            t,
            0,
            1,
            size_z,
            size_c,
            size_t,
            is_rgb,
        ) as usize;
        let Some(first_header) = extended_headers.get(first_idx) else {
            return 1;
        };
        let first_key = stage_key(first_header);
        for z in 0..size_z {
            for c in 0..(if is_rgb { 1 } else { size_c }) {
                let idx = dv_plane_index_for_sizes(
                    image_sequence,
                    z,
                    c,
                    t,
                    0,
                    1,
                    size_z,
                    size_c,
                    size_t,
                    is_rgb,
                ) as usize;
                if extended_headers
                    .get(idx)
                    .map(stage_key)
                    .is_none_or(|key| key != first_key)
                {
                    return 1;
                }
            }
        }
        time_positions.push(first_key);
    }

    let mut unique = Vec::new();
    for key in &time_positions {
        if !unique.contains(key) {
            unique.push(*key);
        }
    }
    let n = unique.len() as u32;
    if n <= 1 || n >= size_t || size_t % n != 0 {
        return 1;
    }
    for (i, key) in time_positions.iter().enumerate() {
        if *key != unique[i % unique.len()] {
            return 1;
        }
    }
    n
}

/// Locate the `.log` companion file next to the given DeltaVision file, exactly
/// as Java `DeltavisionReader.findLogFiles` does for the (non-deconvolution)
/// `logFile`. Returns `None` if the current file has no extension or no log
/// companion exists.
fn find_log_file(current_file: &Path) -> Option<PathBuf> {
    let name = current_file.to_str()?;
    // The current file name has no extension -> skip (Java: lastIndexOf(".") == -1).
    if !name.contains('.') {
        return None;
    }

    let log_file: PathBuf = if name.ends_with("_D3D.dv") {
        // <base>_D3D.dv -> <base>.dv.log
        let base = &name[..name.find("_D3D.dv").unwrap()];
        PathBuf::from(format!("{base}.dv.log"))
    } else {
        // <id>.log, falling back to <id-without-ext>.log
        let candidate = PathBuf::from(format!("{name}.log"));
        if candidate.exists() {
            candidate
        } else {
            let base = &name[..name.rfind('.').unwrap()];
            PathBuf::from(format!("{base}.log"))
        }
    };

    if log_file.exists() {
        Some(log_file)
    } else {
        None
    }
}

fn deltavision_name_kind(path: &Path) -> Option<&'static str> {
    let name = path.to_string_lossy().to_ascii_lowercase();
    if name.ends_with(".pnl") {
        None
    } else if name.ends_with(".dv.log") || name.ends_with(".r3d.log") || name.ends_with("_log.txt")
    {
        Some("log")
    } else if name.ends_with(".dv") || name.ends_with(".r3d") || name.ends_with(".r3d_d3d") {
        Some("pixels")
    } else {
        None
    }
}

fn find_matching_dv_file(path: &Path) -> Option<PathBuf> {
    let parent = path.parent()?;
    let file_name = path.file_name()?.to_str()?;
    let base = file_name
        .rsplit_once('.')
        .map_or(file_name, |(base, _)| base);
    let entries = fs::read_dir(parent).ok()?;
    for entry in entries.flatten() {
        let candidate = entry.path();
        let Some(name) = candidate.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let lower = name.to_ascii_lowercase();
        if lower.ends_with(".dv") && name.starts_with(base) {
            return Some(candidate);
        }
    }
    None
}

fn resolve_deltavision_input_path(path: &Path) -> PathBuf {
    let lower = path.to_string_lossy().to_ascii_lowercase();
    let mut resolved = if lower.ends_with(".dv.log") || lower.ends_with(".r3d.log") {
        path.with_extension("")
    } else if lower.ends_with("_log.txt") {
        let name = path.to_string_lossy();
        if let Some(index) = name.rfind('_') {
            PathBuf::from(format!("{}.dv", &name[..index]))
        } else {
            path.to_path_buf()
        }
    } else {
        path.to_path_buf()
    };

    if !resolved.exists() {
        if let Some(candidate) = find_matching_dv_file(&resolved) {
            resolved = candidate;
        }
    }

    resolved
}

/// Convert a DeltaVision `Created` timestamp (`E MMM d HH:mm:ss yyyy`, e.g.
/// "Wed Jul 25 14:00:00 2007") into an ISO-8601 string, mirroring Java's
/// `DateTools.formatDate(line, DATE_FORMATS)`. Returns `None` if unparseable.
fn format_dv_date(line: &str) -> Option<String> {
    let tokens: Vec<&str> = line.split_whitespace().collect();
    // Expect: <weekday> <month> <day> <HH:mm:ss> <year>
    if tokens.len() != 5 {
        return None;
    }
    let month = match tokens[1] {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let day: u32 = tokens[2].parse().ok()?;
    let year: i32 = tokens[4].parse().ok()?;
    let time = tokens[3];
    // Validate the HH:mm:ss component.
    let time_parts: Vec<&str> = time.split(':').collect();
    if time_parts.len() != 3 || time_parts.iter().any(|p| p.parse::<u32>().is_err()) {
        return None;
    }
    Some(format!("{year:04}-{month:02}-{day:02}T{time}"))
}

impl Default for DeltavisionReader {
    fn default() -> Self {
        Self::new()
    }
}

impl DeltavisionReader {
    fn file_plane_index(&self, z: u32, c: u32, t: u32, meta: &ImageMetadata) -> u64 {
        self.file_plane_index_for_series(z, c, t, self.current_series as u32, meta)
    }

    fn file_plane_index_for_series(
        &self,
        z: u32,
        c: u32,
        t: u32,
        series: u32,
        meta: &ImageMetadata,
    ) -> u64 {
        if self.positions_in_time {
            dv_old_position_plane_index_for_sizes(
                &self.image_sequence,
                z,
                c,
                t,
                series,
                self.series_count() as u32,
                meta.size_z,
                meta.size_c,
                meta.size_t,
                meta.is_rgb,
            )
        } else {
            let panel = if self.split_positions { series } else { 0 };
            dv_plane_index(
                &self.image_sequence,
                z,
                c,
                t,
                panel,
                self.series_count() as u32,
                meta,
            )
        }
    }

    fn stage_metadata_series_index(&self, series: usize) -> u32 {
        let ordering = self.stage_ordering;
        if !(ordering.backwards_x || ordering.backwards_y) {
            return series as u32;
        }

        let x_tiles = ordering.x_tiles.max(1) as usize;
        let y_tiles = ordering.y_tiles.max(1) as usize;
        let x = series % x_tiles;
        let y = series / x_tiles;
        let x_index = if ordering.backwards_x {
            x_tiles - x - 1
        } else {
            x
        };
        let y_index = if ordering.backwards_y {
            y_tiles - y - 1
        } else {
            y
        };
        (y_index * x_tiles + x_index) as u32
    }

    /// Faithful translation of Java `DeltavisionReader.parseLogFile`. Reads the
    /// line-oriented `key: value` `.log` companion located next to the DV file,
    /// records the same global-metadata keys Java records (into every series'
    /// `series_metadata`, since this port has no shared global-metadata map) and
    /// collects the objective/detector/channel/date projections that Java pushes
    /// into the OME `MetadataStore`. Returns `true` if a log file was parsed.
    fn parse_log_file(&mut self) -> bool {
        let Some(current_file) = self.path.clone() else {
            return false;
        };
        let Some(log_file) = find_log_file(&current_file) else {
            return false;
        };
        let Ok(contents) = std::fs::read_to_string(&log_file) else {
            return false;
        };

        // DataTools.readFile(...).split("[\r\n]") -- split on every CR or LF.
        let lines: Vec<&str> = contents.split(['\r', '\n']).collect();

        let mut prefix = String::new();
        let mut current_image: u32 = 0;
        let mut list_counters: HashMap<String, usize> = HashMap::new();

        let mut log = LogFileData::default();
        // Accumulated global-metadata keys (Java addGlobalMeta / addGlobalMetaList).
        let mut global_meta: Vec<(String, String)> = Vec::new();

        for raw_line in lines {
            let line = raw_line;
            let colon = line.find(':');
            if let Some(colon) =
                colon.filter(|&c| c < line.len() - 1 && !line.starts_with("Created"))
            {
                let key = line[..colon].trim().to_string();
                let mut value = line[colon + 1..].trim().to_string();
                if value.is_empty() && !key.is_empty() {
                    prefix = key.clone();
                }
                global_meta.push((format!("{prefix} {key}"), value.clone()));

                // Objective properties
                if key == "Objective" {
                    // assume first word is the manufacturer's name
                    if let Some(space) = value.find(' ') {
                        let manufacturer = value[..space].to_string();
                        let extra = &value[space + 1..];
                        let tokens: Vec<&str> = extra.split(',').collect();

                        log.objective.manufacturer = Some(manufacturer);

                        let mut magnification = "";
                        let mut na = "";
                        if let Some(first) = tokens.first() {
                            if let Some(end) = first.find('X') {
                                if end > 0 {
                                    magnification = &first[..end];
                                }
                            }
                            if let Some(start) = first.find('/') {
                                na = &first[start + 1..];
                            }
                        }
                        if let Ok(mag) = magnification.trim().parse::<f64>() {
                            log.objective.nominal_magnification = Some(mag);
                        }
                        if let Ok(na) = na.trim().parse::<f64>() {
                            log.objective.lens_na = Some(na);
                        }
                        if tokens.len() >= 2 {
                            log.objective.correction = Some(tokens[1].trim().to_string());
                        }
                        // TODO: Token #2 is the microscope model name.
                        if tokens.len() > 3 {
                            log.objective.model = Some(tokens[3].trim().to_string());
                        }
                    }
                } else if key.eq_ignore_ascii_case("Lens ID") {
                    if let Some(comma) = value.find(',') {
                        value = value[..comma].to_string();
                    }
                    if let Some(space) = value.find(' ') {
                        value = value[space + 1..].to_string();
                    }
                    if value != "null" {
                        log.objective.id = Some(format!("Objective:{value}"));
                        log.objective.correction = Some("Other".to_string());
                        log.objective.immersion = Some("Other".to_string());
                    }
                }
                // Image properties
                else if key == "Pixel Size" {
                    let pixel_sizes: Vec<&str> = value.split(' ').collect();
                    for (q, raw) in pixel_sizes.iter().enumerate() {
                        let size = raw.trim().parse::<f64>().ok();
                        match q {
                            0 => log.physical_size_x = size,
                            1 => log.physical_size_y = size,
                            2 => log.physical_size_z = size,
                            _ => {}
                        }
                    }
                } else if key == "Binning" {
                    log.detector.detector_type = Some("Other".to_string());
                }
                // Camera properties
                else if key == "Type" {
                    log.detector.model = Some(value.clone());
                } else if key == "Gain" {
                    let cleaned = value.replace('X', "");
                    if let Ok(gain) = cleaned.trim().parse::<f64>() {
                        log.detector.gain = Some(gain);
                    }
                }
                // Plane properties
                else if key == "EM filter" {
                    if !log.channel_names.contains(&value) {
                        log.channel_names.push(value.clone());
                    }
                } else if key == "ND filter" {
                    // Java: value.replaceAll("%", ""); nd = parse / 100; dedupe;
                    // on NumberFormatException add null (BLANK is silent).
                    let cleaned = value.replace('%', "");
                    match cleaned.trim().parse::<f64>() {
                        Ok(parsed) => {
                            let nd = parsed / 100.0;
                            if !log.nd_filters.iter().any(|f| *f == Some(nd)) {
                                log.nd_filters.push(Some(nd));
                            }
                        }
                        Err(_) => {
                            log.nd_filters.push(None);
                        }
                    }
                } else if key == "Stage coordinates" {
                    current_image += 1;
                }
                // ("Speed", "Temp Setting") parsed by Java affect detector
                // read-out rate / environment temperature only; not projected here.
            } else if line.starts_with("Image") {
                prefix = line.to_string();
            } else if line.starts_with("Created") {
                let mut date_line = line;
                if line.len() > 8 {
                    date_line = line[8..].trim();
                }
                if let Some(date) = format_dv_date(date_line) {
                    log.acquisition_date = Some(date);
                }
            } else if let Some(rest) = line.strip_prefix("#KEY") {
                // Java: line.substring(line.indexOf(" ")).trim()
                let line = rest.trim_start();
                let split = line.find(':').or_else(|| line.find(' '));
                if let Some(split) = split {
                    let key = line[..split].trim().to_string();
                    let value = line[split + 1..].trim().to_string();
                    global_meta.push((key, value));
                }
            } else if let Some(p) = line.strip_suffix(':') {
                prefix = p.to_string();
            } else if !line.starts_with('#')
                && !line.replace('-', "").is_empty()
                && !prefix.is_empty()
            {
                // addGlobalMetaList(prefix, line): append with an incrementing index.
                let n = list_counters.entry(prefix.clone()).or_insert(0);
                *n += 1;
                global_meta.push((format!("{prefix} #{n}"), line.to_string()));
            }
        }

        let _ = current_image;

        // Mirror Java's addGlobalMeta(...) into every series' metadata map (this
        // port has no shared global-metadata store).
        for meta in self.series.iter_mut() {
            for (k, v) in &global_meta {
                meta.series_metadata
                    .insert(k.clone(), MetadataValue::String(v.clone()));
            }
        }

        self.log_data = Some(log);
        true
    }

    /// Build the per-channel `nd_filters` array, mirroring how Java fills its
    /// `ndFilters[]`: the `.log` companion's "ND filter" values take precedence
    /// (copied per channel in `parseLogFile`), and any channel still without a
    /// value is seeded from the first plane's extended-header `ndFilter`
    /// (`if (ndFilters[w] == null) ndFilters[w] = hdr.ndFilter`). Must run after
    /// `parse_log_file` so the log values win, exactly as in Java `initFile`.
    fn populate_nd_filters(&mut self, size_c: u32) {
        let mut nd_filters: Vec<Option<f64>> = vec![None; size_c as usize];

        // Java parseLogFile: for c in [0, effectiveSizeC): if c < filters.size()
        // ndFilters[c] = filters.get(c).
        if let Some(log) = &self.log_data {
            for (c, slot) in nd_filters.iter_mut().enumerate() {
                if let Some(value) = log.nd_filters.get(c) {
                    *slot = *value;
                }
            }
        }

        // Java initExtraMetadata: seed remaining channels from the first plane's
        // extended-header ndFilter, using series 0's stage-mapped index.
        if !self.extended_headers.is_empty() {
            if let Some(meta) = self.series.first() {
                let metadata_series = self.stage_metadata_series_index(0);
                for (c, slot) in nd_filters.iter_mut().enumerate() {
                    if slot.is_none() {
                        let raw_idx =
                            self.file_plane_index_for_series(0, c as u32, 0, metadata_series, meta)
                                as usize;
                        if let Some(h) = self.extended_headers.get(raw_idx) {
                            *slot = Some(h.nd_filter as f64);
                        }
                    }
                }
            }
        }

        self.nd_filters = nd_filters;
    }
}

impl FormatReader for DeltavisionReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        deltavision_name_kind(path).is_some()
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // Java requires enough bytes to inspect the MRC/MAP collision marker at
        // offset 208, via FormatTools.validStream(stream, 212, true).
        if header.len() < 212 {
            return false;
        }
        // Check magic at offset 96 for both LE and BE
        let le = i16::from_le_bytes([header[96], header[97]]);
        let be = i16::from_be_bytes([header[96], header[97]]);
        let little = if le == DV_MAGIC_LE {
            true
        } else if be == DV_MAGIC_LE {
            false
        } else {
            return false;
        };

        // Java DeltavisionReader.isThisType rejects MRC/MAP headers after the
        // magic check. MAP is at offset 208 in MRC files.
        if header.len() >= 212 && header[208..212].iter().any(|&b| b != 0) {
            let map = std::str::from_utf8(&header[208..212]).unwrap_or("").trim();
            if map == "MAP" {
                return false;
            }
        }

        // Java also verifies that the leading dimensions are positive. It uses
        // the file length for a one-plane sanity check; byte sniffing has no
        // complete stream length, so keep the deterministic header part here.
        if header.len() >= 12 {
            r_i32(header, 0, little) > 0
                && r_i32(header, 4, little) > 0
                && r_i32(header, 8, little) > 0
        } else {
            true
        }
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let path = resolve_deltavision_input_path(path);
        let mut f = File::open(&path).map_err(BioFormatsError::Io)?;
        let mut hdr = vec![0u8; HEADER_SIZE];
        f.read_exact(&mut hdr).map_err(BioFormatsError::Io)?;

        // Detect endianness
        let magic_le = i16::from_le_bytes([hdr[96], hdr[97]]);
        let magic_be = i16::from_be_bytes([hdr[96], hdr[97]]);
        let le = if magic_le == DV_MAGIC_LE {
            true
        } else if magic_be == DV_MAGIC_LE {
            false
        } else {
            return Err(BioFormatsError::UnsupportedFormat(
                "DeltaVision header is missing PRIISM magic".into(),
            ));
        };

        let num_x = positive_i32_dim(r_i32(&hdr, 0, le), "width")?;
        let num_y = positive_i32_dim(r_i32(&hdr, 4, le), "height")?;
        let num_z = positive_i32_dim(r_i32(&hdr, 8, le), "section count")?;
        let mode = r_i32(&hdr, 12, le);
        let ext_hdr_size = r_i32(&hdr, 92, le).max(0) as u64;
        // Pixel spacings (µm). Java DeltavisionReader seeks to offset 16 and
        // reads 6 ints (subImageStart X/Y/Z at 16/20/24, pixelSampling X/Y/Z at
        // 28/32/36) before the three pixel-size floats, so pixX/pixY/pixZ live at
        // offsets 40/44/48 — not 28/32/36, which hold integer sampling counts.
        let dx = r_f32(&hdr, 40, le);
        let dy = r_f32(&hdr, 44, le);
        let dz = r_f32(&hdr, 48, le);
        let file_type = r_i16(&hdr, 160, le);

        // NumWaves at offset 196, NumTimes at offset 180 (Bio-Formats offsets)
        let ints_per_section = r_u16(&hdr, 128, le);
        let floats_per_section = r_u16(&hdr, 130, le);
        let num_waves = r_i16(&hdr, 196, le).max(1) as u32;
        let mut channel_emission_wavelengths = Vec::with_capacity(num_waves as usize);
        for c in 0..num_waves as usize {
            // Java's initExtraMetadata reads emission wavelengths from the
            // fixed header table: channels 1-5 immediately after NumWaves, and
            // channels 6-12 from the NEW_TYPE extension block.
            let off = if c < 5 {
                Some(198 + c * 2)
            } else if file_type == 100 && c < 12 {
                Some(676 + (c - 5) * 2)
            } else {
                None
            };
            let value = off.map(|off| r_i16(&hdr, off, le) as f64).unwrap_or(0.0);
            channel_emission_wavelengths.push((value > 0.0).then_some(value));
        }
        let mut raw_num_times = r_u16(&hdr, 180, le) as u32;
        let mut num_panels = 0u32;
        if file_type == 100 {
            let secondary_t = r_i32(&hdr, 852, le);
            let panels = r_i32(&hdr, 880, le);
            let plane_area = (num_x as u64).saturating_mul(num_y as u64).max(1);
            let max_reasonable_panels = (f.metadata().map_err(BioFormatsError::Io)?.len()
                / plane_area)
                .min(u32::MAX as u64) as u32;
            if panels > 0 && (panels as u32) <= max_reasonable_panels {
                num_panels = panels as u32;
                if secondary_t > 0 && (raw_num_times == 0 || raw_num_times == u16::MAX as u32) {
                    raw_num_times = secondary_t as u32;
                }
            }
        }
        let num_times = raw_num_times.max(1);
        let sequence = r_i16(&hdr, 182, le) as i32;
        let image_sequence = dv_image_sequence(sequence);

        let (pixel_type, bpp) = dv_pixel_type(mode);
        // Java always sets rgb=false, interleaved=false; mode 6 is single-channel UINT16.
        let is_rgb = false;
        let samples_per_pixel = 1u32;
        let channels = num_waves;

        let panels = num_panels.max(1);
        let logical_planes_per_z = channels.max(1) * num_times.max(1);
        let raw_size_z = (num_z / (logical_planes_per_z * panels).max(1)).max(1);
        let raw_image_count = raw_size_z * channels * num_times;

        let extended_headers = read_extended_headers(
            &mut f,
            le,
            ext_hdr_size,
            raw_image_count * panels,
            ints_per_section,
            floats_per_section,
        )?;
        let older_positions = if num_panels == 0 {
            older_position_series_count(
                &extended_headers,
                image_sequence,
                raw_size_z,
                channels,
                num_times,
                is_rgb,
            )
        } else {
            1
        };
        let series_count = panels.max(older_positions);
        let size_t = if older_positions > 1 {
            (num_times / older_positions).max(1)
        } else {
            num_times
        };
        let size_z = raw_size_z;
        let image_count = size_z * channels * size_t;
        let data_offset = HEADER_SIZE as u64 + ext_hdr_size;

        let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
        meta_map.insert("pixel_spacing_x".into(), MetadataValue::Float(dx as f64));
        meta_map.insert("pixel_spacing_y".into(), MetadataValue::Float(dy as f64));
        meta_map.insert("pixel_spacing_z".into(), MetadataValue::Float(dz as f64));
        meta_map.insert("dv_mode".into(), MetadataValue::Int(mode as i64));
        meta_map.insert("dv_file_type".into(), MetadataValue::Int(file_type as i64));
        meta_map.insert("dv_panels".into(), MetadataValue::Int(num_panels as i64));
        meta_map.insert(
            "dv_extended_header_planes".into(),
            MetadataValue::Int(extended_headers.len() as i64),
        );
        meta_map.insert(
            "image_sequence".into(),
            MetadataValue::String(image_sequence.to_string()),
        );

        let base_meta = ImageMetadata {
            size_x: num_x,
            size_y: num_y,
            size_z: size_z,
            size_c: channels,
            size_t,
            pixel_type,
            bits_per_pixel: bpp,
            image_count,
            dimension_order: dv_dimension_order(image_sequence),
            is_rgb,
            is_interleaved: is_rgb,
            is_indexed: false,
            is_little_endian: le,
            resolution_count: 1,
            thumbnail: false,
            series_metadata: meta_map,
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        };
        self.series = (0..series_count)
            .map(|series| {
                let mut meta = base_meta.clone();
                meta.series_metadata
                    .insert("dv_panel_index".into(), MetadataValue::Int(series as i64));
                if older_positions > 1 {
                    meta.series_metadata.insert(
                        "dv_position_index".into(),
                        MetadataValue::Int(series as i64),
                    );
                }
                for plane in 0..meta.image_count {
                    let (z, c, t) = raster_to_zct(plane, &meta);
                    let raw_idx = if older_positions > 1 {
                        dv_old_position_plane_index_for_sizes(
                            image_sequence,
                            z,
                            c,
                            t,
                            series,
                            series_count,
                            meta.size_z,
                            meta.size_c,
                            meta.size_t,
                            meta.is_rgb,
                        )
                    } else {
                        dv_plane_index(image_sequence, z, c, t, series, series_count, &meta)
                    } as usize;
                    if let Some(h) = extended_headers.get(raw_idx) {
                        let prefix = format!("Extended header Z{z} W{c} T{t}");
                        meta.series_metadata.insert(
                            format!("{prefix}:photosensorReading"),
                            MetadataValue::Float(h.photosensor_reading as f64),
                        );
                        meta.series_metadata.insert(
                            format!("{prefix}:timeStampSeconds"),
                            MetadataValue::Float(h.time_stamp_seconds as f64),
                        );
                        meta.series_metadata.insert(
                            format!("{prefix}:stageXCoord"),
                            MetadataValue::Float(h.stage_x as f64),
                        );
                        meta.series_metadata.insert(
                            format!("{prefix}:stageYCoord"),
                            MetadataValue::Float(h.stage_y as f64),
                        );
                        meta.series_metadata.insert(
                            format!("{prefix}:stageZCoord"),
                            MetadataValue::Float(h.stage_z as f64),
                        );
                        meta.series_metadata.insert(
                            format!("{prefix}:minInten"),
                            MetadataValue::Float(h.min_intensity as f64),
                        );
                        meta.series_metadata.insert(
                            format!("{prefix}:maxInten"),
                            MetadataValue::Float(h.max_intensity as f64),
                        );
                        meta.series_metadata.insert(
                            format!("{prefix}:expTime"),
                            MetadataValue::Float(h.exposure_time as f64),
                        );
                        meta.series_metadata.insert(
                            format!("{prefix}:ndFilter"),
                            MetadataValue::Float(h.nd_filter as f64),
                        );
                        meta.series_metadata.insert(
                            format!("{prefix}:exWavelen"),
                            MetadataValue::Float(h.excitation_wavelength as f64),
                        );
                        meta.series_metadata.insert(
                            format!("{prefix}:emWavelen"),
                            MetadataValue::Float(h.emission_wavelength as f64),
                        );
                        meta.series_metadata.insert(
                            format!("{prefix}:intenScaling"),
                            MetadataValue::Float(h.intensity_scaling as f64),
                        );
                        meta.series_metadata.insert(
                            format!("{prefix}:energyConvFactor"),
                            MetadataValue::Float(h.energy_conversion_factor as f64),
                        );
                    }
                }
                meta
            })
            .collect();
        self.current_series = 0;
        self.data_offset = data_offset;
        self.image_sequence = image_sequence.to_string();
        self.samples_per_pixel = samples_per_pixel;
        self.split_positions = series_count > 1;
        self.positions_in_time = older_positions > 1;
        self.stage_ordering = stage_ordering(&extended_headers, series_count);
        self.extended_headers = extended_headers;
        self.channel_emission_wavelengths = channel_emission_wavelengths;
        self.path = Some(path);
        self.log_data = None;
        // Java initFile: parseLogFile(store) (gated on isGroupFiles(), which
        // defaults to true). Locates the `.log` companion and projects its
        // metadata; absent companions are skipped silently.
        self.parse_log_file();
        self.populate_nd_filters(channels);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.series.clear();
        self.current_series = 0;
        self.split_positions = false;
        self.positions_in_time = false;
        self.stage_ordering = StageOrdering::default();
        self.extended_headers.clear();
        self.channel_emission_wavelengths.clear();
        self.nd_filters.clear();
        self.log_data = None;
        Ok(())
    }
    fn series_count(&self) -> usize {
        self.series.len()
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.series.is_empty() {
            Err(BioFormatsError::NotInitialized)
        } else if s >= self.series_count() {
            Err(BioFormatsError::SeriesOutOfRange(s))
        } else {
            self.current_series = s;
            Ok(())
        }
    }
    fn series(&self) -> usize {
        self.current_series
    }
    fn metadata(&self) -> &ImageMetadata {
        self.series
            .get(self.current_series)
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self
            .series
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let bps = meta.pixel_type.bytes_per_sample();
        let samples = self.samples_per_pixel as usize;
        let row_bytes = meta.size_x as usize * samples * bps;
        let plane_bytes = row_bytes * meta.size_y as usize;
        let (z, c, t) = raster_to_zct(plane_index, meta);
        let file_plane_index = self.file_plane_index(z, c, t, meta);
        let offset = self.data_offset + file_plane_index * plane_bytes as u64;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        let file_len = f.metadata().map_err(BioFormatsError::Io)?.len();
        let end = offset.checked_add(plane_bytes as u64).ok_or_else(|| {
            BioFormatsError::InvalidData("DeltaVision plane offset overflows".into())
        })?;
        if end > file_len {
            return Err(BioFormatsError::InvalidData(format!(
                "DeltaVision plane {plane_index} exceeds file length: need bytes {offset}..{end}, file length {file_len}"
            )));
        }
        f.seek(SeekFrom::Start(offset))
            .map_err(BioFormatsError::Io)?;
        let mut stored = vec![0u8; plane_bytes];
        f.read_exact(&mut stored).map_err(BioFormatsError::Io)?;

        let mut buf = vec![0u8; plane_bytes];
        for y in 0..meta.size_y as usize {
            let src = y * row_bytes;
            let dst = (meta.size_y as usize - 1 - y) * row_bytes;
            buf[dst..dst + row_bytes].copy_from_slice(&stored[src..src + row_bytes]);
        }
        Ok(buf)
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
        let meta = self.series.get(self.current_series).unwrap();
        if x.checked_add(w).is_none_or(|end| end > meta.size_x)
            || y.checked_add(h).is_none_or(|end| end > meta.size_y)
        {
            return Err(BioFormatsError::InvalidData("region out of bounds".into()));
        }
        let spp = self.samples_per_pixel as usize;
        let bps = meta.pixel_type.bytes_per_sample();
        let row = meta.size_x as usize * spp * bps;
        let out_row = w as usize * spp * bps;
        let mut out = Vec::with_capacity(h as usize * out_row);
        for r in 0..h as usize {
            let src = &full[(y as usize + r) * row..];
            out.extend_from_slice(&src[x as usize * spp * bps..x as usize * spp * bps + out_row]);
        }
        Ok(out)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self
            .series
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = ((meta.size_x - tw) / 2, (meta.size_y - th) / 2);
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        use crate::common::metadata::MetadataValue;
        use crate::common::ome_metadata::{OmeMetadata, OmePlane};
        let meta = self.series.get(self.current_series)?;
        let mut ome = OmeMetadata::from_image_metadata(meta);
        let img = &mut ome.images[0];
        if img.name.is_none() {
            img.name = self
                .path
                .as_ref()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .map(|s| s.to_string());
        }
        let get_f = |k: &str| -> Option<f64> {
            if let Some(MetadataValue::Float(v)) = meta.series_metadata.get(k) {
                Some(*v)
            } else {
                None
            }
        };
        // DeltaVision pixel_spacing is stored in µm
        img.physical_size_x = get_f("pixel_spacing_x");
        img.physical_size_y = get_f("pixel_spacing_y");
        img.physical_size_z = get_f("pixel_spacing_z");
        if !self.extended_headers.is_empty() {
            let metadata_series = self.stage_metadata_series_index(self.current_series);
            let mut exposure_by_channel = vec![None; meta.size_c as usize];
            img.planes = (0..meta.image_count)
                .filter_map(|plane| {
                    let (z, c, t) = raster_to_zct(plane, meta);
                    let raw_idx =
                        self.file_plane_index_for_series(z, c, t, metadata_series, meta) as usize;
                    let h = self.extended_headers.get(raw_idx)?;
                    let exposure = exposure_by_channel.get_mut(c as usize).and_then(|slot| {
                        if slot.is_none() {
                            *slot = Some(h.exposure_time as f64);
                        }
                        *slot
                    });
                    Some(OmePlane {
                        the_z: z,
                        the_c: c,
                        the_t: t,
                        delta_t: Some(h.time_stamp_seconds as f64),
                        exposure_time: exposure,
                        position_x: Some(h.stage_x as f64),
                        position_y: Some(h.stage_y as f64),
                        position_z: Some(h.stage_z as f64),
                    })
                })
                .collect();

            for c in 0..meta.size_c as usize {
                if let Some(channel) = img.channels.get_mut(c) {
                    let raw_idx =
                        self.file_plane_index_for_series(0, c as u32, 0, metadata_series, meta)
                            as usize;
                    if let Some(Some(emission)) = self.channel_emission_wavelengths.get(c) {
                        channel.emission_wavelength = Some(*emission);
                    }
                    if let Some(h) = self.extended_headers.get(raw_idx) {
                        if h.excitation_wavelength > 0.0 {
                            channel.excitation_wavelength = Some(h.excitation_wavelength as f64);
                        }
                    }
                }
            }
        }

        // Channel NDFilter (Java store.setChannelNDFilter(ndFilters[w], ...)).
        for c in 0..meta.size_c as usize {
            if let Some(channel) = img.channels.get_mut(c) {
                if let Some(Some(nd)) = self.nd_filters.get(c) {
                    channel.nd_filter = Some(*nd);
                }
            }
        }
        // Project metadata parsed from the `.log` companion file, mirroring the
        // OME `MetadataStore` writes in Java `DeltavisionReader.parseLogFile`.
        if let Some(log) = &self.log_data {
            use crate::common::ome_metadata::{OmeDetector, OmeInstrument, OmeObjective};
            let img = &mut ome.images[0];

            if let Some(date) = &log.acquisition_date {
                img.acquisition_date = Some(date.clone());
            }

            // Physical pixel sizes (Pixel Size: X Y Z).
            if let Some(v) = log.physical_size_x {
                img.physical_size_x = Some(v);
            }
            if let Some(v) = log.physical_size_y {
                img.physical_size_y = Some(v);
            }
            if let Some(v) = log.physical_size_z {
                img.physical_size_z = Some(v);
            }

            // Channel names (EM filter).
            for (c, name) in log.channel_names.iter().enumerate() {
                if let Some(channel) = img.channels.get_mut(c) {
                    channel.name = Some(name.clone());
                }
            }

            if log.has_objective() || log.has_detector() {
                if ome.instruments.is_empty() {
                    ome.instruments.push(OmeInstrument::default());
                }
                let inst = &mut ome.instruments[0];

                if log.has_objective() {
                    let o = &log.objective;
                    inst.objectives.push(OmeObjective {
                        id: o.id.clone(),
                        model: o.model.clone(),
                        manufacturer: o.manufacturer.clone(),
                        nominal_magnification: o.nominal_magnification,
                        calibrated_magnification: None,
                        lens_na: o.lens_na,
                        immersion: o.immersion.clone(),
                        correction: o.correction.clone(),
                        working_distance: None,
                    });
                }
                if log.has_detector() {
                    let d = &log.detector;
                    inst.detectors.push(OmeDetector {
                        id: None,
                        model: d.model.clone(),
                        manufacturer: None,
                        detector_type: d.detector_type.clone(),
                        gain: d.gain,
                        offset: None,
                    });
                }

                ome.images[0].instrument_ref = Some(0);
                if log.has_objective() {
                    ome.images[0].objective_ref = Some(0);
                }
            }
        }

        let _ = ome.add_original_metadata_annotations(meta, 0);
        Some(ome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::{Seek, SeekFrom, Write};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dv_path(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "bioformats_rs_{}_{}_{}.dv",
            name,
            std::process::id(),
            stamp
        ))
    }

    fn write_i32(buf: &mut [u8], off: usize, value: i32) {
        buf[off..off + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn write_i16(buf: &mut [u8], off: usize, value: i16) {
        buf[off..off + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn write_u16(buf: &mut [u8], off: usize, value: u16) {
        buf[off..off + 2].copy_from_slice(&value.to_le_bytes());
    }

    #[test]
    fn byte_sniffing_rejects_mrc_map_header_with_dv_magic_collision() {
        let mut header = vec![0u8; 212];
        write_i32(&mut header, 0, 4);
        write_i32(&mut header, 4, 4);
        write_i32(&mut header, 8, 1);
        write_i16(&mut header, 96, DV_MAGIC_LE);
        header[208..212].copy_from_slice(b"MAP ");

        assert!(!DeltavisionReader::new().is_this_type_by_bytes(&header));
    }

    #[test]
    fn byte_sniffing_requires_positive_leading_dimensions() {
        let mut header = vec![0u8; 212];
        write_i32(&mut header, 0, 0);
        write_i32(&mut header, 4, 4);
        write_i32(&mut header, 8, 1);
        write_i16(&mut header, 96, DV_MAGIC_LE);

        assert!(!DeltavisionReader::new().is_this_type_by_bytes(&header));
    }

    #[test]
    fn byte_sniffing_requires_java_minimum_header_length() {
        let mut header = vec![0u8; 98];
        write_i32(&mut header, 0, 4);
        write_i32(&mut header, 4, 4);
        write_i32(&mut header, 8, 1);
        write_i16(&mut header, 96, DV_MAGIC_LE);

        assert!(!DeltavisionReader::new().is_this_type_by_bytes(&header));
    }

    #[test]
    fn name_sniffing_accepts_java_companion_suffixes() {
        let reader = DeltavisionReader::new();
        assert!(reader.is_this_type_by_name(Path::new("sample.dv")));
        assert!(reader.is_this_type_by_name(Path::new("sample.r3d")));
        assert!(reader.is_this_type_by_name(Path::new("sample.r3d_d3d")));
        assert!(reader.is_this_type_by_name(Path::new("sample.dv.log")));
        assert!(reader.is_this_type_by_name(Path::new("sample.r3d.log")));
        assert!(reader.is_this_type_by_name(Path::new("sample_log.txt")));
        assert!(!reader.is_this_type_by_name(Path::new("sample.pnl")));
    }

    fn write_synthetic_dv_with_header(
        name: &str,
        size_x: i32,
        size_y: i32,
        sections: i32,
        mode: i32,
        size_t: i16,
        sequence: i16,
        waves: i16,
        planes: &[&[u8]],
        customize_header: impl FnOnce(&mut [u8]),
    ) -> PathBuf {
        let path = temp_dv_path(name);
        let mut hdr = vec![0u8; HEADER_SIZE];
        write_i32(&mut hdr, 0, size_x);
        write_i32(&mut hdr, 4, size_y);
        write_i32(&mut hdr, 8, sections);
        write_i32(&mut hdr, 12, mode);
        write_i32(&mut hdr, 92, 0);
        write_i16(&mut hdr, 96, DV_MAGIC_LE);
        write_i16(&mut hdr, 180, size_t);
        write_i16(&mut hdr, 182, sequence);
        write_i16(&mut hdr, 196, waves);
        customize_header(&mut hdr);

        let mut file = fs::File::create(&path).unwrap();
        file.write_all(&hdr).unwrap();
        for plane in planes {
            file.write_all(plane).unwrap();
        }
        path
    }

    fn write_synthetic_dv(
        name: &str,
        size_x: i32,
        size_y: i32,
        sections: i32,
        mode: i32,
        size_t: i16,
        sequence: i16,
        waves: i16,
        planes: &[&[u8]],
    ) -> PathBuf {
        write_synthetic_dv_with_header(
            name,
            size_x,
            size_y,
            sections,
            mode,
            size_t,
            sequence,
            waves,
            planes,
            |_| {},
        )
    }

    #[test]
    fn rejects_missing_magic_and_non_positive_dimensions() {
        let missing_magic = temp_dv_path("missing_magic");
        let mut hdr = vec![0u8; HEADER_SIZE];
        write_i32(&mut hdr, 0, 1);
        write_i32(&mut hdr, 4, 1);
        write_i32(&mut hdr, 8, 1);
        fs::write(&missing_magic, hdr).unwrap();

        let mut reader = DeltavisionReader::new();
        let err = reader.set_id(&missing_magic).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("PRIISM magic")),
            "{err:?}"
        );
        let _ = fs::remove_file(&missing_magic);

        let zero_width = write_synthetic_dv("zero_width", 0, 1, 1, 0, 1, 0, 1, &[&[7]]);
        let mut reader = DeltavisionReader::new();
        let err = reader.set_id(&zero_width).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("non-positive width")),
            "{err:?}"
        );
        let _ = fs::remove_file(&zero_width);
    }

    fn write_synthetic_dv_with_extended_headers(
        name: &str,
        size_x: i32,
        size_y: i32,
        sections: i32,
        mode: i32,
        size_t: i16,
        sequence: i16,
        waves: i16,
        ext_headers: &[DvExtendedHeader],
        planes: &[&[u8]],
    ) -> PathBuf {
        let path = temp_dv_path(name);
        let floats_per_section = 14i16;
        let ext_size = ext_headers.len() as i32 * floats_per_section as i32 * 4;
        let mut hdr = vec![0u8; HEADER_SIZE];
        write_i32(&mut hdr, 0, size_x);
        write_i32(&mut hdr, 4, size_y);
        write_i32(&mut hdr, 8, sections);
        write_i32(&mut hdr, 12, mode);
        write_i32(&mut hdr, 92, ext_size);
        write_i16(&mut hdr, 96, DV_MAGIC_LE);
        write_i16(&mut hdr, 128, 0);
        write_i16(&mut hdr, 130, floats_per_section);
        write_i16(&mut hdr, 180, size_t);
        write_i16(&mut hdr, 182, sequence);
        write_i16(&mut hdr, 196, waves);
        for c in 0..(waves.max(0) as usize).min(5) {
            let wavelength = ext_headers
                .get(c)
                .map(|h| h.emission_wavelength as i16)
                .unwrap_or(0);
            write_i16(&mut hdr, 198 + c * 2, wavelength);
        }

        let mut file = fs::File::create(&path).unwrap();
        file.write_all(&hdr).unwrap();
        for h in ext_headers {
            let values = [
                h.photosensor_reading,
                h.time_stamp_seconds,
                h.stage_x,
                h.stage_y,
                h.stage_z,
                h.min_intensity,
                h.max_intensity,
                0.0,
                h.exposure_time,
                h.nd_filter,
                h.excitation_wavelength,
                h.emission_wavelength,
                h.intensity_scaling,
                h.energy_conversion_factor,
            ];
            for value in values {
                file.write_all(&value.to_le_bytes()).unwrap();
            }
        }
        for plane in planes {
            file.write_all(plane).unwrap();
        }
        path
    }

    fn ext_header(
        time_stamp_seconds: f32,
        stage_x: f32,
        stage_y: f32,
        exposure_time: f32,
        excitation_wavelength: f32,
        emission_wavelength: f32,
    ) -> DvExtendedHeader {
        DvExtendedHeader {
            photosensor_reading: 1.0,
            time_stamp_seconds,
            stage_x,
            stage_y,
            stage_z: 3.0,
            min_intensity: 4.0,
            max_intensity: 5.0,
            exposure_time,
            nd_filter: 50.0,
            excitation_wavelength,
            emission_wavelength,
            intensity_scaling: 1.0,
            energy_conversion_factor: 1.0,
        }
    }

    #[test]
    fn non_rgb_planes_stride_by_logical_channel_and_flip_rows() {
        let stored_c0_z0 = [1, 2, 3, 4];
        let stored_c1_z0 = [11, 12, 13, 14];
        let stored_c0_z1 = [21, 22, 23, 24];
        let stored_c1_z1 = [31, 32, 33, 34];
        let path = write_synthetic_dv(
            "non_rgb_stride",
            2,
            2,
            4,
            5,
            1,
            1,
            2,
            &[&stored_c0_z0, &stored_c1_z0, &stored_c0_z1, &stored_c1_z1],
        );

        let mut reader = DeltavisionReader::new();
        reader.set_id(&path).unwrap();
        let meta = reader.metadata();
        assert_eq!(meta.size_z, 2);
        assert_eq!(meta.size_c, 2);
        assert_eq!(meta.image_count, 4);
        assert!(!meta.is_rgb);
        assert!(!meta.is_interleaved);
        assert_eq!(meta.dimension_order, DimensionOrder::XYCZT);

        assert_eq!(reader.open_bytes(1).unwrap(), vec![13, 14, 11, 12]);
        assert_eq!(
            reader.open_bytes_region(1, 1, 0, 1, 2).unwrap(),
            vec![14, 12]
        );

        fs::remove_file(path).ok();
    }

    #[test]
    fn set_id_resolves_dv_log_input_to_pixel_file() {
        let path = write_synthetic_dv("log_input_resolution", 1, 1, 1, 0, 1, 0, 1, &[&[42]]);
        let log_path = PathBuf::from(format!("{}.log", path.to_str().unwrap()));
        fs::write(&log_path, "Created:     Wed Jul 25 14:00:00 2007\n").unwrap();

        let mut reader = DeltavisionReader::new();
        reader.set_id(&log_path).unwrap();
        assert_eq!(reader.metadata().image_count, 1);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![42]);
        let ome = reader.ome_metadata().unwrap();
        assert_eq!(
            ome.images[0].acquisition_date.as_deref(),
            Some("2007-07-25T14:00:00")
        );

        fs::remove_file(&log_path).ok();
        fs::remove_file(&path).ok();
    }

    #[test]
    fn mode_6_reports_single_channel_uint16() {
        // Java getPixelType: mode 6 -> UINT16, rgb=false, interleaved=false, 1 channel.
        // 2x1 UINT16 plane = 4 bytes; only first 4 bytes of stored data are read.
        let stored = [1, 2, 3, 4, 5, 6];
        let path = write_synthetic_dv("mode6_uint16", 2, 1, 1, 6, 1, 0, 1, &[&stored]);

        let mut reader = DeltavisionReader::new();
        reader.set_id(&path).unwrap();
        let meta = reader.metadata();
        assert_eq!(meta.size_z, 1);
        assert_eq!(meta.size_c, 1);
        assert_eq!(meta.image_count, 1);
        assert_eq!(meta.pixel_type, PixelType::Uint16);
        assert!(!meta.is_rgb);
        assert!(!meta.is_interleaved);
        // Single row, so no row flipping changes; first 4 bytes of the plane.
        assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4]);
        assert!(matches!(
            reader.open_bytes(1),
            Err(BioFormatsError::PlaneOutOfRange(1))
        ));

        fs::remove_file(path).ok();
    }

    #[test]
    fn truncated_plane_returns_error_instead_of_zero_filling_tail() {
        let truncated = [1, 2, 3];
        let path = write_synthetic_dv("truncated_plane", 2, 2, 1, 5, 1, 0, 1, &[&truncated]);

        let mut reader = DeltavisionReader::new();
        reader.set_id(&path).unwrap();
        let meta = reader.metadata();
        assert_eq!(meta.image_count, 1);
        assert_eq!(meta.size_x, 2);
        assert_eq!(meta.size_y, 2);

        let err = reader.open_bytes(0).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::InvalidData(ref message) if message.contains("exceeds file length")),
            "{err:?}"
        );

        fs::remove_file(path).ok();
    }

    #[test]
    fn new_type_panels_are_exposed_as_series_and_offset_by_panel() {
        let panel0 = [1, 2, 3, 4];
        let panel1 = [11, 12, 13, 14];
        let path = write_synthetic_dv_with_header(
            "new_type_panels",
            2,
            2,
            2,
            5,
            -1,
            3,
            1,
            &[&panel0, &panel1],
            |hdr| {
                write_i16(hdr, 160, 100);
                write_u16(hdr, 180, u16::MAX);
                write_i32(hdr, 852, 1);
                write_i32(hdr, 880, 2);
            },
        );

        let mut reader = DeltavisionReader::new();
        reader.set_id(&path).unwrap();
        assert_eq!(reader.series_count(), 2);
        assert_eq!(reader.metadata().size_z, 1);
        assert_eq!(reader.metadata().size_t, 1);
        assert_eq!(reader.metadata().image_count, 1);
        assert_eq!(
            reader
                .metadata()
                .series_metadata
                .get("dv_panels")
                .unwrap()
                .to_string(),
            "2"
        );
        assert_eq!(reader.open_bytes(0).unwrap(), vec![3, 4, 1, 2]);

        reader.set_series(1).unwrap();
        assert_eq!(
            reader
                .metadata()
                .series_metadata
                .get("dv_panel_index")
                .unwrap()
                .to_string(),
            "1"
        );
        assert_eq!(reader.open_bytes(0).unwrap(), vec![13, 14, 11, 12]);
        assert!(matches!(
            reader.set_series(2),
            Err(BioFormatsError::SeriesOutOfRange(2))
        ));

        fs::remove_file(path).ok();
    }

    #[test]
    fn extended_header_populates_original_and_ome_plane_metadata() {
        let headers = [
            ext_header(0.25, 10.0, 20.0, 0.05, 488.0, 525.0),
            ext_header(1.25, 11.0, 21.0, 0.07, 561.0, 620.0),
        ];
        let path = write_synthetic_dv_with_extended_headers(
            "extended_header_metadata",
            1,
            1,
            2,
            5,
            2,
            0,
            1,
            &headers,
            &[&[7], &[9]],
        );

        let mut reader = DeltavisionReader::new();
        reader.set_id(&path).unwrap();
        let meta = reader.metadata();
        assert_eq!(meta.image_count, 2);
        assert_eq!(
            meta.series_metadata
                .get("Extended header Z0 W0 T1:timeStampSeconds")
                .unwrap()
                .to_string(),
            "1.25"
        );
        assert_eq!(
            meta.series_metadata
                .get("Extended header Z0 W0 T0:ndFilter")
                .unwrap()
                .to_string(),
            "0.5"
        );

        let ome = reader.ome_metadata().unwrap();
        let planes = &ome.images[0].planes;
        assert_eq!(planes.len(), 2);
        assert_eq!(planes[0].delta_t, Some(0.25));
        assert_eq!(planes[0].exposure_time, Some(0.05000000074505806));
        assert_eq!(planes[1].exposure_time, Some(0.05000000074505806));
        assert_eq!(planes[0].position_x, Some(10.0));
        assert_eq!(planes[1].the_t, 1);
        assert_eq!(planes[1].position_y, Some(21.0));
        assert_eq!(ome.images[0].channels[0].excitation_wavelength, Some(488.0));
        assert!(ome
            .annotations
            .iter()
            .any(|annotation| format!("{annotation:?}").contains("OriginalMetadata")));

        assert_eq!(reader.open_bytes(1).unwrap(), vec![9]);
        fs::remove_file(path).ok();
    }

    #[test]
    fn channel_emission_wavelength_comes_from_fixed_header_table() {
        let headers = [ext_header(0.25, 10.0, 20.0, 0.05, 488.0, 525.0)];
        let path = write_synthetic_dv_with_extended_headers(
            "emission_from_header",
            1,
            1,
            1,
            5,
            1,
            0,
            1,
            &headers,
            &[&[7]],
        );
        let mut file = fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.seek(SeekFrom::Start(198)).unwrap();
        file.write_all(&600i16.to_le_bytes()).unwrap();

        let mut reader = DeltavisionReader::new();
        reader.set_id(&path).unwrap();
        let ome = reader.ome_metadata().unwrap();
        assert_eq!(ome.images[0].channels[0].emission_wavelength, Some(600.0));
        assert_eq!(ome.images[0].channels[0].excitation_wavelength, Some(488.0));

        fs::remove_file(path).ok();
    }

    #[test]
    fn older_stage_positions_split_timepoints_into_series() {
        let headers = [
            ext_header(0.0, 100.0, 200.0, 0.01, 488.0, 525.0),
            ext_header(0.1, 300.0, 200.0, 0.01, 488.0, 525.0),
            ext_header(1.0, 100.0, 200.0, 0.01, 488.0, 525.0),
            ext_header(1.1, 300.0, 200.0, 0.01, 488.0, 525.0),
        ];
        let path = write_synthetic_dv_with_extended_headers(
            "older_stage_positions",
            1,
            1,
            4,
            5,
            4,
            0,
            1,
            &headers,
            &[&[1], &[2], &[3], &[4]],
        );

        let mut reader = DeltavisionReader::new();
        reader.set_id(&path).unwrap();
        assert_eq!(reader.series_count(), 2);
        assert_eq!(reader.metadata().size_t, 2);
        assert_eq!(reader.metadata().image_count, 2);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![1]);
        assert_eq!(reader.open_bytes(1).unwrap(), vec![3]);

        reader.set_series(1).unwrap();
        assert_eq!(
            reader
                .metadata()
                .series_metadata
                .get("dv_position_index")
                .unwrap()
                .to_string(),
            "1"
        );
        assert_eq!(reader.open_bytes(0).unwrap(), vec![2]);
        assert_eq!(reader.open_bytes(1).unwrap(), vec![4]);
        let ome = reader.ome_metadata().unwrap();
        assert_eq!(ome.images[0].planes[0].position_x, Some(300.0));
        assert_eq!(ome.images[0].planes[1].delta_t, Some(1.100000023841858));

        fs::remove_file(path).ok();
    }

    #[test]
    fn nd_filter_seeded_from_extended_header_when_no_log() {
        // ext_header writes ndFilter=50.0; from_floats divides by 100 -> 0.5.
        let headers = [
            ext_header(0.25, 10.0, 20.0, 0.05, 488.0, 525.0),
            ext_header(1.25, 11.0, 21.0, 0.07, 561.0, 620.0),
        ];
        let path = write_synthetic_dv_with_extended_headers(
            "nd_filter_seed",
            1,
            1,
            2,
            5,
            1,
            0,
            2,
            &headers,
            &[&[7], &[9]],
        );

        let mut reader = DeltavisionReader::new();
        reader.set_id(&path).unwrap();
        // Two channels, both seeded from their first plane's ndFilter (0.5).
        assert_eq!(reader.nd_filters, vec![Some(0.5), Some(0.5)]);

        let ome = reader.ome_metadata().unwrap();
        assert_eq!(ome.images[0].channels[0].nd_filter, Some(0.5));
        assert_eq!(ome.images[0].channels[1].nd_filter, Some(0.5));

        fs::remove_file(path).ok();
    }

    #[test]
    fn nd_filter_from_log_overrides_extended_header() {
        let headers = [ext_header(0.25, 10.0, 20.0, 0.05, 488.0, 525.0)];
        let path = write_synthetic_dv_with_extended_headers(
            "nd_filter_log_override",
            1,
            1,
            1,
            5,
            1,
            0,
            1,
            &headers,
            &[&[7]],
        );
        let log_path = PathBuf::from(format!("{}.log", path.to_str().unwrap()));
        // "ND filter" value with a percent sign, parsed and divided by 100.
        fs::write(&log_path, "ND filter:   32%\n").unwrap();

        let mut reader = DeltavisionReader::new();
        reader.set_id(&path).unwrap();
        // Log value (0.32) wins over the extended-header seed (0.5).
        assert_eq!(reader.nd_filters, vec![Some(0.32)]);
        let ome = reader.ome_metadata().unwrap();
        assert_eq!(ome.images[0].channels[0].nd_filter, Some(0.32));

        fs::remove_file(&log_path).ok();
        fs::remove_file(&path).ok();
    }

    #[test]
    fn parses_log_companion_into_metadata_and_ome() {
        // Single-channel 1x1 plane; the magic/header is enough for set_id.
        let path = write_synthetic_dv("log_companion", 1, 1, 1, 0, 1, 0, 1, &[&[42]]);
        // find_log_file first tries "<id>.log" -> "<...>.dv.log".
        let log_path = PathBuf::from(format!("{}.log", path.to_str().unwrap()));
        let log = "Created:     Wed Jul 25 14:00:00 2007\n\
                   Objective:   Olympus 60X/1.40,PlanApo,microscope,UPLSAPO60X\n\
                   Lens ID:     Olympus 12345\n\
                   Pixel Size:  0.1 0.1 0.2\n\
                   Binning:     2x2\n\
                   Type:        CoolSNAP HQ\n\
                   Gain:        2X\n\
                   EM filter:   FITC\n";
        fs::write(&log_path, log).unwrap();

        let mut reader = DeltavisionReader::new();
        reader.set_id(&path).unwrap();

        let meta = reader.metadata();
        // addGlobalMeta keys (Java exact key names, prefixed by current prefix).
        assert_eq!(
            meta.series_metadata.get(" Objective").unwrap().to_string(),
            "Olympus 60X/1.40,PlanApo,microscope,UPLSAPO60X"
        );
        assert_eq!(
            meta.series_metadata.get(" Lens ID").unwrap().to_string(),
            "Olympus 12345"
        );
        assert_eq!(
            meta.series_metadata.get(" Binning").unwrap().to_string(),
            "2x2"
        );

        let ome = reader.ome_metadata().unwrap();
        assert_eq!(
            ome.images[0].acquisition_date.as_deref(),
            Some("2007-07-25T14:00:00")
        );
        assert_eq!(ome.images[0].physical_size_x, Some(0.1));
        assert_eq!(ome.images[0].physical_size_z, Some(0.2));
        assert_eq!(ome.images[0].channels[0].name.as_deref(), Some("FITC"));

        let inst = &ome.instruments[0];
        let obj = &inst.objectives[0];
        // Objective: manufacturer is the first word; magnification/NA from token 0.
        assert_eq!(obj.manufacturer.as_deref(), Some("Olympus"));
        assert_eq!(obj.nominal_magnification, Some(60.0));
        assert_eq!(obj.lens_na, Some(1.40));
        // Lens ID strips the manufacturer prefix and sets the objective ID.
        assert_eq!(obj.id.as_deref(), Some("Objective:12345"));
        // Lens ID overrides correction/immersion to "Other".
        assert_eq!(obj.correction.as_deref(), Some("Other"));
        assert_eq!(obj.immersion.as_deref(), Some("Other"));

        let det = &inst.detectors[0];
        assert_eq!(det.detector_type.as_deref(), Some("Other"));
        assert_eq!(det.model.as_deref(), Some("CoolSNAP HQ"));
        assert_eq!(det.gain, Some(2.0));

        assert_eq!(ome.images[0].objective_ref, Some(0));
        assert_eq!(ome.images[0].instrument_ref, Some(0));

        fs::remove_file(&log_path).ok();
        fs::remove_file(&path).ok();
    }

    #[test]
    fn missing_log_companion_is_skipped_silently() {
        let path = write_synthetic_dv("no_log_companion", 1, 1, 1, 0, 1, 0, 1, &[&[42]]);
        let mut reader = DeltavisionReader::new();
        reader.set_id(&path).unwrap();
        // No log -> no objective/detector instrument projected.
        let ome = reader.ome_metadata().unwrap();
        assert!(ome.instruments.is_empty() || ome.instruments[0].objectives.is_empty());
        fs::remove_file(&path).ok();
    }

    #[test]
    fn reversed_stage_x_reorders_ome_positions_without_changing_pixel_series() {
        let headers = [
            ext_header(0.0, 300.0, 200.0, 0.01, 488.0, 525.0),
            ext_header(0.1, 100.0, 200.0, 0.01, 561.0, 620.0),
            ext_header(1.0, 300.0, 200.0, 0.02, 488.0, 525.0),
            ext_header(1.1, 100.0, 200.0, 0.02, 561.0, 620.0),
        ];
        let path = write_synthetic_dv_with_extended_headers(
            "reversed_stage_x",
            1,
            1,
            4,
            5,
            4,
            0,
            1,
            &headers,
            &[&[1], &[2], &[3], &[4]],
        );

        let mut reader = DeltavisionReader::new();
        reader.set_id(&path).unwrap();
        assert_eq!(reader.series_count(), 2);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![1]);
        assert_eq!(reader.open_bytes(1).unwrap(), vec![3]);

        let ome = reader.ome_metadata().unwrap();
        assert_eq!(ome.images[0].planes[0].position_x, Some(100.0));
        assert_eq!(ome.images[0].planes[0].delta_t, Some(0.10000000149011612));

        reader.set_series(1).unwrap();
        assert_eq!(reader.open_bytes(0).unwrap(), vec![2]);
        assert_eq!(reader.open_bytes(1).unwrap(), vec![4]);
        let ome = reader.ome_metadata().unwrap();
        assert_eq!(ome.images[0].planes[0].position_x, Some(300.0));
        assert_eq!(ome.images[0].planes[1].delta_t, Some(1.0));

        fs::remove_file(path).ok();
    }
}
