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

pub mod cache;
pub mod common;
pub mod error;
pub mod formats;
pub mod memoizer;
pub mod metadata;
mod metakit;
pub mod pixel;
pub mod reader;
mod reader_order;
pub mod registry;
pub mod stitcher;
pub mod tiff;
pub mod wrappers;
mod writer_order;
pub mod writer_registry;

pub use crate::cache::{CacheStrategy, CachedReader};
pub use crate::common::compressed::{
    CompressedBytes, CompressedExtractionConstraint, CompressedExtractionSupport,
    CompressedFileRange, CompressedLevelInfo, CompressedTile, CompressedTileMode,
    Jpeg2000Container, JpegColorSpace, JpegSubsampling, LossyCodec,
};
pub use crate::common::ome_metadata::{
    create_lsid, OmeAnnotation, OmeChannel, OmeDataset, OmeDetector, OmeDichroic, OmeExperiment,
    OmeExperimenter, OmeFilter, OmeImage, OmeInstrument, OmeLightPath, OmeLightSource, OmeMetadata,
    OmeObjective, OmePlane, OmePlate, OmeROI, OmeScreen, OmeShape, OmeWell, OmeWellSample,
};
pub use crate::common::writer::FormatWriter;
pub use crate::memoizer::Memoizer;
pub use crate::stitcher::{AxisGuesser, AxisType, FilePattern, FilePatternBlock, FileStitcher};
pub use crate::tiff::{TiffWriter, WriteCompression};
pub use crate::wrappers::{
    ChannelFiller, ChannelMerger, ChannelSeparator, DimensionSwapper, MinMaxCalculator,
};
pub use error::{BioFormatsError, Result};
pub use metadata::{
    DimensionOrder, ImageMetadata, LookupTable, MetadataLevel, MetadataOptions, MetadataValue,
    ModuloAnnotation,
};
pub use pixel::PixelType;
pub use reader::FormatReader;
pub use registry::ImageReader;
pub use writer_registry::ImageWriter;
