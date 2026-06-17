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
//! Pixel data is a simple gradient (the per-pixel encoding here is not a
//! faithful port of Java's special-pixel scheme; only the metadata parsing
//! is).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;

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

pub struct FakeReader {
    path: Option<PathBuf>,
    /// One [`ImageMetadata`] per series (each carrying its own resolution
    /// count); mirrors Java's `core` list of `CoreMetadata`.
    series: Vec<ImageMetadata>,
    current_series: usize,
}

impl FakeReader {
    pub fn new() -> Self {
        FakeReader {
            path: None,
            series: Vec::new(),
            current_series: 0,
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

        let ms = ImageMetadata {
            size_x: p.size_x,
            size_y: p.size_y,
            size_z: p.size_z,
            size_c: p.size_c,
            size_t: p.size_t,
            pixel_type: p.pixel_type,
            bits_per_pixel,
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
        }
    }
    for (i, w) in p.excitation_wavelengths.iter().enumerate() {
        if let Some(w) = w {
            m.insert(format!("excitation_{}", i), MetadataValue::Float(*w));
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
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();

    let tokens: Vec<&str> = stem.split(TOKEN_SEPARATOR).collect();
    let params = parse_tokens(&tokens)?;
    build_metadata(params)
}

impl FormatReader for FakeReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("fake"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.series = init_file(path)?;
        self.current_series = 0;
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.series = Vec::new();
        self.current_series = 0;
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
            Ok(())
        }
    }

    fn series(&self) -> usize {
        self.current_series
    }

    fn metadata(&self) -> &ImageMetadata {
        self.series
            .get(self.current_series)
            .unwrap_or_else(|| crate::common::reader::uninitialized_metadata())
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
        let w = meta.size_x as usize;
        let h = meta.size_y as usize;
        let mut buf = vec![0u8; w * h * bps];
        let pidx = plane_index as usize;
        for y in 0..h {
            for x in 0..w {
                let val = ((x + y + pidx) % 256) as u8;
                let off = (y * w + x) * bps;
                for b in 0..bps {
                    buf[off + b] = val;
                }
            }
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
        let meta = self
            .series
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        crop_full_plane("Fake", &full, meta, 1, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self
            .series
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
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
    fn unknown_pixel_type_errors() {
        assert!(init_file(Path::new("img&pixelType=bogus.fake")).is_err());
    }
}
