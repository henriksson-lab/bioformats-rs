use std::path::Path;
use crate::error::Result;
use crate::common::metadata::{ImageMetadata, MetadataOptions};
use super::ome_metadata::OmeMetadata;

/// Core trait that every format reader must implement.
pub trait FormatReader: Send + Sync {
    fn is_this_type_by_name(&self, path: &Path) -> bool;
    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool;
    fn set_id(&mut self, path: &Path) -> Result<()>;
    fn close(&mut self) -> Result<()>;
    fn series_count(&self) -> usize;
    fn set_series(&mut self, series: usize) -> Result<()>;
    fn series(&self) -> usize;
    fn metadata(&self) -> &ImageMetadata;
    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>>;
    fn open_bytes_region(&mut self, plane_index: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>>;
    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>>;
    fn resolution_count(&self) -> usize { 1 }
    fn set_resolution(&mut self, _level: usize) -> Result<()> { Ok(()) }
    fn resolution(&self) -> usize { 0 }

    /// Set metadata parsing options. Must be called before `set_id`.
    /// Default: parse all metadata.
    fn set_metadata_options(&mut self, _options: MetadataOptions) {
        // Default: ignore — readers that support levels can override
    }
    /// Return structured OME metadata.
    ///
    /// Equivalent to Java Bio-Formats `reader.setMetadataStore(service.createOMEXMLMetadata())`.
    ///
    /// The default implementation returns a baseline populated from
    /// [`FormatReader::metadata`] (channel count and samples-per-pixel).
    /// Format-specific overrides enrich this with physical pixel sizes,
    /// channel names, wavelengths, plane positions, etc.
    ///
    /// Must be called after [`FormatReader::set_id`].
    fn ome_metadata(&self) -> Option<OmeMetadata> {
        Some(OmeMetadata::from_image_metadata(self.metadata()))
    }
}
