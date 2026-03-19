//! PerkinElmer FLEX HCS format reader.
//!
//! FLEX is a TIFF-based format used for high-content screening (HCS) by PerkinElmer.
//! This reader wraps TiffReader and adds .flex extension recognition.

use std::path::Path;
use bioformats_common::error::Result;
use bioformats_common::metadata::ImageMetadata;
use bioformats_common::reader::FormatReader;

pub struct FlexReader {
    inner: bioformats_tiff::TiffReader,
}

impl FlexReader {
    pub fn new() -> Self { FlexReader { inner: bioformats_tiff::TiffReader::new() } }
}
impl Default for FlexReader { fn default() -> Self { Self::new() } }

impl FormatReader for FlexReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension().and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("flex"))
            .unwrap_or(false)
    }
    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // FLEX files are TIFF — check TIFF magic
        if header.len() < 4 { return false; }
        (header[0] == 0x49 && header[1] == 0x49 && header[2] == 0x2A && header[3] == 0x00)
        || (header[0] == 0x4D && header[1] == 0x4D && header[2] == 0x00 && header[3] == 0x2A)
        // BigTIFF
        || (header[0] == 0x49 && header[1] == 0x49 && header[2] == 0x2B && header[3] == 0x00)
    }
    fn set_id(&mut self, path: &Path) -> Result<()> { self.inner.set_id(path) }
    fn close(&mut self) -> Result<()> { self.inner.close() }
    fn series_count(&self) -> usize { self.inner.series_count() }
    fn set_series(&mut self, s: usize) -> Result<()> { self.inner.set_series(s) }
    fn series(&self) -> usize { self.inner.series() }
    fn metadata(&self) -> &ImageMetadata { self.inner.metadata() }
    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> { self.inner.open_bytes(p) }
    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        self.inner.open_bytes_region(p, x, y, w, h)
    }
    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> { self.inner.open_thumb_bytes(p) }
    fn resolution_count(&self) -> usize { self.inner.resolution_count() }
    fn set_resolution(&mut self, l: usize) -> Result<()> { self.inner.set_resolution(l) }
    fn resolution(&self) -> usize { self.inner.resolution() }
}
