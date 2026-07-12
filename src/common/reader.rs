use super::ome_metadata::OmeMetadata;
use crate::common::compressed::{CompressedExtractionSupport, CompressedTile, CompressedTileMode};
use crate::common::error::BioFormatsError;
use crate::common::metadata::{ImageMetadata, LookupTable, MetadataOptions};
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

    /// Report whether a logical plane/resolution level can expose source
    /// lossy-compressed blocks without pixel-domain decode/recompress.
    ///
    /// Most readers return `NotSupported`. Implementations should be
    /// conservative and only report `Supported` when one requested compressed
    /// tile/frame maps cleanly to stored source bytes or a lossless repack.
    fn compressed_level_info(
        &self,
        _plane_index: u32,
        _level: u32,
    ) -> Result<CompressedExtractionSupport> {
        Ok(CompressedExtractionSupport::NotSupported {
            reason: "reader does not expose compressed source blocks".into(),
        })
    }

    /// Return one source compressed tile/frame for a logical plane/resolution.
    fn read_compressed_tile(
        &mut self,
        _plane_index: u32,
        _level: u32,
        _col: u64,
        _row: u64,
        _preferred_modes: &[CompressedTileMode],
    ) -> Result<CompressedTile> {
        Err(BioFormatsError::UnsupportedFormat(
            "reader does not expose compressed source blocks".into(),
        ))
    }
    fn resolution_count(&self) -> usize {
        1
    }
    fn set_resolution(&mut self, _level: usize) -> Result<()> {
        Ok(())
    }
    fn resolution(&self) -> usize {
        0
    }

    /// Whether the current series is a low-resolution thumbnail/preview rather
    /// than full-resolution image data. Mirrors Java
    /// `IFormatReader.isThumbnailSeries()`. The default reads
    /// [`ImageMetadata::thumbnail`]; readers that flag thumbnail series (e.g.
    /// Imaris collapsed sub-resolutions) populate that field.
    fn is_thumbnail_series(&self) -> bool {
        self.metadata().thumbnail
    }

    /// Return the colour lookup table that applies to `plane_index`'s channel,
    /// if the image is indexed.
    ///
    /// The default implementation returns the single LUT stored in
    /// [`FormatReader::metadata`] (ignoring the plane). Indexed readers that
    /// carry a distinct palette per channel (e.g. Imaris) override this to
    /// select the LUT for the plane's channel. Mirrors Java's
    /// `get8BitLookupTable`/`get16BitLookupTable` per-`lastChannel` selection.
    fn lookup_table(&mut self, _plane_index: u32) -> Result<Option<LookupTable>> {
        Ok(self.metadata().lookup_table.clone())
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

    /// Minimal reader that returns a caller-supplied `ImageMetadata`, used to
    /// exercise the default trait accessors that read from it.
    struct MetaReader(ImageMetadata);

    impl FormatReader for MetaReader {
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
            1
        }
        fn set_series(&mut self, _series: usize) -> Result<()> {
            Ok(())
        }
        fn series(&self) -> usize {
            0
        }
        fn metadata(&self) -> &ImageMetadata {
            &self.0
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
    fn is_thumbnail_series_reflects_metadata_flag() {
        // Default ImageMetadata is a full-resolution (non-thumbnail) series.
        let full = MetaReader(ImageMetadata::default());
        assert!(!full.0.thumbnail);
        assert!(!full.is_thumbnail_series());

        let mut meta = ImageMetadata::default();
        meta.thumbnail = true;
        let thumb = MetaReader(meta);
        assert!(thumb.is_thumbnail_series());
    }
}
