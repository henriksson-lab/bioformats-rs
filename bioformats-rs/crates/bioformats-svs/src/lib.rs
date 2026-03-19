//! Whole-slide TIFF-based format reader.
//!
//! Wraps TiffReader to add extension recognition for whole-slide image formats:
//! - Aperio SVS (.svs)
//! - Ventana BIF (.bif)
//! - Hamamatsu NDPI (.ndpi)
//! - Leica SCN (.scn)
//! - Olympus VSI (.vsi)
//! - TIFF-based AFI (.afi)

use std::path::Path;

use bioformats_common::error::Result;
use bioformats_common::metadata::ImageMetadata;
use bioformats_common::reader::FormatReader;

pub struct WholeSlideTiffReader {
    inner: bioformats_tiff::TiffReader,
}

impl WholeSlideTiffReader {
    pub fn new() -> Self {
        WholeSlideTiffReader { inner: bioformats_tiff::TiffReader::new() }
    }
}

impl Default for WholeSlideTiffReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for WholeSlideTiffReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension().and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("svs") | Some("bif") | Some("ndpi") | Some("scn") | Some("vsi") | Some("afi"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        // Extension-only; let TiffReader handle magic bytes
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

    fn set_series(&mut self, series: usize) -> Result<()> {
        self.inner.set_series(series)
    }

    fn series(&self) -> usize {
        self.inner.series()
    }

    fn metadata(&self) -> &ImageMetadata {
        self.inner.metadata()
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        self.inner.open_bytes(plane_index)
    }

    fn open_bytes_region(&mut self, plane_index: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        self.inner.open_bytes_region(plane_index, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        self.inner.open_thumb_bytes(plane_index)
    }

    fn resolution_count(&self) -> usize {
        self.inner.resolution_count()
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        self.inner.set_resolution(level)
    }

    fn resolution(&self) -> usize {
        self.inner.resolution()
    }
}
