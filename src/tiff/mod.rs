mod compression;
pub mod ifd;
pub(crate) mod jpeg_restart;
pub(crate) mod nikon;
pub mod parser;
mod reader;
mod writer;

pub use reader::TiffReader;
pub use writer::{PyramidOmeTiffWriter, TiffWriter, WriteCompression};
// In-place TIFF metadata overwrite (port of Java TiffSaver.overwrite*).
pub use writer::{
    make_valid_ifd, overwrite_comment, overwrite_ifd_value, overwrite_last_ifd_offset,
    read_first_comment, TiffSaverValue,
};
