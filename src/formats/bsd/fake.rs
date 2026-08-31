//! Synthetic "fake" image format for testing.
//!
//! The filename encodes image parameters as `&key=value` pairs before the
//! `.fake` extension.  Example:
//!   `test_&sizeX=512&sizeY=256&sizeZ=5&pixelType=uint16.fake`
//!
//! This is a faithful port of Java Bio-Formats'
//! `loci.formats.in.FakeReader` filename-parameter parsing.  Java honors
//! roughly fifty `key=value` tokens; this reader recognizes the same set.
//! Parameters that affect pixel layout (`rgb`, `dimOrder`, `interleaved`,
//! `indexed`, `bitsPerPixel`, `thumbSize*`, `little`, `series`,
//! `resolutions`, `resolutionScale`, `pixelType`, the `size*` family) are
//! reflected directly in [`ImageMetadata`].  Parameters that the Rust
//! metadata model cannot represent structurally (annotations, ROI shapes,
//! HCS screens/plates, channel colors, wavelengths, physical sizes, ...) are
//! still parsed and validated exactly as Java does, and recorded as
//! original metadata key/value pairs rather than fabricating unsupported
//! structures.
//!
//! Pixel data follows Java's simple gradient and special-pixel scheme: the
//! upper-left boxes encode series, plane, Z, C, and T indices.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;

// -- Constants (mirroring Java FakeReader) --

const DEFAULT_SIZE_X: u32 = 512;
const DEFAULT_SIZE_Y: u32 = 512;
const DEFAULT_SIZE_Z: u32 = 1;
const DEFAULT_SIZE_C: u32 = 1;
const DEFAULT_SIZE_T: u32 = 1;
const DEFAULT_RGB_CHANNEL_COUNT: u32 = 1;
const DEFAULT_DIMENSION_ORDER: &str = "XYZCT";
const DEFAULT_RGB_DIMENSION_ORDER: &str = "XYCZT";
const DEFAULT_RESOLUTION_SCALE: u32 = 2;
const TOKEN_SEPARATOR: char = '&';
const BOX_SIZE: u32 = 10;

pub struct FakeReader {
    path: Option<PathBuf>,
    /// One [`ImageMetadata`] per series (each carrying its own resolution
    /// count); mirrors Java's `core` list of `CoreMetadata`.
    series: Vec<ImageMetadata>,
    current_series: usize,
    current_resolution: usize,
    resolution_meta: Option<ImageMetadata>,
}

impl FakeReader {
    pub fn new() -> Self {
        FakeReader {
            path: None,
            series: Vec::new(),
            current_series: 0,
            current_resolution: 0,
            resolution_meta: None,
        }
    }
}

impl Default for FakeReader {
    fn default() -> Self {
        Self::new()
    }
}

/// Map a pixel-type string to [`PixelType`], mirroring
/// `FormatTools.pixelTypeFromString`.  Returns `None` for unknown strings
/// (Java throws; we surface this as an error at the call site).
fn pixel_type_from_string(value: &str) -> Option<PixelType> {
    match value.to_ascii_lowercase().as_str() {
        "int8" => Some(PixelType::Int8),
        "uint8" => Some(PixelType::Uint8),
        "int16" => Some(PixelType::Int16),
        "uint16" => Some(PixelType::Uint16),
        "int32" => Some(PixelType::Int32),
        "uint32" => Some(PixelType::Uint32),
        "float" => Some(PixelType::Float32),
        "double" => Some(PixelType::Float64),
        "bit" => Some(PixelType::Bit),
        _ => None,
    }
}

/// Parse a color value, mirroring Java's `parseColor`.
///
/// Colors are parsed as (possibly unsigned) longs so values like
/// `0xff0000ff` (opaque red, RGBA) can be specified.  Decimal by default,
/// hex if prefixed with `0x`/`0X`.  Invalid values yield `0`, as in Java.
fn parse_color(value: &str) -> i32 {
    let (digits, radix) = if let Some(rest) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        (rest, 16)
    } else {
        (value, 10)
    };
    match i64::from_str_radix(digits, radix) {
        Ok(v) => v as i32,
        Err(_) => 0,
    }
}

/// Validate a physical-size token, mirroring Java's `parsePhysicalSize`.
///
/// Java parses a length (value + optional unit) and rejects non-positive
/// values.  The Rust metadata model has no physical-size field, so the
/// caller only needs the numeric value for validation and storage as
/// original metadata.  Returns `Err` on an entirely unparseable value
/// (Java throws a `RuntimeException`), `Ok(None)` for a non-positive value
/// (Java warns and returns null), `Ok(Some(v))` otherwise.
fn parse_physical_size(s: &str) -> Result<Option<f64>> {
    match parse_length_value(s) {
        None => Err(BioFormatsError::InvalidData(format!(
            "Invalid physical size: {}",
            s
        ))),
        Some(v) if v > 0.0 => Ok(Some(v)),
        Some(_) => Ok(None),
    }
}

/// Validate a wavelength token, mirroring Java's `parseWavelength`.
/// Same contract as [`parse_physical_size`].
fn parse_wavelength(s: &str) -> Result<Option<f64>> {
    match parse_length_value(s) {
        None => Err(BioFormatsError::InvalidData(format!(
            "Invalid wavelength: {}",
            s
        ))),
        Some(v) if v > 0.0 => Ok(Some(v)),
        Some(_) => Ok(None),
    }
}

/// Extract the numeric magnitude from a length token such as `1.5` or
/// `1.5mm`, mirroring `FormatTools.parseLength` insofar as the Rust model
/// needs it (the unit is ignored — we only validate/store the value).
fn parse_length_value(s: &str) -> Option<f64> {
    let s = s.trim();
    if let Ok(v) = s.parse::<f64>() {
        return Some(v);
    }
    // Strip a trailing unit suffix and retry.
    let split = s
        .find(|c: char| c.is_alphabetic() || c == '%' || c == ' ')
        .unwrap_or(s.len());
    let (num, _unit) = s.split_at(split);
    num.trim().parse::<f64>().ok()
}

/// Parsed-but-unrepresentable parameters, recorded as original metadata.
///
/// The Rust [`ImageMetadata`] model has no structural home for many of the
/// things Java's FakeReader fabricates (HCS plates, ROI shapes,
/// annotations, channel colors/wavelengths, physical sizes, ...).  Java
/// still parses and validates these; we mirror that and stash the results
/// here so they end up in `series_metadata` rather than being silently
/// dropped.
struct FakeParams {
    name: Option<String>,

    size_x: u32,
    size_y: u32,
    size_z: u32,
    size_c: u32,
    size_t: u32,
    thumb_size_x: i32,
    thumb_size_y: i32,
    pixel_type: PixelType,
    bits_per_pixel: i32,
    rgb: u32,
    dim_order: Option<String>,
    order_certain: bool,
    little: bool,
    interleaved: bool,
    indexed: bool,
    false_color: bool,
    metadata_complete: bool,
    thumbnail: bool,
    with_microbeam: bool,
    with_instrument: bool,

    series_count: u32,
    resolution_count: u32,
    resolution_scale: u32,
    lut_length: i32,

    scale_factor: f64,
    exposure_time: Option<f64>,
    acquisition_date: Option<String>,

    screens: i32,
    plates: i32,
    plate_rows: i32,
    plate_cols: i32,
    fields: i32,
    plate_acqs: i32,

    ann_long: i32,
    ann_double: i32,
    ann_map: i32,
    ann_comment: i32,
    ann_bool: i32,
    ann_time: i32,
    ann_tag: i32,
    ann_term: i32,
    ann_xml: i32,

    ellipses: i32,
    labels: i32,
    lines: i32,
    masks: i32,
    points: i32,
    polygons: i32,
    polylines: i32,
    rectangles: i32,

    physical_size_x: Option<f64>,
    physical_size_y: Option<f64>,
    physical_size_z: Option<f64>,

    default_color: Option<i32>,
    color: Vec<Option<i32>>,
    emission_wavelengths: Vec<Option<f64>>,
    excitation_wavelengths: Vec<Option<f64>>,
    series_channel_names: HashMap<usize, Vec<Option<String>>>,
    series_emission_wavelengths: HashMap<usize, Vec<Option<f64>>>,
    series_excitation_wavelengths: HashMap<usize, Vec<Option<f64>>>,

    sleep_open_bytes: i32,
    sleep_init_file: i32,
    label_planes: bool,
}

impl FakeParams {
    /// Initialize with the same defaults as Java's `initFile` locals.
    fn with_defaults() -> Self {
        FakeParams {
            name: None,
            size_x: DEFAULT_SIZE_X,
            size_y: DEFAULT_SIZE_Y,
            size_z: DEFAULT_SIZE_Z,
            size_c: DEFAULT_SIZE_C,
            size_t: DEFAULT_SIZE_T,
            thumb_size_x: 0,
            thumb_size_y: 0,
            pixel_type: PixelType::Uint8,
            bits_per_pixel: 0,
            rgb: DEFAULT_RGB_CHANNEL_COUNT,
            dim_order: None,
            order_certain: true,
            little: true,
            interleaved: false,
            indexed: false,
            false_color: false,
            metadata_complete: true,
            thumbnail: false,
            with_microbeam: false,
            with_instrument: false,
            series_count: 1,
            resolution_count: 1,
            resolution_scale: DEFAULT_RESOLUTION_SCALE,
            lut_length: 3,
            scale_factor: 1.0,
            exposure_time: None,
            acquisition_date: None,
            screens: 0,
            plates: 0,
            plate_rows: 0,
            plate_cols: 0,
            fields: 0,
            plate_acqs: 0,
            ann_long: 0,
            ann_double: 0,
            ann_map: 0,
            ann_comment: 0,
            ann_bool: 0,
            ann_time: 0,
            ann_tag: 0,
            ann_term: 0,
            ann_xml: 0,
            ellipses: 0,
            labels: 0,
            lines: 0,
            masks: 0,
            points: 0,
            polygons: 0,
            polylines: 0,
            rectangles: 0,
            physical_size_x: None,
            physical_size_y: None,
            physical_size_z: None,
            default_color: None,
            color: Vec::new(),
            emission_wavelengths: Vec::new(),
            excitation_wavelengths: Vec::new(),
            series_channel_names: HashMap::new(),
            series_emission_wavelengths: HashMap::new(),
            series_excitation_wavelengths: HashMap::new(),
            sleep_open_bytes: 0,
            sleep_init_file: 0,
            label_planes: false,
        }
    }
}

/// Parse the `&`-separated token loop, mirroring Java's `initFile` loop
/// (FakeReader.java lines 742-861).  The first token is the image name;
/// each remaining `key=value` token updates one field.
fn parse_tokens(tokens: &[&str]) -> Result<FakeParams> {
    let mut p = FakeParams::with_defaults();

    for token in tokens {
        if p.name.is_none() {
            // first token is the image name
            p.name = Some((*token).to_string());
            continue;
        }
        let (key, value) = match token.split_once('=') {
            Some(kv) => kv,
            None => {
                // ignoring token (Java logs a warning)
                continue;
            }
        };

        let bool_value = value == "true";
        // Java: doubleValue = parseDouble(value) or NaN; intValue = NaN ? -1 : (int) doubleValue
        let double_value = value.parse::<f64>().unwrap_or(f64::NAN);
        let int_value: i32 = if double_value.is_nan() {
            -1
        } else {
            double_value as i32
        };

        match key {
            "sizeX" => p.size_x = int_value as u32,
            "sizeY" => p.size_y = int_value as u32,
            "sizeZ" => p.size_z = int_value as u32,
            "sizeC" => p.size_c = int_value as u32,
            "sizeT" => p.size_t = int_value as u32,
            "thumbSizeX" => p.thumb_size_x = int_value,
            "thumbSizeY" => p.thumb_size_y = int_value,
            "pixelType" => {
                p.pixel_type = pixel_type_from_string(value).ok_or_else(|| {
                    BioFormatsError::InvalidData(format!("Unknown pixel type: {}", value))
                })?;
            }
            "bitsPerPixel" => p.bits_per_pixel = int_value,
            "rgb" => p.rgb = int_value as u32,
            "dimOrder" => p.dim_order = Some(value.to_uppercase()),
            "orderCertain" => p.order_certain = bool_value,
            "little" => p.little = bool_value,
            "interleaved" => p.interleaved = bool_value,
            "indexed" => p.indexed = bool_value,
            "falseColor" => p.false_color = bool_value,
            "metadataComplete" => p.metadata_complete = bool_value,
            "thumbnail" => p.thumbnail = bool_value,
            "series" => p.series_count = int_value as u32,
            "resolutions" => p.resolution_count = int_value as u32,
            "resolutionScale" => p.resolution_scale = int_value as u32,
            "lutLength" => p.lut_length = int_value,
            "scaleFactor" => p.scale_factor = double_value,
            "exposureTime" => p.exposure_time = Some(double_value),
            "acquisitionDate" => p.acquisition_date = Some(value.to_string()),
            "screens" => p.screens = int_value,
            "plates" => p.plates = int_value,
            "plateRows" => p.plate_rows = int_value,
            "plateCols" => p.plate_cols = int_value,
            "fields" => p.fields = int_value,
            "plateAcqs" => p.plate_acqs = int_value,
            "withMicrobeam" => p.with_microbeam = bool_value,
            "withInstrument" => p.with_instrument = bool_value,
            "annLong" => p.ann_long = int_value,
            "annDouble" => p.ann_double = int_value,
            "annMap" => p.ann_map = int_value,
            "annComment" => p.ann_comment = int_value,
            "annBool" => p.ann_bool = int_value,
            "annTime" => p.ann_time = int_value,
            "annTag" => p.ann_tag = int_value,
            "annTerm" => p.ann_term = int_value,
            "annXml" => p.ann_xml = int_value,
            "ellipses" => p.ellipses = int_value,
            "labels" => p.labels = int_value,
            "lines" => p.lines = int_value,
            "masks" => p.masks = int_value,
            "points" => p.points = int_value,
            "polygons" => p.polygons = int_value,
            "polylines" => p.polylines = int_value,
            "rectangles" => p.rectangles = int_value,
            "physicalSizeX" => p.physical_size_x = parse_physical_size(value)?,
            "physicalSizeY" => p.physical_size_y = parse_physical_size(value)?,
            "physicalSizeZ" => p.physical_size_z = parse_physical_size(value)?,
            "color" => p.default_color = Some(parse_color(value)),
            "sleepOpenBytes" => p.sleep_open_bytes = int_value,
            "sleepInitFile" => p.sleep_init_file = int_value,
            "labelPlanes" => p.label_planes = bool_value,
            _ => {
                // 'color' and 'color_x' can be used together, but 'color_x'
                // takes precedence; 'color' fills missing/invalid 'color_x'.
                if let Some(idx) = key.strip_prefix("color_") {
                    if let Ok(index) = idx.parse::<usize>() {
                        while index >= p.color.len() {
                            p.color.push(None);
                        }
                        p.color[index] = Some(parse_color(value));
                    }
                } else if let Some(idx) = key.strip_prefix("emission_") {
                    if let Ok(index) = idx.parse::<usize>() {
                        while index >= p.emission_wavelengths.len() {
                            p.emission_wavelengths.push(None);
                        }
                        p.emission_wavelengths[index] = parse_wavelength(value)?;
                    }
                } else if let Some(idx) = key.strip_prefix("excitation_") {
                    if let Ok(index) = idx.parse::<usize>() {
                        while index >= p.excitation_wavelengths.len() {
                            p.excitation_wavelengths.push(None);
                        }
                        p.excitation_wavelengths[index] = parse_wavelength(value)?;
                    }
                }
                // Any other unknown key is ignored, as in Java.
            }
        }
    }

    Ok(p)
}

#[derive(Default)]
struct FakeIni {
    default_tokens: Vec<String>,
    series_tables: HashMap<usize, HashMap<String, String>>,
}

fn parse_fake_ini(path: &Path) -> Result<FakeIni> {
    let file = File::open(path).map_err(BioFormatsError::Io)?;
    let reader = BufReader::new(file);
    let mut ini = FakeIni::default();
    let mut section: Option<String> = None;

    for line in reader.lines() {
        let line = line.map_err(BioFormatsError::Io)?;
        let line = line.trim();
        if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = Some(line[1..line.len() - 1].trim().to_string());
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim().to_string();
        let value = value.trim().to_string();

        match section.as_deref() {
            None | Some("") => ini.default_tokens.push(format!("{key}={value}")),
            Some(name) if name.starts_with("series_") => {
                if let Ok(index) = name["series_".len()..].parse::<usize>() {
                    ini.series_tables
                        .entry(index)
                        .or_default()
                        .insert(key, value);
                }
            }
            Some("GlobalMetadata") => {}
            Some(_) => {}
        }
    }

    Ok(ini)
}

fn set_indexed<T>(values: &mut Vec<Option<T>>, index: usize, value: T) {
    while index >= values.len() {
        values.push(None);
    }
    values[index] = Some(value);
}

fn apply_fake_ini_series_tables(
    p: &mut FakeParams,
    tables: HashMap<usize, HashMap<String, String>>,
) -> Result<()> {
    for (series, table) in tables {
        for (key, value) in table {
            if let Some(index) = key.strip_prefix("ChannelName_") {
                if let Ok(channel) = index.parse::<usize>() {
                    let names = p.series_channel_names.entry(series).or_default();
                    set_indexed(names, channel, value);
                }
            } else if let Some(index) = key.strip_prefix("ChannelEmissionWavelength_") {
                if let Ok(channel) = index.parse::<usize>() {
                    if let Some(wavelength) = parse_wavelength(&value)? {
                        let wavelengths = p.series_emission_wavelengths.entry(series).or_default();
                        set_indexed(wavelengths, channel, wavelength);
                    }
                }
            } else if let Some(index) = key.strip_prefix("ChannelExcitationWavelength_") {
                if let Ok(channel) = index.parse::<usize>() {
                    if let Some(wavelength) = parse_wavelength(&value)? {
                        let wavelengths =
                            p.series_excitation_wavelengths.entry(series).or_default();
                        set_indexed(wavelengths, channel, wavelength);
                    }
                }
            }
        }
    }
    Ok(())
}

/// Convert a validated dimension-order string to [`DimensionOrder`],
/// mirroring `MetadataTools.getDimensionOrder` (which throws on an invalid
/// order).
fn dimension_order_from_string(s: &str) -> Result<DimensionOrder> {
    match s {
        "XYCTZ" => Ok(DimensionOrder::XYCTZ),
        "XYCZT" => Ok(DimensionOrder::XYCZT),
        "XYTCZ" => Ok(DimensionOrder::XYTCZ),
        "XYTZC" => Ok(DimensionOrder::XYTZC),
        "XYZCT" => Ok(DimensionOrder::XYZCT),
        "XYZTC" => Ok(DimensionOrder::XYZTC),
        _ => Err(BioFormatsError::InvalidData(format!(
            "Invalid dimension order: {}",
            s
        ))),
    }
}

fn decompose_plane(
    index: u32,
    size_z: u32,
    effective_size_c: u32,
    size_t: u32,
    order: DimensionOrder,
) -> (u32, u32, u32) {
    match order {
        DimensionOrder::XYZCT => {
            let z = index % size_z;
            let c = (index / size_z) % effective_size_c;
            let t = index / (size_z * effective_size_c);
            (z, c, t)
        }
        DimensionOrder::XYZTC => {
            let z = index % size_z;
            let t = (index / size_z) % size_t;
            let c = index / (size_z * size_t);
            (z, c, t)
        }
        DimensionOrder::XYCZT => {
            let c = index % effective_size_c;
            let z = (index / effective_size_c) % size_z;
            let t = index / (effective_size_c * size_z);
            (z, c, t)
        }
        DimensionOrder::XYCTZ => {
            let c = index % effective_size_c;
            let t = (index / effective_size_c) % size_t;
            let z = index / (effective_size_c * size_t);
            (z, c, t)
        }
        DimensionOrder::XYTCZ => {
            let t = index % size_t;
            let c = (index / size_t) % effective_size_c;
            let z = index / (size_t * effective_size_c);
            (z, c, t)
        }
        DimensionOrder::XYTZC => {
            let t = index % size_t;
            let z = (index / size_t) % size_z;
            let c = index / (size_t * size_z);
            (z, c, t)
        }
    }
}

fn rgb_channel_count(meta: &ImageMetadata) -> u32 {
    if !meta.is_rgb {
        return 1;
    }
    let zt = meta.size_z.saturating_mul(meta.size_t).max(1);
    let effective_size_c = (meta.image_count / zt).max(1);
    (meta.size_c / effective_size_c).max(1)
}

fn scale_factor(meta: &ImageMetadata) -> f64 {
    match meta.series_metadata.get("scaleFactor") {
        Some(MetadataValue::Float(v)) => *v,
        _ => 1.0,
    }
}

fn resolution_scale(meta: &ImageMetadata) -> u32 {
    match meta.series_metadata.get("resolutionScale") {
        Some(MetadataValue::Int(v)) if *v > 1 => *v as u32,
        _ => DEFAULT_RESOLUTION_SCALE,
    }
}

fn fake_resolution_metadata(base: &ImageMetadata, level: usize) -> ImageMetadata {
    let mut meta = base.clone();
    if level > 0 {
        let scale = resolution_scale(base).saturating_pow(level as u32).max(1);
        meta.size_x /= scale;
        meta.size_y /= scale;
    }
    meta
}

fn signed_min(pixel_type: PixelType) -> i64 {
    match pixel_type {
        PixelType::Int8 => i8::MIN as i64,
        PixelType::Int16 => i16::MIN as i64,
        PixelType::Int32 => i32::MIN as i64,
        _ => 0,
    }
}

fn pack_fake_pixel(pixel_type: PixelType, little: bool, value: i64, out: &mut [u8]) {
    match pixel_type {
        PixelType::Float32 => {
            let bits = (value as f32).to_bits();
            let bytes = if little {
                bits.to_le_bytes()
            } else {
                bits.to_be_bytes()
            };
            out.copy_from_slice(&bytes);
        }
        PixelType::Float64 => {
            let bits = (value as f64).to_bits();
            let bytes = if little {
                bits.to_le_bytes()
            } else {
                bits.to_be_bytes()
            };
            out.copy_from_slice(&bytes);
        }
        _ => {
            let bytes = if little {
                value.to_le_bytes()
            } else {
                value.to_be_bytes()
            };
            if little {
                out.copy_from_slice(&bytes[..out.len()]);
            } else {
                let start = bytes.len() - out.len();
                out.copy_from_slice(&bytes[start..]);
            }
        }
    }
}

fn fake_plane_region(
    meta: &ImageMetadata,
    series_index: usize,
    plane_index: u32,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
) -> Result<Vec<u8>> {
    if plane_index >= meta.image_count {
        return Err(BioFormatsError::PlaneOutOfRange(plane_index));
    }
    crate::common::region::validate_region("Fake", meta.size_x, meta.size_y, x, y, w, h)?;

    let rgb = rgb_channel_count(meta);
    let effective_size_c = (meta.size_c / rgb).max(1);
    let (z_index, c_index, t_index) = decompose_plane(
        plane_index,
        meta.size_z,
        effective_size_c,
        meta.size_t,
        meta.dimension_order,
    );
    let bps = meta.pixel_type.bytes_per_sample();
    let plane_len = (w as usize)
        .checked_mul(h as usize)
        .and_then(|v| v.checked_mul(rgb as usize))
        .and_then(|v| v.checked_mul(bps))
        .ok_or_else(|| BioFormatsError::Format("Fake plane size overflows".to_string()))?;
    let mut buf = vec![0u8; plane_len];
    let min = signed_min(meta.pixel_type);
    let scale = scale_factor(meta);

    for c_offset in 0..rgb {
        let channel = rgb * c_index + c_offset;
        for row in 0..h {
            let yy = y + row;
            for col in 0..w {
                let xx = x + col;
                let mut pixel = min + i64::from(xx);
                let mut special_pixel = false;
                if yy < BOX_SIZE {
                    special_pixel = true;
                    pixel = match xx / BOX_SIZE {
                        0 => series_index as i64,
                        1 => plane_index as i64,
                        2 => z_index as i64,
                        3 => channel as i64,
                        4 => t_index as i64,
                        _ => {
                            special_pixel = false;
                            pixel
                        }
                    };
                }

                if !special_pixel {
                    pixel = (scale * pixel as f64) as i64;
                }

                let sample_index = if meta.is_interleaved {
                    (w as usize * rgb as usize * row as usize)
                        + (rgb as usize * col as usize)
                        + c_offset as usize
                } else {
                    (h as usize * w as usize * c_offset as usize)
                        + (w as usize * row as usize)
                        + col as usize
                };
                let off = sample_index * bps;
                pack_fake_pixel(
                    meta.pixel_type,
                    meta.is_little_endian,
                    pixel,
                    &mut buf[off..off + bps],
                );
            }
        }
    }

    Ok(buf)
}

/// Validate parameters and build per-series [`ImageMetadata`], mirroring the
/// "sanity checks" and "populate core metadata" sections of Java's
/// `initFile` (lines 863-973).
fn build_metadata(mut p: FakeParams) -> Result<Vec<ImageMetadata>> {
    // do some sanity checks
    if (p.size_x as i32) < 1 {
        return Err(BioFormatsError::InvalidData(format!(
            "Invalid sizeX: {}",
            p.size_x as i32
        )));
    }
    if (p.size_y as i32) < 1 {
        return Err(BioFormatsError::InvalidData(format!(
            "Invalid sizeY: {}",
            p.size_y as i32
        )));
    }
    if (p.size_z as i32) < 1 {
        return Err(BioFormatsError::InvalidData(format!(
            "Invalid sizeZ: {}",
            p.size_z as i32
        )));
    }
    if (p.size_c as i32) < 1 {
        return Err(BioFormatsError::InvalidData(format!(
            "Invalid sizeC: {}",
            p.size_c as i32
        )));
    }
    if (p.size_t as i32) < 1 {
        return Err(BioFormatsError::InvalidData(format!(
            "Invalid sizeT: {}",
            p.size_t as i32
        )));
    }
    if p.thumb_size_x < 0 {
        return Err(BioFormatsError::InvalidData(format!(
            "Invalid thumbSizeX: {}",
            p.thumb_size_x
        )));
    }
    if p.thumb_size_y < 0 {
        return Err(BioFormatsError::InvalidData(format!(
            "Invalid thumbSizeY: {}",
            p.thumb_size_y
        )));
    }
    if p.rgb < 1 || p.rgb > p.size_c || p.size_c % p.rgb != 0 {
        return Err(BioFormatsError::InvalidData(format!(
            "Invalid sizeC/rgb combination: {}/{}",
            p.size_c, p.rgb
        )));
    }

    // make sure the dimension order is correct for RGB data and set the
    // correct default if not explicitly specified
    let dim_order_str = if p.rgb > 1 {
        let mut new_dim_order = p
            .dim_order
            .clone()
            .unwrap_or_else(|| DEFAULT_RGB_DIMENSION_ORDER.to_string());
        if !new_dim_order.starts_with("XYC") {
            let z = new_dim_order.find('Z');
            let t = new_dim_order.find('T');
            new_dim_order = match (z, t) {
                (Some(z), Some(t)) if z < t => "XYCZT".to_string(),
                _ => "XYCTZ".to_string(),
            };
        }
        new_dim_order
    } else {
        p.dim_order
            .clone()
            .unwrap_or_else(|| DEFAULT_DIMENSION_ORDER.to_string())
    };

    // validate the dimension order
    let dim_order = dimension_order_from_string(&dim_order_str)?;

    if p.false_color && !p.indexed {
        return Err(BioFormatsError::InvalidData(
            "False color images must be indexed".to_string(),
        ));
    }
    if (p.series_count as i32) < 1 {
        return Err(BioFormatsError::InvalidData(format!(
            "Invalid seriesCount: {}",
            p.series_count as i32
        )));
    }
    if p.lut_length < 1 {
        return Err(BioFormatsError::InvalidData(format!(
            "Invalid lutLength: {}",
            p.lut_length
        )));
    }
    if (p.resolution_count as i32) < 1 {
        return Err(BioFormatsError::InvalidData(format!(
            "Invalid resolutionCount: {}",
            p.resolution_count as i32
        )));
    }
    if (p.resolution_scale as i32) <= 1 {
        return Err(BioFormatsError::InvalidData(format!(
            "Invalid resolutionScale: {}",
            p.resolution_scale as i32
        )));
    }

    // SPW (screens/plates/wells) overrides the series count to match the
    // generated image count.  The Rust model cannot build OME HCS metadata,
    // so we replicate Java's count arithmetic (XMLMockObjects produces one
    // Image per well-sample) and record the layout as original metadata.
    let has_spw = p.screens > 0
        || p.plates > 0
        || p.plate_rows > 0
        || p.plate_cols > 0
        || p.fields > 0
        || p.plate_acqs > 0;
    if has_spw {
        if p.screens < 0 {
            p.screens = 0;
        }
        if p.plates <= 0 {
            p.plates = 1;
        }
        if p.plate_rows <= 0 {
            p.plate_rows = 1;
        }
        if p.plate_cols <= 0 {
            p.plate_cols = 1;
        }
        if p.fields <= 0 {
            p.fields = 1;
        }
        if p.plate_acqs <= 0 {
            p.plate_acqs = 1;
        }
        // imageCount = screens? * plates * rows * cols * fields * acqs
        let screen_count = p.screens.max(1);
        let image_count =
            screen_count * p.plates * p.plate_rows * p.plate_cols * p.fields * p.plate_acqs;
        if image_count > 0 {
            p.series_count = image_count as u32;
        }
    }

    // populate core metadata
    let eff_size_c = p.size_c / p.rgb;
    let bps = p.pixel_type.bytes_per_sample();
    // bitsPerPixel default (0) means "use the pixel type's natural width".
    let bits_per_pixel: u8 = if p.bits_per_pixel > 0 {
        p.bits_per_pixel as u8
    } else {
        (bps * 8) as u8
    };

    let original = build_original_metadata(&p, &dim_order_str);

    let name = p.name.clone().unwrap_or_default();
    let mut series = Vec::with_capacity(p.series_count as usize);
    for s in 0..p.series_count {
        let mut sm = original.clone();
        let image_name = if s > 0 {
            format!("{} {}", name, s + 1)
        } else {
            name.clone()
        };
        sm.insert("Image name".to_string(), MetadataValue::String(image_name));
        let series_index = s as usize;
        if let Some(names) = p.series_channel_names.get(&series_index) {
            for (i, name) in names.iter().enumerate() {
                if let Some(name) = name {
                    sm.insert(
                        format!("ChannelName_{}", i),
                        MetadataValue::String(name.clone()),
                    );
                    sm.insert(
                        format!("channel.{}.name", i),
                        MetadataValue::String(name.clone()),
                    );
                }
            }
        }
        if let Some(wavelengths) = p.series_emission_wavelengths.get(&series_index) {
            for (i, wavelength) in wavelengths.iter().enumerate() {
                if let Some(wavelength) = wavelength {
                    sm.insert(
                        format!("ChannelEmissionWavelength_{}", i),
                        MetadataValue::Float(*wavelength),
                    );
                    sm.insert(
                        format!("channel.{}.emission_wavelength", i),
                        MetadataValue::Float(*wavelength),
                    );
                }
            }
        }
        if let Some(wavelengths) = p.series_excitation_wavelengths.get(&series_index) {
            for (i, wavelength) in wavelengths.iter().enumerate() {
                if let Some(wavelength) = wavelength {
                    sm.insert(
                        format!("ChannelExcitationWavelength_{}", i),
                        MetadataValue::Float(*wavelength),
                    );
                    sm.insert(
                        format!("channel.{}.excitation_wavelength", i),
                        MetadataValue::Float(*wavelength),
                    );
                }
            }
        }

        let ms = ImageMetadata {
            size_x: p.size_x,
            size_y: p.size_y,
            size_z: p.size_z,
            size_c: p.size_c,
            size_t: p.size_t,
            pixel_type: p.pixel_type,
            bits_per_pixel: bits_per_pixel.into(),
            image_count: p.size_z * eff_size_c * p.size_t,
            dimension_order: dim_order,
            is_rgb: p.rgb > 1,
            is_interleaved: p.interleaved,
            is_indexed: p.indexed,
            is_little_endian: p.little,
            resolution_count: p.resolution_count,
            thumbnail: false,
            series_metadata: sm,
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        };
        series.push(ms);
    }

    Ok(series)
}

/// Collect parsed-but-unrepresentable parameters into an original-metadata
/// map.  Only entries with non-default values are recorded, so simple
/// `.fake` names stay clean.
fn build_original_metadata(p: &FakeParams, dim_order_str: &str) -> HashMap<String, MetadataValue> {
    let mut m = HashMap::new();

    let put_int = |m: &mut HashMap<String, MetadataValue>, k: &str, v: i32| {
        if v != 0 {
            m.insert(k.to_string(), MetadataValue::Int(v as i64));
        }
    };

    put_int(&mut m, "thumbSizeX", p.thumb_size_x);
    put_int(&mut m, "thumbSizeY", p.thumb_size_y);
    put_int(&mut m, "bitsPerPixel", p.bits_per_pixel);

    m.insert(
        "dimOrder".to_string(),
        MetadataValue::String(dim_order_str.to_string()),
    );
    if !p.order_certain {
        m.insert("orderCertain".to_string(), MetadataValue::Bool(false));
    }
    if p.false_color {
        m.insert("falseColor".to_string(), MetadataValue::Bool(true));
    }
    if !p.metadata_complete {
        m.insert("metadataComplete".to_string(), MetadataValue::Bool(false));
    }
    if p.thumbnail {
        m.insert("thumbnail".to_string(), MetadataValue::Bool(true));
    }
    if p.with_microbeam {
        m.insert("withMicrobeam".to_string(), MetadataValue::Bool(true));
    }
    if p.with_instrument {
        m.insert("withInstrument".to_string(), MetadataValue::Bool(true));
    }

    if p.scale_factor != 1.0 {
        m.insert(
            "scaleFactor".to_string(),
            MetadataValue::Float(p.scale_factor),
        );
    }
    if p.resolution_scale != DEFAULT_RESOLUTION_SCALE {
        m.insert(
            "resolutionScale".to_string(),
            MetadataValue::Int(p.resolution_scale as i64),
        );
    }
    if let Some(e) = p.exposure_time {
        m.insert("exposureTime".to_string(), MetadataValue::Float(e));
    }
    if let Some(d) = &p.acquisition_date {
        m.insert(
            "acquisitionDate".to_string(),
            MetadataValue::String(d.clone()),
        );
    }

    // SPW / HCS layout
    put_int(&mut m, "screens", p.screens);
    put_int(&mut m, "plates", p.plates);
    put_int(&mut m, "plateRows", p.plate_rows);
    put_int(&mut m, "plateCols", p.plate_cols);
    put_int(&mut m, "fields", p.fields);
    put_int(&mut m, "plateAcqs", p.plate_acqs);

    // annotations
    put_int(&mut m, "annLong", p.ann_long);
    put_int(&mut m, "annDouble", p.ann_double);
    put_int(&mut m, "annMap", p.ann_map);
    put_int(&mut m, "annComment", p.ann_comment);
    put_int(&mut m, "annBool", p.ann_bool);
    put_int(&mut m, "annTime", p.ann_time);
    put_int(&mut m, "annTag", p.ann_tag);
    put_int(&mut m, "annTerm", p.ann_term);
    put_int(&mut m, "annXml", p.ann_xml);

    // ROI shapes
    put_int(&mut m, "ellipses", p.ellipses);
    put_int(&mut m, "labels", p.labels);
    put_int(&mut m, "lines", p.lines);
    put_int(&mut m, "masks", p.masks);
    put_int(&mut m, "points", p.points);
    put_int(&mut m, "polygons", p.polygons);
    put_int(&mut m, "polylines", p.polylines);
    put_int(&mut m, "rectangles", p.rectangles);

    // physical sizes
    if let Some(v) = p.physical_size_x {
        m.insert("physicalSizeX".to_string(), MetadataValue::Float(v));
    }
    if let Some(v) = p.physical_size_y {
        m.insert("physicalSizeY".to_string(), MetadataValue::Float(v));
    }
    if let Some(v) = p.physical_size_z {
        m.insert("physicalSizeZ".to_string(), MetadataValue::Float(v));
    }

    // channel colors / wavelengths
    if let Some(c) = p.default_color {
        m.insert("color".to_string(), MetadataValue::Int(c as i64));
    }
    for (i, c) in p.color.iter().enumerate() {
        if let Some(c) = c {
            m.insert(format!("color_{}", i), MetadataValue::Int(*c as i64));
        }
    }
    for (i, w) in p.emission_wavelengths.iter().enumerate() {
        if let Some(w) = w {
            m.insert(format!("emission_{}", i), MetadataValue::Float(*w));
            m.insert(
                format!("channel.{}.emission_wavelength", i),
                MetadataValue::Float(*w),
            );
        }
    }
    for (i, w) in p.excitation_wavelengths.iter().enumerate() {
        if let Some(w) = w {
            m.insert(format!("excitation_{}", i), MetadataValue::Float(*w));
            m.insert(
                format!("channel.{}.excitation_wavelength", i),
                MetadataValue::Float(*w),
            );
        }
    }

    // misc debugging
    put_int(&mut m, "sleepOpenBytes", p.sleep_open_bytes);
    put_int(&mut m, "sleepInitFile", p.sleep_init_file);
    if p.label_planes {
        m.insert("labelPlanes".to_string(), MetadataValue::Bool(true));
    }

    m
}

/// Top-level entry point mirroring Java's `initFile`: split the filename
/// stem into `&`-separated tokens, parse them, validate, and build the
/// per-series core metadata.
fn init_file(path: &Path) -> Result<Vec<ImageMetadata>> {
    // Java strips the extension then splits on '&'.  `file_stem` already
    // drops the trailing `.fake`, leaving e.g. `name&sizeX=2&sizeY=1`.
    let file_name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    let is_fake_ini = file_name.to_ascii_lowercase().ends_with(".fake.ini");
    let stem = if is_fake_ini {
        file_name
            .strip_suffix(".ini")
            .and_then(|s| s.strip_suffix(".fake"))
            .unwrap_or(file_name)
            .to_string()
    } else {
        path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string()
    };

    let mut owned_tokens: Vec<String> = stem
        .split(TOKEN_SEPARATOR)
        .map(|token| token.to_string())
        .collect();
    let mut series_tables = HashMap::new();
    if is_fake_ini {
        let ini = parse_fake_ini(path)?;
        owned_tokens.extend(ini.default_tokens);
        series_tables = ini.series_tables;
    }

    let tokens: Vec<&str> = owned_tokens.iter().map(String::as_str).collect();
    let mut params = parse_tokens(&tokens)?;
    apply_fake_ini_series_tables(&mut params, series_tables)?;
    build_metadata(params)
}

fn fake_meta_i64(meta: &ImageMetadata, key: &str) -> Option<i64> {
    match meta.series_metadata.get(key) {
        Some(MetadataValue::Int(v)) => Some(*v),
        _ => None,
    }
}

fn fake_meta_f64(meta: &ImageMetadata, key: &str) -> Option<f64> {
    match meta.series_metadata.get(key) {
        Some(MetadataValue::Float(v)) if v.is_finite() => Some(*v),
        Some(MetadataValue::Int(v)) => Some(*v as f64),
        _ => None,
    }
}

fn fake_meta_bool(meta: &ImageMetadata, key: &str) -> bool {
    matches!(
        meta.series_metadata.get(key),
        Some(MetadataValue::Bool(true))
    )
}

fn fake_meta_string(meta: &ImageMetadata, key: &str) -> Option<String> {
    match meta.series_metadata.get(key) {
        Some(MetadataValue::String(v)) if !v.trim().is_empty() => Some(v.clone()),
        _ => None,
    }
}

fn fake_effective_size_c(meta: &ImageMetadata) -> usize {
    let rgb = if meta.is_rgb {
        meta.size_c.min(3).max(1)
    } else {
        1
    };
    (meta.size_c / rgb).max(1) as usize
}

fn fake_has_spw(meta: &ImageMetadata) -> bool {
    [
        "screens",
        "plates",
        "plateRows",
        "plateCols",
        "fields",
        "plateAcqs",
    ]
    .iter()
    .any(|key| fake_meta_i64(meta, key).unwrap_or(0) > 0)
}

fn fake_spw_count(meta: &ImageMetadata, key: &str, default: u32) -> u32 {
    fake_meta_i64(meta, key)
        .filter(|v| *v > 0)
        .map(|v| v as u32)
        .unwrap_or(default)
}

fn fake_ome_image(
    meta: &ImageMetadata,
    image_index: usize,
) -> crate::common::ome_metadata::OmeImage {
    let mut ome = crate::common::ome_metadata::OmeMetadata::from_image_metadata(meta);
    let mut image = ome.images.pop().unwrap_or_default();
    image.name = fake_meta_string(meta, "Image name");
    image.acquisition_date = fake_meta_string(meta, "acquisitionDate");
    image.physical_size_x = fake_meta_f64(meta, "physicalSizeX").filter(|v| *v > 0.0);
    image.physical_size_y = fake_meta_f64(meta, "physicalSizeY").filter(|v| *v > 0.0);
    image.physical_size_z = fake_meta_f64(meta, "physicalSizeZ").filter(|v| *v > 0.0);

    let channel_count = fake_effective_size_c(meta);
    image.channels.resize_with(
        channel_count,
        crate::common::ome_metadata::OmeChannel::default,
    );
    for c in 0..channel_count {
        let channel = &mut image.channels[c];
        if channel.samples_per_pixel == 0 {
            channel.samples_per_pixel = if meta.is_rgb { 3 } else { 1 };
        }
        if channel.name.is_none() {
            channel.name = fake_meta_string(meta, &format!("ChannelName_{c}"));
        }
        if let Some(color) =
            fake_meta_i64(meta, &format!("color_{c}")).or_else(|| fake_meta_i64(meta, "color"))
        {
            channel.color = Some(color as i32);
        }
    }

    if fake_meta_bool(meta, "withInstrument") || fake_has_spw(meta) {
        image.instrument_ref = Some(0);
        image.objective_ref = Some(0);
        image.light_paths.resize_with(
            channel_count,
            crate::common::ome_metadata::OmeLightPath::default,
        );
        for c in 0..channel_count {
            image.channels[c].detector_ref = Some(crate::common::ome_metadata::create_lsid(
                "Detector",
                &[0, 0],
            ));
            image.channels[c].light_source_settings_id = Some(format!("LightSource:0:{}", c % 5));
            image.light_paths[c].dichroic_id = Some(crate::common::ome_metadata::create_lsid(
                "Dichroic",
                &[0, 0],
            ));
            image.light_paths[c].emission_filter_ids =
                vec![crate::common::ome_metadata::create_lsid("Filter", &[0, 0])];
            image.light_paths[c].excitation_filter_ids =
                vec![crate::common::ome_metadata::create_lsid("Filter", &[0, 1])];
        }
    }

    if let Some(exposure) = fake_meta_f64(meta, "exposureTime") {
        for plane in &mut image.planes {
            plane.exposure_time = Some(exposure);
        }
        if image.planes.is_empty() {
            image.planes.push(crate::common::ome_metadata::OmePlane {
                the_z: 0,
                the_c: 0,
                the_t: 0,
                exposure_time: Some(exposure),
                ..Default::default()
            });
        }
    }

    if image.name.is_none() {
        image.name = Some(format!("Series {image_index}"));
    }
    image
}

fn fake_instrument() -> crate::common::ome_metadata::OmeInstrument {
    crate::common::ome_metadata::OmeInstrument {
        id: Some(crate::common::ome_metadata::create_lsid("Instrument", &[0])),
        microscope_model: Some("Fake microscope".to_string()),
        objectives: vec![crate::common::ome_metadata::OmeObjective {
            id: Some(crate::common::ome_metadata::create_lsid(
                "Objective",
                &[0, 0],
            )),
            model: Some("Fake objective".to_string()),
            ..Default::default()
        }],
        detectors: vec![crate::common::ome_metadata::OmeDetector {
            id: Some(crate::common::ome_metadata::create_lsid(
                "Detector",
                &[0, 0],
            )),
            model: Some("Fake detector".to_string()),
            ..Default::default()
        }],
        light_sources: (0..5)
            .map(|i| crate::common::ome_metadata::OmeLightSource {
                id: Some(format!("LightSource:0:{i}")),
                light_source_type: Some(
                    match i {
                        0 | 4 => "Laser",
                        1 => "Arc",
                        2 => "Filament",
                        _ => "LED",
                    }
                    .to_string(),
                ),
                ..Default::default()
            })
            .collect(),
        filters: vec![
            crate::common::ome_metadata::OmeFilter {
                id: Some(crate::common::ome_metadata::create_lsid("Filter", &[0, 0])),
                filter_type: Some("Emission".to_string()),
                ..Default::default()
            },
            crate::common::ome_metadata::OmeFilter {
                id: Some(crate::common::ome_metadata::create_lsid("Filter", &[0, 1])),
                filter_type: Some("Excitation".to_string()),
                ..Default::default()
            },
        ],
        dichroics: vec![crate::common::ome_metadata::OmeDichroic {
            id: Some(crate::common::ome_metadata::create_lsid(
                "Dichroic",
                &[0, 0],
            )),
            model: Some("Fake dichroic".to_string()),
            ..Default::default()
        }],
        ..Default::default()
    }
}

fn fake_populate_spw(ome: &mut crate::common::ome_metadata::OmeMetadata, meta: &ImageMetadata) {
    if !fake_has_spw(meta) {
        return;
    }
    let screens = fake_spw_count(meta, "screens", 0);
    let screen_count = screens.max(1);
    let plates = fake_spw_count(meta, "plates", 1);
    let rows = fake_spw_count(meta, "plateRows", 1);
    let cols = fake_spw_count(meta, "plateCols", 1);
    let fields = fake_spw_count(meta, "fields", 1);
    let acqs = fake_spw_count(meta, "plateAcqs", 1);

    if screens > 0 {
        for s in 0..screens {
            ome.screens.push(crate::common::ome_metadata::OmeScreen {
                id: Some(crate::common::ome_metadata::create_lsid(
                    "Screen",
                    &[s as usize],
                )),
                name: Some(format!("Screen {s}")),
                ..Default::default()
            });
        }
    }

    for screen in 0..screen_count {
        for plate in 0..plates {
            let plate_index = (screen * plates + plate) as usize;
            let mut wells = Vec::new();
            for row in 0..rows {
                for col in 0..cols {
                    let mut well_samples = Vec::new();
                    for acq in 0..acqs {
                        for field in 0..fields {
                            let image_index =
                                (((screen * plates + plate) * rows * cols + row * cols + col)
                                    * fields
                                    * acqs
                                    + acq * fields
                                    + field) as usize;
                            well_samples.push(crate::common::ome_metadata::OmeWellSample {
                                id: Some(crate::common::ome_metadata::create_lsid(
                                    "WellSample",
                                    &[plate_index, (row * cols + col) as usize, well_samples.len()],
                                )),
                                index: well_samples.len() as u32,
                                image_ref: (image_index < ome.images.len()).then_some(image_index),
                                position_x: Some(field as f64),
                                position_y: Some(acq as f64),
                            });
                        }
                    }
                    wells.push(crate::common::ome_metadata::OmeWell {
                        id: Some(crate::common::ome_metadata::create_lsid(
                            "Well",
                            &[plate_index, wells.len()],
                        )),
                        row,
                        column: col,
                        well_samples,
                    });
                }
            }
            ome.plates.push(crate::common::ome_metadata::OmePlate {
                id: Some(crate::common::ome_metadata::create_lsid(
                    "Plate",
                    &[plate_index],
                )),
                name: Some(format!("Plate {plate_index}")),
                rows,
                columns: cols,
                wells,
            });
        }
    }
}

fn fake_roi_x(meta: &ImageMetadata, i: i32) -> f64 {
    ((BOX_SIZE as i32 * i) % meta.size_x as i32) as f64
}

fn fake_roi_y(meta: &ImageMetadata, i: i32) -> f64 {
    ((BOX_SIZE as i32 * ((BOX_SIZE as i32 * i) / meta.size_x as i32)) % meta.size_y as i32) as f64
}

fn fake_roi_points(meta: &ImageMetadata, i: i32) -> Vec<(f64, f64)> {
    let x0 = fake_roi_x(meta, i) + BOX_SIZE as f64 / 2.0;
    let y0 = fake_roi_y(meta, i) + BOX_SIZE as f64 / 2.0;
    let dx = [-0.8, -0.3, 0.4, 0.5, -0.1];
    let dy = [-0.4, 0.6, 0.5, -0.3, -0.7];
    dx.iter()
        .zip(dy.iter())
        .map(|(dx, dy)| {
            (
                x0 + BOX_SIZE as f64 / 2.0 * dx,
                y0 + BOX_SIZE as f64 / 2.0 * dy,
            )
        })
        .collect()
}

fn fake_add_roi(
    rois: &mut Vec<crate::common::ome_metadata::OmeROI>,
    shape: crate::common::ome_metadata::OmeShape,
) {
    let index = rois.len();
    rois.push(crate::common::ome_metadata::OmeROI {
        id: Some(crate::common::ome_metadata::create_lsid("ROI", &[index])),
        shapes: vec![shape],
        ..Default::default()
    });
}

fn fake_populate_rois(ome: &mut crate::common::ome_metadata::OmeMetadata, meta: &ImageMetadata) {
    use crate::common::ome_metadata::OmeShape;
    let half = BOX_SIZE as f64 / 2.0;
    let quarter = BOX_SIZE as f64 / 4.0;

    for i in 0..fake_meta_i64(meta, "ellipses").unwrap_or(0).max(0) as i32 {
        fake_add_roi(
            &mut ome.rois,
            OmeShape::Ellipse {
                x: fake_roi_x(meta, i) + half,
                y: fake_roi_y(meta, i) + half,
                radius_x: half,
                radius_y: half,
                the_z: None,
                the_t: None,
                the_c: None,
            },
        );
    }
    for i in 0..fake_meta_i64(meta, "lines").unwrap_or(0).max(0) as i32 {
        fake_add_roi(
            &mut ome.rois,
            OmeShape::Line {
                x1: fake_roi_x(meta, i) + quarter,
                y1: fake_roi_y(meta, i) + quarter,
                x2: fake_roi_x(meta, i) + half,
                y2: fake_roi_y(meta, i) + half,
                the_z: None,
                the_t: None,
                the_c: None,
            },
        );
    }
    for i in 0..fake_meta_i64(meta, "points").unwrap_or(0).max(0) as i32 {
        fake_add_roi(
            &mut ome.rois,
            OmeShape::Point {
                x: fake_roi_x(meta, i) + half,
                y: fake_roi_y(meta, i) + half,
                the_z: None,
                the_t: None,
                the_c: None,
            },
        );
    }
    for i in 0..fake_meta_i64(meta, "polygons").unwrap_or(0).max(0) as i32 {
        fake_add_roi(
            &mut ome.rois,
            OmeShape::Polygon {
                points: fake_roi_points(meta, i),
                the_z: None,
                the_t: None,
                the_c: None,
            },
        );
    }
    for i in 0..fake_meta_i64(meta, "polylines").unwrap_or(0).max(0) as i32 {
        fake_add_roi(
            &mut ome.rois,
            OmeShape::Polyline {
                points: fake_roi_points(meta, i),
                the_z: None,
                the_t: None,
                the_c: None,
            },
        );
    }
    for i in 0..fake_meta_i64(meta, "rectangles").unwrap_or(0).max(0) as i32 {
        fake_add_roi(
            &mut ome.rois,
            OmeShape::Rectangle {
                x: fake_roi_x(meta, i) + quarter,
                y: fake_roi_y(meta, i) + quarter,
                width: half,
                height: half,
                the_z: None,
                the_t: None,
                the_c: None,
            },
        );
    }
}

fn fake_populate_annotations(
    ome: &mut crate::common::ome_metadata::OmeMetadata,
    meta: &ImageMetadata,
) {
    use crate::common::ome_metadata::OmeAnnotation;
    const NS: &str = "openmicroscopy.org/bioformats/fake";
    let mut annotation_count = 0usize;

    for _ in 0..fake_meta_i64(meta, "annBool").unwrap_or(0).max(0) {
        let id = crate::common::ome_metadata::create_lsid("Annotation:Map", &[annotation_count]);
        ome.annotations.push(OmeAnnotation::MapAnnotation {
            id: Some(id),
            namespace: Some(NS.to_string()),
            values: vec![("Boolean".to_string(), "true".to_string())],
        });
        annotation_count += 1;
    }
    for _ in 0..fake_meta_i64(meta, "annComment").unwrap_or(0).max(0) {
        let id =
            crate::common::ome_metadata::create_lsid("Annotation:Comment", &[annotation_count]);
        ome.annotations.push(OmeAnnotation::CommentAnnotation {
            id: Some(id),
            namespace: Some(NS.to_string()),
            value: format!("Comment:{}", annotation_count + 1),
        });
        annotation_count += 1;
    }
    for _ in 0..fake_meta_i64(meta, "annDouble").unwrap_or(0).max(0) {
        let id = crate::common::ome_metadata::create_lsid("Annotation:Map", &[annotation_count]);
        ome.annotations.push(OmeAnnotation::MapAnnotation {
            id: Some(id),
            namespace: Some(NS.to_string()),
            values: vec![(
                "Double".to_string(),
                (0.111 * (annotation_count + 1) as f64).to_string(),
            )],
        });
        annotation_count += 1;
    }
    for _ in 0..fake_meta_i64(meta, "annLong").unwrap_or(0).max(0) {
        let id = crate::common::ome_metadata::create_lsid("Annotation:Map", &[annotation_count]);
        ome.annotations.push(OmeAnnotation::MapAnnotation {
            id: Some(id),
            namespace: Some(NS.to_string()),
            values: vec![(
                "Long".to_string(),
                (365 + annotation_count as i64).to_string(),
            )],
        });
        annotation_count += 1;
    }
    for _ in 0..fake_meta_i64(meta, "annMap").unwrap_or(0).max(0) {
        let id = crate::common::ome_metadata::create_lsid("Annotation:Map", &[annotation_count]);
        let values = (0..10)
            .map(|key_num| {
                (
                    format!("keyS0N{key_num}"),
                    format!("val{}", (key_num + 1) * (annotation_count + 1)),
                )
            })
            .collect();
        ome.annotations.push(OmeAnnotation::MapAnnotation {
            id: Some(id),
            namespace: Some(NS.to_string()),
            values,
        });
        annotation_count += 1;
    }
    for _ in 0..fake_meta_i64(meta, "annTag").unwrap_or(0).max(0) {
        let id = crate::common::ome_metadata::create_lsid("Annotation:Tag", &[annotation_count]);
        ome.annotations.push(OmeAnnotation::TagAnnotation {
            id: Some(id),
            namespace: Some(NS.to_string()),
            value: format!("Tag:{}", annotation_count + 1),
        });
        annotation_count += 1;
    }
    for key in ["annTerm", "annTime", "annXml"] {
        for _ in 0..fake_meta_i64(meta, key).unwrap_or(0).max(0) {
            let value = match key {
                "annTerm" => format!("Term:{}", annotation_count + 1),
                "annTime" => "1970-01-01T00:00:00".to_string(),
                _ => format!("<dummyXml>{}</dummyXml>", annotation_count + 1),
            };
            let id =
                crate::common::ome_metadata::create_lsid("Annotation:Map", &[annotation_count]);
            ome.annotations.push(OmeAnnotation::MapAnnotation {
                id: Some(id),
                namespace: Some(NS.to_string()),
                values: vec![(key.to_string(), value)],
            });
            annotation_count += 1;
        }
    }

    let labels = fake_meta_i64(meta, "labels").unwrap_or(0).max(0);
    let masks = fake_meta_i64(meta, "masks").unwrap_or(0).max(0);
    if labels > 0 || masks > 0 || fake_meta_bool(meta, "withMicrobeam") {
        let mut values = Vec::new();
        if labels > 0 {
            values.push(("unrepresented_labels".to_string(), labels.to_string()));
        }
        if masks > 0 {
            values.push(("unrepresented_masks".to_string(), masks.to_string()));
        }
        if fake_meta_bool(meta, "withMicrobeam") {
            values.push((
                "microbeam".to_string(),
                "present in Java XMLMockObjects".to_string(),
            ));
        }
        let id = crate::common::ome_metadata::create_lsid("Annotation:Map", &[annotation_count]);
        ome.annotations.push(OmeAnnotation::MapAnnotation {
            id: Some(id),
            namespace: Some(NS.to_string()),
            values,
        });
    }
}

impl FormatReader for FakeReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.to_ascii_lowercase());
        name.as_deref()
            .is_some_and(|name| name.ends_with(".fake") || name.ends_with(".fake.ini"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.series = init_file(path)?;
        self.current_series = 0;
        self.current_resolution = 0;
        self.resolution_meta = None;
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.series = Vec::new();
        self.current_series = 0;
        self.current_resolution = 0;
        self.resolution_meta = None;
        Ok(())
    }

    fn series_count(&self) -> usize {
        self.series.len().max(1)
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if s >= self.series.len() {
            Err(BioFormatsError::SeriesOutOfRange(s))
        } else {
            self.current_series = s;
            self.current_resolution = 0;
            self.resolution_meta = None;
            Ok(())
        }
    }

    fn series(&self) -> usize {
        self.current_series
    }

    fn metadata(&self) -> &ImageMetadata {
        self.resolution_meta.as_ref().unwrap_or_else(|| {
            self.series
                .get(self.current_series)
                .unwrap_or_else(|| crate::common::reader::uninitialized_metadata())
        })
    }

    fn resolution_count(&self) -> usize {
        self.series
            .get(self.current_series)
            .map(|meta| meta.resolution_count.max(1) as usize)
            .unwrap_or(1)
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        let base = self
            .series
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        if level >= base.resolution_count.max(1) as usize {
            return Err(BioFormatsError::PlaneOutOfRange(level as u32));
        }
        self.current_resolution = level;
        self.resolution_meta = (level > 0).then(|| fake_resolution_metadata(base, level));
        Ok(())
    }

    fn resolution(&self) -> usize {
        self.current_resolution
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        if self.series.get(self.current_series).is_none() {
            return Err(BioFormatsError::NotInitialized);
        }
        let meta = self.metadata();
        fake_plane_region(
            meta,
            self.current_series,
            plane_index,
            0,
            0,
            meta.size_x,
            meta.size_y,
        )
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        if self.series.get(self.current_series).is_none() {
            return Err(BioFormatsError::NotInitialized);
        }
        let meta = self.metadata();
        fake_plane_region(meta, self.current_series, plane_index, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        if self.series.get(self.current_series).is_none() {
            return Err(BioFormatsError::NotInitialized);
        }
        let meta = self.metadata();
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        if self.series.is_empty() {
            return None;
        }

        let mut ome = crate::common::ome_metadata::OmeMetadata::default();
        ome.images = self
            .series
            .iter()
            .enumerate()
            .map(|(image_index, meta)| fake_ome_image(meta, image_index))
            .collect();

        let first = &self.series[0];
        if fake_meta_bool(first, "withInstrument")
            || fake_has_spw(first)
            || fake_meta_bool(first, "withMicrobeam")
        {
            ome.instruments.push(fake_instrument());
        }
        fake_populate_spw(&mut ome, first);
        fake_populate_rois(&mut ome, first);
        fake_populate_annotations(&mut ome, first);

        for (image_index, meta) in self.series.iter().enumerate() {
            let _ = ome.add_original_metadata_annotations(meta, image_index);
        }

        Some(ome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta_for(name: &str) -> Vec<ImageMetadata> {
        init_file(Path::new(name)).unwrap()
    }

    #[test]
    fn defaults_match_java() {
        let m = &meta_for("name.fake")[0];
        assert_eq!(m.size_x, 512);
        assert_eq!(m.size_y, 512);
        assert_eq!(m.size_z, 1);
        assert_eq!(m.size_c, 1);
        assert_eq!(m.size_t, 1);
        assert_eq!(m.pixel_type, PixelType::Uint8);
        assert_eq!(m.dimension_order, DimensionOrder::XYZCT);
        assert!(!m.is_rgb);
        assert!(m.is_little_endian);
        assert_eq!(m.bits_per_pixel, 8);
    }

    #[test]
    fn basic_sizes_and_pixel_type() {
        let m = &meta_for("img&sizeX=64&sizeY=32&sizeZ=3&sizeC=2&sizeT=4&pixelType=uint16.fake")[0];
        assert_eq!(m.size_x, 64);
        assert_eq!(m.size_y, 32);
        assert_eq!(m.size_z, 3);
        assert_eq!(m.size_c, 2);
        assert_eq!(m.size_t, 4);
        assert_eq!(m.pixel_type, PixelType::Uint16);
        assert_eq!(m.bits_per_pixel, 16);
        // image_count = sizeZ * effSizeC * sizeT = 3 * 2 * 4
        assert_eq!(m.image_count, 24);
    }

    #[test]
    fn rgb_forces_interleaved_layout_and_dim_order() {
        // rgb=3, sizeC=3 -> effective C = 1, is_rgb true, default RGB dim order
        let m = &meta_for("rgb&sizeC=3&rgb=3&interleaved=true.fake")[0];
        assert!(m.is_rgb);
        assert!(m.is_interleaved);
        assert_eq!(m.dimension_order, DimensionOrder::XYCZT);
        // effective C = sizeC/rgb = 1, so image_count = 1*1*1
        assert_eq!(m.image_count, 1);
    }

    #[test]
    fn rgb_corrects_bad_dim_order() {
        // dimOrder not starting with XYC, rgb>1: Z before T -> XYCZT
        let m = &meta_for("rgb&sizeC=3&rgb=3&dimOrder=XYZCT.fake")[0];
        assert_eq!(m.dimension_order, DimensionOrder::XYCZT);
        // T before Z -> XYCTZ
        let m2 = &meta_for("rgb&sizeC=3&rgb=3&dimOrder=XYTZC.fake")[0];
        assert_eq!(m2.dimension_order, DimensionOrder::XYCTZ);
    }

    #[test]
    fn explicit_dim_order_and_little_endian() {
        let m = &meta_for("img&dimOrder=XYZTC&little=false.fake")[0];
        assert_eq!(m.dimension_order, DimensionOrder::XYZTC);
        assert!(!m.is_little_endian);
    }

    #[test]
    fn indexed_and_bits_per_pixel() {
        let m = &meta_for("img&indexed=true&bitsPerPixel=12&pixelType=uint16.fake")[0];
        assert!(m.is_indexed);
        assert_eq!(m.bits_per_pixel, 12);
    }

    #[test]
    fn thumb_size_recorded() {
        let m = &meta_for("img&thumbSizeX=16&thumbSizeY=8.fake")[0];
        assert!(matches!(
            m.series_metadata.get("thumbSizeX"),
            Some(MetadataValue::Int(16))
        ));
        assert!(matches!(
            m.series_metadata.get("thumbSizeY"),
            Some(MetadataValue::Int(8))
        ));
    }

    #[test]
    fn multi_series() {
        let series = meta_for("multi&series=3&sizeX=4&sizeY=4.fake");
        assert_eq!(series.len(), 3);
        assert!(matches!(
            series[0].series_metadata.get("Image name"),
            Some(MetadataValue::String(s)) if s == "multi"
        ));
        assert!(matches!(
            series[1].series_metadata.get("Image name"),
            Some(MetadataValue::String(s)) if s == "multi 2"
        ));
    }

    #[test]
    fn resolutions_recorded() {
        let m = &meta_for("pyr&sizeX=1000&sizeY=1000&resolutions=4&resolutionScale=2.fake")[0];
        assert_eq!(m.resolution_count, 4);
    }

    #[test]
    fn set_resolution_scales_fake_geometry_like_java() {
        let mut reader = FakeReader::new();
        reader
            .set_id(Path::new(
                "pyr&sizeX=1000&sizeY=800&resolutions=4&resolutionScale=2.fake",
            ))
            .unwrap();

        assert_eq!(reader.resolution_count(), 4);
        assert_eq!(reader.resolution(), 0);
        assert_eq!(
            (reader.metadata().size_x, reader.metadata().size_y),
            (1000, 800)
        );

        reader.set_resolution(2).unwrap();
        assert_eq!(reader.resolution(), 2);
        assert_eq!(
            (reader.metadata().size_x, reader.metadata().size_y),
            (250, 200)
        );
        assert_eq!(reader.open_bytes(0).unwrap().len(), 250 * 200);

        reader.set_resolution(0).unwrap();
        assert_eq!(reader.resolution(), 0);
        assert_eq!(
            (reader.metadata().size_x, reader.metadata().size_y),
            (1000, 800)
        );
        assert!(matches!(
            reader.set_resolution(4),
            Err(BioFormatsError::PlaneOutOfRange(4))
        ));
    }

    #[test]
    fn false_color_requires_indexed() {
        let err = init_file(Path::new("img&falseColor=true.fake"));
        assert!(err.is_err());
        let ok = init_file(Path::new("img&falseColor=true&indexed=true.fake"));
        assert!(ok.is_ok());
    }

    #[test]
    fn invalid_rgb_combination_rejected() {
        // rgb does not divide sizeC
        assert!(init_file(Path::new("img&sizeC=3&rgb=2.fake")).is_err());
    }

    #[test]
    fn invalid_resolution_scale_rejected() {
        assert!(init_file(Path::new("img&resolutionScale=1.fake")).is_err());
    }

    #[test]
    fn physical_sizes_and_color() {
        let m = &meta_for("img&physicalSizeX=0.5&physicalSizeY=0.5&color=0xff0000ff.fake")[0];
        assert!(matches!(
            m.series_metadata.get("physicalSizeX"),
            Some(MetadataValue::Float(v)) if (*v - 0.5).abs() < 1e-9
        ));
        // 0xff0000ff parsed as long then cast to i32
        assert!(matches!(
            m.series_metadata.get("color"),
            Some(MetadataValue::Int(_))
        ));
    }

    #[test]
    fn per_channel_color_indexed_keys() {
        let m = &meta_for("img&sizeC=2&color_0=0x00ff00ff&color_1=0x0000ffff.fake")[0];
        assert!(m.series_metadata.contains_key("color_0"));
        assert!(m.series_metadata.contains_key("color_1"));
    }

    #[test]
    fn ome_metadata_projects_filename_channel_wavelengths_like_java() {
        let mut reader = FakeReader::new();
        reader
            .set_id(Path::new(
                "img&sizeC=2&emission_0=520&emission_1=610&excitation_1=488.fake",
            ))
            .unwrap();

        let ome = reader.ome_metadata().unwrap();
        let channels = &ome.images[0].channels;
        assert_eq!(channels.len(), 2);
        assert_eq!(channels[0].emission_wavelength, Some(520.0));
        assert_eq!(channels[1].emission_wavelength, Some(610.0));
        assert_eq!(channels[0].excitation_wavelength, None);
        assert_eq!(channels[1].excitation_wavelength, Some(488.0));
    }

    #[test]
    fn fake_ini_projects_series_channel_metadata_like_java() {
        let dir =
            std::env::temp_dir().join(format!("bioformats_rs_fake_ini_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("img.fake.ini");
        std::fs::write(
            &path,
            "sizeC=2\n[series_0]\nChannelName_0=DAPI\nChannelName_1=FITC\nChannelEmissionWavelength_0=461\nChannelExcitationWavelength_1=488\n",
        )
        .unwrap();

        let mut reader = FakeReader::new();
        reader.set_id(&path).unwrap();

        let ome = reader.ome_metadata().unwrap();
        let channels = &ome.images[0].channels;
        assert_eq!(channels.len(), 2);
        assert_eq!(channels[0].name.as_deref(), Some("DAPI"));
        assert_eq!(channels[1].name.as_deref(), Some("FITC"));
        assert_eq!(channels[0].emission_wavelength, Some(461.0));
        assert_eq!(channels[1].excitation_wavelength, Some(488.0));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn spw_overrides_series_count() {
        // plates=1, rows=2, cols=3, fields=1, acqs=1, screens=0 -> 1*2*3*1*1 = 6 images
        let series = meta_for("SPW&plates=1&plateRows=2&plateCols=3&fields=1&plateAcqs=1.fake");
        assert_eq!(series.len(), 6);
    }

    #[test]
    fn annotations_and_rois_recorded() {
        let m = &meta_for("regions&points=10&ellipses=5&annLong=2.fake")[0];
        assert!(matches!(
            m.series_metadata.get("points"),
            Some(MetadataValue::Int(10))
        ));
        assert!(matches!(
            m.series_metadata.get("ellipses"),
            Some(MetadataValue::Int(5))
        ));
        assert!(matches!(
            m.series_metadata.get("annLong"),
            Some(MetadataValue::Int(2))
        ));
    }

    #[test]
    fn ome_metadata_projects_fake_spw_instrument_rois_and_annotations() {
        let mut reader = FakeReader::new();
        reader
            .set_id(Path::new(
                "SPW&sizeX=64&sizeY=32&sizeC=2&plates=1&plateRows=2&plateCols=2&fields=2&plateAcqs=1&withInstrument=true&points=1&rectangles=1&annComment=1&annMap=1.fake",
            ))
            .unwrap();

        let ome = reader.ome_metadata().unwrap();

        assert_eq!(ome.images.len(), 8);
        assert_eq!(ome.plates.len(), 1);
        assert_eq!(ome.plates[0].rows, 2);
        assert_eq!(ome.plates[0].columns, 2);
        assert_eq!(ome.plates[0].wells.len(), 4);
        assert_eq!(ome.plates[0].wells[0].well_samples.len(), 2);
        assert_eq!(ome.plates[0].wells[0].well_samples[0].image_ref, Some(0));
        assert_eq!(ome.plates[0].wells[0].well_samples[1].image_ref, Some(1));

        assert_eq!(ome.instruments.len(), 1);
        assert_eq!(ome.images[0].instrument_ref, Some(0));
        assert_eq!(ome.images[0].objective_ref, Some(0));
        assert_eq!(
            ome.images[0].channels[0].detector_ref.as_deref(),
            Some("Detector:0:0")
        );
        assert_eq!(
            ome.images[1].channels[1]
                .light_source_settings_id
                .as_deref(),
            Some("LightSource:0:1")
        );
        assert_eq!(
            ome.images[0].light_paths[0].emission_filter_ids[0],
            "Filter:0:0"
        );

        assert_eq!(ome.rois.len(), 2);
        assert!(matches!(
            ome.rois[0].shapes[0],
            crate::common::ome_metadata::OmeShape::Point { x: 5.0, y: 5.0, .. }
        ));
        assert!(matches!(
            ome.rois[1].shapes[0],
            crate::common::ome_metadata::OmeShape::Rectangle {
                x: 2.5,
                y: 2.5,
                width: 5.0,
                height: 5.0,
                ..
            }
        ));

        assert!(ome.annotations.iter().any(|ann| matches!(
            ann,
            crate::common::ome_metadata::OmeAnnotation::CommentAnnotation { value, .. }
                if value == "Comment:1"
        )));
        assert!(ome.annotations.iter().any(|ann| matches!(
            ann,
            crate::common::ome_metadata::OmeAnnotation::MapAnnotation { values, .. }
                if values.iter().any(|(k, v)| k == "keyS0N0" && v == "val2")
        )));
    }

    #[test]
    fn ome_metadata_records_unrepresented_fake_label_mask_microbeam_fields() {
        let mut reader = FakeReader::new();
        reader
            .set_id(Path::new(
                "SPW&sizeX=64&sizeY=32&labels=2&masks=1&withMicrobeam=true.fake",
            ))
            .unwrap();

        let ome = reader.ome_metadata().unwrap();
        assert!(ome.annotations.iter().any(|ann| matches!(
            ann,
            crate::common::ome_metadata::OmeAnnotation::MapAnnotation { values, .. }
                if values.iter().any(|(k, v)| k == "unrepresented_labels" && v == "2")
                    && values.iter().any(|(k, v)| k == "unrepresented_masks" && v == "1")
                    && values.iter().any(|(k, _)| k == "microbeam")
        )));
    }

    #[test]
    fn unknown_pixel_type_errors() {
        assert!(init_file(Path::new("img&pixelType=bogus.fake")).is_err());
    }

    fn u16_le(bytes: &[u8], offset: usize) -> u16 {
        u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
    }

    #[test]
    fn open_bytes_encodes_java_special_pixels() {
        let path = Path::new("img&sizeX=60&sizeY=12&sizeZ=2&sizeC=3&sizeT=2.fake");
        let mut reader = FakeReader::new();
        reader.set_id(path).unwrap();

        // XYZCT order: plane 5 => z=1, c=2, t=0.
        let plane = reader.open_bytes(5).unwrap();
        assert_eq!(plane[0], 0); // series
        assert_eq!(plane[BOX_SIZE as usize], 5); // plane number
        assert_eq!(plane[(2 * BOX_SIZE) as usize], 1); // Z
        assert_eq!(plane[(3 * BOX_SIZE) as usize], 2); // C
        assert_eq!(plane[(4 * BOX_SIZE) as usize], 0); // T
        assert_eq!(plane[(5 * BOX_SIZE) as usize], 50); // normal gradient
    }

    #[test]
    fn open_bytes_uses_signed_minimum_like_java() {
        let path = Path::new("img&sizeX=60&sizeY=12&pixelType=int16.fake");
        let mut reader = FakeReader::new();
        reader.set_id(path).unwrap();

        let plane = reader.open_bytes(0).unwrap();
        assert_eq!(u16_le(&plane, 0), 0);
        let off = 50 * 2;
        assert_eq!(i16::from_le_bytes([plane[off], plane[off + 1]]), -32718);
    }

    #[test]
    fn open_bytes_scales_normal_pixels_but_not_special_pixels() {
        let path = Path::new("img&sizeX=60&sizeY=12&pixelType=uint16&scaleFactor=2.fake");
        let mut reader = FakeReader::new();
        reader.set_id(path).unwrap();

        let plane = reader.open_bytes(0).unwrap();
        assert_eq!(u16_le(&plane, 0), 0);
        assert_eq!(u16_le(&plane, 50 * 2), 100);
    }

    #[test]
    fn rgb_planes_include_all_samples_and_honor_planar_layout() {
        let path =
            Path::new("rgb&sizeX=60&sizeY=12&sizeC=6&rgb=3&interleaved=false&dimOrder=XYCZT.fake");
        let mut reader = FakeReader::new();
        reader.set_id(path).unwrap();

        let plane = reader.open_bytes(1).unwrap();
        let samples_per_channel = 60 * 12;
        assert_eq!(plane.len(), samples_per_channel * 3);
        // XYCZT order with sizeC/rgb=2: plane 1 is effective channel 1.
        assert_eq!(plane[3 * BOX_SIZE as usize], 3);
        assert_eq!(plane[samples_per_channel + 3 * BOX_SIZE as usize], 4);
        assert_eq!(plane[2 * samples_per_channel + 3 * BOX_SIZE as usize], 5);
    }

    #[test]
    fn rgb_region_generation_matches_java_region_layout() {
        let path = Path::new("rgb&sizeX=60&sizeY=12&sizeC=3&rgb=3&interleaved=true.fake");
        let mut reader = FakeReader::new();
        reader.set_id(path).unwrap();

        let region = reader.open_bytes_region(0, 29, BOX_SIZE, 3, 1).unwrap();
        assert_eq!(region, vec![29, 29, 29, 30, 30, 30, 31, 31, 31]);
    }
}
