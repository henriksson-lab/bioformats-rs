use std::collections::HashMap;
use super::pixel_type::PixelType;

/// Controls how much metadata is parsed during `set_id`.
///
/// Equivalent to Java Bio-Formats' `MetadataLevel`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MetadataLevel {
    /// Minimum metadata: dimensions, pixel type, plane count. Fastest.
    Minimal,
    /// All metadata except overlay/ROI data.
    NoOverlays,
    /// Full metadata parsing including ROIs, annotations, etc.
    #[default]
    All,
}

/// Configurable metadata parsing options.
#[derive(Debug, Clone, Default)]
pub struct MetadataOptions {
    /// Controls the depth of metadata parsing.
    pub level: MetadataLevel,
    /// Whether to populate original/proprietary metadata in `series_metadata`.
    pub original_metadata: bool,
}

/// Modulo annotation — encodes sub-dimensions within Z, C, or T.
///
/// Used for 6D+ imaging: e.g., a FLIM acquisition might store lifetime bins
/// as sub-channels within C, or a spectral scan might have wavelength steps.
///
/// Equivalent to Java Bio-Formats' `Modulo` class.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ModuloAnnotation {
    /// Which parent dimension this modulo subdivides: "Z", "C", or "T".
    pub parent_dimension: String,
    /// Type of sub-dimension: "lifetime", "lambda", "angle", "phase", "tile", or custom.
    pub modulo_type: String,
    /// Start value of the sub-dimension range.
    pub start: f64,
    /// Step size between consecutive sub-dimension values.
    pub step: f64,
    /// End value of the sub-dimension range.
    pub end: f64,
    /// Unit of the sub-dimension values (e.g., "nm", "ps", "degree").
    pub unit: String,
    /// Optional labels for each sub-dimension position.
    pub labels: Vec<String>,
}

/// Dimension ordering of the image planes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DimensionOrder {
    XYCTZ,
    XYCZT,
    XYTCZ,
    XYTZC,
    XYZCT,
    XYZTC,
}

impl Default for DimensionOrder {
    fn default() -> Self {
        DimensionOrder::XYCZT
    }
}

/// A typed metadata value.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum MetadataValue {
    String(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Bytes(Vec<u8>),
}

impl std::fmt::Display for MetadataValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MetadataValue::String(s) => write!(f, "{}", s),
            MetadataValue::Int(i) => write!(f, "{}", i),
            MetadataValue::Float(v) => write!(f, "{}", v),
            MetadataValue::Bool(b) => write!(f, "{}", b),
            MetadataValue::Bytes(b) => write!(f, "<{} bytes>", b.len()),
        }
    }
}

/// Optional indexed colour lookup table.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LookupTable {
    pub red: Vec<u16>,
    pub green: Vec<u16>,
    pub blue: Vec<u16>,
}

/// Core metadata for one image series.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ImageMetadata {
    pub size_x: u32,
    pub size_y: u32,
    pub size_z: u32,
    pub size_c: u32,
    pub size_t: u32,
    pub pixel_type: PixelType,
    pub bits_per_pixel: u8,
    pub image_count: u32,
    pub dimension_order: DimensionOrder,
    pub is_rgb: bool,
    pub is_interleaved: bool,
    pub is_indexed: bool,
    pub is_little_endian: bool,
    pub resolution_count: u32,
    pub series_metadata: HashMap<String, MetadataValue>,
    pub lookup_table: Option<LookupTable>,
    /// Modulo annotation for Z dimension (sub-dimensions within Z).
    pub modulo_z: Option<ModuloAnnotation>,
    /// Modulo annotation for C dimension (sub-dimensions within C).
    pub modulo_c: Option<ModuloAnnotation>,
    /// Modulo annotation for T dimension (sub-dimensions within T).
    pub modulo_t: Option<ModuloAnnotation>,
}

impl Default for ImageMetadata {
    fn default() -> Self {
        ImageMetadata {
            size_x: 0,
            size_y: 0,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type: PixelType::Uint8,
            bits_per_pixel: 8,
            image_count: 1,
            dimension_order: DimensionOrder::XYCZT,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: true,
            resolution_count: 1,
            series_metadata: HashMap::new(),
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        }
    }
}
