//! Leica LIF (Leica Image Format) detector.
//!
//! LIF is a binary Leica container with UTF-16 XML metadata and separate memory
//! blocks for image payloads. The previous reader parsed only a small subset of
//! that structure, exposed no stable `ImageMetadata`, and guessed plane offsets.
//! Until the full metadata and plane layout rules are ported, this reader
//! identifies candidate files and fails explicitly instead of exposing fake
//! metadata or incorrect pixels.

use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::ImageMetadata;
use crate::common::reader::FormatReader;

const LIF_MAGIC: u8 = 0x70;
const LIF_UNSUPPORTED: &str =
    "Leica LIF decoding is incomplete; disabled until metadata and plane layout are ported";

pub struct LifReader {
    path: Option<PathBuf>,
    meta: ImageMetadata,
    current_series: usize,
}

impl LifReader {
    pub fn new() -> Self {
        LifReader {
            path: None,
            meta: ImageMetadata::default(),
            current_series: 0,
        }
    }

    fn unsupported() -> BioFormatsError {
        BioFormatsError::UnsupportedFormat(LIF_UNSUPPORTED.to_string())
    }
}

impl Default for LifReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for LifReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("lif"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        !header.is_empty() && header[0] == LIF_MAGIC
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.path = None;
        self.current_series = 0;
        self.meta = ImageMetadata::default();

        if !self.is_this_type_by_name(path) {
            let header = std::fs::read(path).map_err(BioFormatsError::Io)?;
            if !self.is_this_type_by_bytes(&header) {
                return Err(BioFormatsError::Format("Not a Leica LIF file".into()));
            }
        }

        Err(Self::unsupported())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = ImageMetadata::default();
        self.current_series = 0;
        Ok(())
    }

    fn series_count(&self) -> usize {
        1
    }

    fn set_series(&mut self, series: usize) -> Result<()> {
        if series != 0 {
            return Err(BioFormatsError::SeriesOutOfRange(series));
        }
        self.current_series = series;
        Ok(())
    }

    fn series(&self) -> usize {
        self.current_series
    }

    fn metadata(&self) -> &ImageMetadata {
        &self.meta
    }

    fn open_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
        Err(Self::unsupported())
    }

    fn open_bytes_region(
        &mut self,
        _plane_index: u32,
        _x: u32,
        _y: u32,
        _w: u32,
        _h: u32,
    ) -> Result<Vec<u8>> {
        Err(Self::unsupported())
    }

    fn open_thumb_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
        Err(Self::unsupported())
    }
}

#[cfg(test)]
mod tests {
    use super::{LifReader, LIF_MAGIC, LIF_UNSUPPORTED};
    use crate::common::error::BioFormatsError;
    use crate::common::reader::FormatReader;
    use crate::ImageReader;
    use std::path::PathBuf;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("bioformats_lif_{name}"))
    }

    fn assert_lif_unsupported(err: BioFormatsError) {
        assert!(
            matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains(LIF_UNSUPPORTED)),
            "expected LIF unsupported error, got {err:?}"
        );
    }

    #[test]
    fn invalid_lif_extension_rejects_without_metadata_panic() {
        let path = temp_path("tiny_invalid.lif");
        std::fs::write(&path, b"not a real lif").unwrap();

        let mut reader = LifReader::new();
        assert_lif_unsupported(reader.set_id(&path).unwrap_err());
        assert_eq!(reader.metadata().size_x, 0);
        assert_lif_unsupported(reader.open_bytes(0).unwrap_err());

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn image_reader_does_not_open_tiny_lif_with_fake_metadata() {
        let path = temp_path("tiny_registry.lif");
        std::fs::write(&path, [LIF_MAGIC, 0, 0, 0, 0]).unwrap();

        let err = match ImageReader::open(&path) {
            Ok(_) => panic!("LIF reader opened fake metadata"),
            Err(err) => err,
        };
        assert_lif_unsupported(err);

        let _ = std::fs::remove_file(path);
    }
}
