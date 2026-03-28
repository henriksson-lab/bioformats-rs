use std::collections::HashMap;
use super::pixel_type::PixelType;

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
        }
    }
}
