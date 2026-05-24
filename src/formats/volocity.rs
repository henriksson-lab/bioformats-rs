//! Volocity (.mvd2) and Nikon NIS (.nif) format readers.

use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::ImageMetadata;
use crate::common::reader::FormatReader;

// ─── Volocity .mvd2 ───────────────────────────────────────────────────────────
//
// Volocity (PerkinElmer) stores 3D/4D microscopy data in .mvd2 files.
// The format is a proprietary binary container. Without full reverse-engineering,
// extension matches report an explicit unsupported error instead of synthetic
// metadata.

pub struct VolocityReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl VolocityReader {
    pub fn new() -> Self {
        VolocityReader {
            path: None,
            meta: None,
        }
    }
}
impl Default for VolocityReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for VolocityReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("mvd2"))
            .unwrap_or(false)
    }
    fn is_this_type_by_bytes(&self, _: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, _path: &Path) -> Result<()> {
        Err(BioFormatsError::UnsupportedFormat(
            "Volocity MVD2 format reading is not yet implemented".to_string(),
        ))
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        Ok(())
    }
    fn series_count(&self) -> usize {
        1
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if s != 0 {
            Err(BioFormatsError::SeriesOutOfRange(s))
        } else {
            Ok(())
        }
    }
    fn series(&self) -> usize {
        0
    }
    fn metadata(&self) -> &ImageMetadata {
        self.meta.as_ref().expect("set_id not called")
    }
    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        let _ = p;
        Err(BioFormatsError::UnsupportedFormat(
            "Volocity MVD2 format reading is not yet implemented".to_string(),
        ))
    }
    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let _ = (p, x, y, w, h);
        Err(BioFormatsError::UnsupportedFormat(
            "Volocity MVD2 format reading is not yet implemented".to_string(),
        ))
    }
    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        let _ = p;
        Err(BioFormatsError::UnsupportedFormat(
            "Volocity MVD2 format reading is not yet implemented".to_string(),
        ))
    }
}

// ─── Nikon NIS-Elements .nif ──────────────────────────────────────────────────
//
// Nikon NIS-Elements Image File (.nif) — TIFF-based format.
// Delegates to TiffReader for pixel data.

pub struct NikonNisReader {
    inner: crate::tiff::TiffReader,
}

impl NikonNisReader {
    pub fn new() -> Self {
        NikonNisReader {
            inner: crate::tiff::TiffReader::new(),
        }
    }
}
impl Default for NikonNisReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for NikonNisReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("nif") | Some("nd2") )
        // .nd2 is already handled by bioformats-nd2, so effectively only .nif here
        && matches!(ext.as_deref(), Some("nif"))
    }
    fn is_this_type_by_bytes(&self, _: &[u8]) -> bool {
        false
    }
    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.inner.set_id(path)
    }
    fn close(&mut self) -> Result<()> {
        self.inner.close()
    }
    fn series_count(&self) -> usize {
        self.inner.series_count()
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        self.inner.set_series(s)
    }
    fn series(&self) -> usize {
        self.inner.series()
    }
    fn metadata(&self) -> &ImageMetadata {
        self.inner.metadata()
    }
    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        self.inner.open_bytes(p)
    }
    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        self.inner.open_bytes_region(p, x, y, w, h)
    }
    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        self.inner.open_thumb_bytes(p)
    }
    fn resolution_count(&self) -> usize {
        self.inner.resolution_count()
    }
    fn set_resolution(&mut self, l: usize) -> Result<()> {
        self.inner.set_resolution(l)
    }
    fn resolution(&self) -> usize {
        self.inner.resolution()
    }
}
