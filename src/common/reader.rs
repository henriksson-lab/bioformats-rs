use super::ome_metadata::OmeMetadata;
use crate::common::metadata::{ImageMetadata, MetadataOptions};
use crate::error::Result;
use std::path::Path;
use std::sync::OnceLock;

/// Shared fallback metadata for uninitialized readers.
///
/// The legacy reader trait returns `&ImageMetadata` instead of `Result`, so
/// direct calls before `set_id` cannot report a normal error. Readers should
/// return this value instead of panicking until the trait can grow a fallible
/// metadata accessor.
pub fn uninitialized_metadata() -> &'static ImageMetadata {
    static EMPTY: OnceLock<ImageMetadata> = OnceLock::new();
    EMPTY.get_or_init(ImageMetadata::default)
}

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
    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>>;
    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>>;
    fn resolution_count(&self) -> usize {
        1
    }
    fn set_resolution(&mut self, _level: usize) -> Result<()> {
        Ok(())
    }
    fn resolution(&self) -> usize {
        0
    }

    /// Set metadata parsing options. Must be called before `set_id`.
    ///
    /// The default implementation is a no-op. Readers that implement a
    /// cheaper metadata path should override this; otherwise they parse their
    /// normal metadata regardless of the requested level.
    fn set_metadata_options(&mut self, _options: MetadataOptions) {
        // Default: ignore.
    }
    /// Return structured OME metadata.
    ///
    /// The default implementation returns baseline OME metadata derived from
    /// [`FormatReader::metadata`]. Format-specific overrides enrich this with
    /// physical pixel sizes, channel names, wavelengths, plane positions, etc.
    /// This is a convenience conversion, not Java Bio-Formats'
    /// pre-`setId` metadata-store configuration model.
    ///
    /// Must be called after [`FormatReader::set_id`].
    fn ome_metadata(&self) -> Option<OmeMetadata> {
        let meta = self.metadata();
        if std::ptr::eq(meta, uninitialized_metadata()) {
            None
        } else {
            Some(OmeMetadata::from_image_metadata(meta))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{uninitialized_metadata, FormatReader};
    use crate::common::error::{BioFormatsError, Result};
    use crate::common::metadata::ImageMetadata;
    use std::path::Path;

    struct UninitializedReader;

    impl FormatReader for UninitializedReader {
        fn is_this_type_by_name(&self, _path: &Path) -> bool {
            false
        }
        fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
            false
        }
        fn set_id(&mut self, _path: &Path) -> Result<()> {
            Ok(())
        }
        fn close(&mut self) -> Result<()> {
            Ok(())
        }
        fn series_count(&self) -> usize {
            0
        }
        fn set_series(&mut self, series: usize) -> Result<()> {
            Err(BioFormatsError::SeriesOutOfRange(series))
        }
        fn series(&self) -> usize {
            0
        }
        fn metadata(&self) -> &ImageMetadata {
            uninitialized_metadata()
        }
        fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
            Err(BioFormatsError::PlaneOutOfRange(plane_index))
        }
        fn open_bytes_region(
            &mut self,
            plane_index: u32,
            _x: u32,
            _y: u32,
            _w: u32,
            _h: u32,
        ) -> Result<Vec<u8>> {
            Err(BioFormatsError::PlaneOutOfRange(plane_index))
        }
        fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
            Err(BioFormatsError::PlaneOutOfRange(plane_index))
        }
    }

    #[test]
    fn default_ome_metadata_returns_none_for_uninitialized_metadata() {
        let reader = UninitializedReader;
        assert!(reader.ome_metadata().is_none());
    }
}
