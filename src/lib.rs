//! `bioformats` — pure-Rust reader/writer for scientific image formats.
//!
//! # Quick start — reading
//!
//! ```no_run
//! use bioformats::ImageReader;
//! use std::path::Path;
//!
//! let mut reader = ImageReader::open(Path::new("image.tif")).unwrap();
//! let meta = reader.metadata();
//! println!("{}x{}", meta.size_x, meta.size_y);
//! let plane0 = reader.open_bytes(0).unwrap();
//! ```
//!
//! # Quick start — writing
//!
//! ```no_run
//! use bioformats::{ImageWriter, ImageMetadata, PixelType};
//! use std::path::Path;
//!
//! let mut meta = ImageMetadata::default();
//! meta.size_x = 512; meta.size_y = 512;
//! meta.pixel_type = PixelType::Uint16;
//! meta.image_count = 1;
//!
//! let data = vec![0u8; 512 * 512 * 2]; // 16-bit zeros
//! ImageWriter::save(Path::new("out.tif"), &meta, &[data]).unwrap();
//! ```

pub mod common;
pub mod tiff;
pub mod error;
pub mod formats;
pub mod metadata;
pub mod pixel;
pub mod reader;
pub mod registry;
pub mod memoizer;
pub mod wrappers;
pub mod writer_registry;

pub use error::{BioFormatsError, Result};
pub use metadata::{DimensionOrder, ImageMetadata, LookupTable, MetadataValue};
pub use pixel::PixelType;
pub use reader::FormatReader;
pub use registry::ImageReader;
pub use writer_registry::ImageWriter;
pub use crate::common::ome_metadata::{
    OmeChannel, OmeImage, OmeMetadata, OmePlane,
    OmeInstrument, OmeObjective, OmeDetector, OmeLightSource,
    OmeFilter, OmeDichroic, OmeLightPath,
    OmeROI, OmeShape, OmeExperimenter, OmeAnnotation,
};
pub use crate::common::writer::FormatWriter;
pub use crate::tiff::{TiffWriter, WriteCompression};
pub use crate::wrappers::{ChannelSeparator, ChannelMerger, DimensionSwapper};
pub use crate::memoizer::Memoizer;
