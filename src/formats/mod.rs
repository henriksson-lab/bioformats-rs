/// Readers/writers ported from upstream Bio-Formats' BSD-2-Clause
/// `formats-bsd` module. Always available.
pub mod bsd;
/// Readers ported from upstream Bio-Formats' GPL-2.0-or-later `formats-gpl`
/// module. Only compiled when the `gpl` cargo feature is enabled (on by
/// default); see README.md.
#[cfg(feature = "gpl")]
pub mod gpl;

pub mod extended;
pub mod flim2;
pub mod java_writer;
pub mod legacy;
pub mod metaimage;
pub mod misc;
pub mod misc4;
pub mod openslide_reader;
pub mod png;
pub mod raster;
pub mod simfcs;
pub(crate) mod stack_writer;
pub mod v3draw;
#[cfg(feature = "zarr")]
pub mod zarr;
