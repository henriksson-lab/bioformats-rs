use crate::error::Result;
use crate::metadata::ImageMetadata;
use std::path::Path;

/// Core trait that every format writer must implement.
///
/// Mirrors `IFormatWriter` from the Java library.
pub trait FormatWriter: Send + Sync {
    /// True if this writer can handle the file path (by extension).
    fn is_this_type(&self, path: &Path) -> bool;

    /// Open the output file and prepare for writing.
    /// Must be called after `set_metadata`.
    fn set_id(&mut self, path: &Path) -> Result<()>;

    /// Flush and close the output file.
    fn close(&mut self) -> Result<()>;

    /// Set the image metadata that describes what will be written.
    /// Must be called before `set_id`.
    fn set_metadata(&mut self, meta: &ImageMetadata) -> Result<()>;

    /// Write raw pixel bytes for one plane (same layout as `FormatReader::open_bytes`).
    fn save_bytes(&mut self, plane_index: u32, data: &[u8]) -> Result<()>;

    /// True if this writer supports multi-plane (Z/C/T stack) files.
    fn can_do_stacks(&self) -> bool {
        true
    }

    // --- Multi-series support (optional) ---
    fn set_series(&mut self, _series: usize) -> Result<()> {
        Ok(())
    }
    fn series(&self) -> usize {
        0
    }
}
