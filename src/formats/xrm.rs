//! Zeiss XRM X-ray tomography format reader.
//!
//! XRM/TXRM files are OLE2-based (Compound Document) format from Zeiss Xradia.
//! Extension-only detection for .xrm and .txrm; returns placeholder metadata.

use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::ImageMetadata;
use crate::common::reader::FormatReader;

pub struct XrmReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl XrmReader {
    pub fn new() -> Self {
        XrmReader { path: None, meta: None }
    }
}

impl Default for XrmReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for XrmReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("xrm") | Some("txrm"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool { false }

    fn set_id(&mut self, _path: &Path) -> Result<()> {
        Err(BioFormatsError::UnsupportedFormat(
            "Zeiss XRM format reading is not yet implemented".to_string()
        ))
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        Ok(())
    }

    fn series_count(&self) -> usize { 1 }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if s != 0 { Err(BioFormatsError::SeriesOutOfRange(s)) } else { Ok(()) }
    }

    fn series(&self) -> usize { 0 }

    fn metadata(&self) -> &ImageMetadata {
        self.meta.as_ref().expect("set_id not called")
    }

    fn open_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "Zeiss XRM format reading is not yet implemented".to_string()
        ))
    }

    fn open_bytes_region(&mut self, _plane_index: u32, _x: u32, _y: u32, _w: u32, _h: u32) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "Zeiss XRM format reading is not yet implemented".to_string()
        ))
    }

    fn open_thumb_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "Zeiss XRM format reading is not yet implemented".to_string()
        ))
    }
}
