//! Memoizer — caches parsed reader metadata to disk for fast re-opening.
//!
//! Equivalent to Java Bio-Formats' `Memoizer` class. On the first open of a
//! file, the inner reader parses the file normally and core series metadata is
//! serialized to a `.bfmemo` cache file. On subsequent opens, if the cache is
//! valid (same file size and mtime), that cached core metadata is reused for
//! [`FormatReader::metadata`]. The wrapped reader is still opened for pixel
//! access and format-specific metadata such as OME XML.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::ImageMetadata;
use crate::common::ome_metadata::OmeMetadata;
use crate::common::reader::FormatReader;

/// Cached state that can be serialized/deserialized.
#[derive(Serialize, Deserialize)]
struct MemoCache {
    /// File size at time of caching.
    file_size: u64,
    /// File modification time (seconds since epoch).
    mtime_secs: u64,
    /// Number of series.
    series_count: usize,
    /// Metadata per series.
    series_metadata: Vec<ImageMetadata>,
}

/// Reader wrapper that caches core series metadata to disk.
///
/// Memoizer does not replace the underlying format reader: `set_id` still
/// initializes the wrapped reader so pixel reads, thumbnails, resolutions, and
/// format-specific metadata remain backed by the parsed source file. The cache
/// currently covers only the per-series [`ImageMetadata`] returned by
/// [`FormatReader::metadata`].
///
/// # Usage
/// ```no_run
/// use bioformats::Memoizer;
/// use bioformats::FormatReader;
/// use std::path::Path;
///
/// // Wrap the auto-detecting reader with memoization
/// let mut reader = Memoizer::open(Path::new("large_file.nd2")).unwrap();
/// let meta = reader.metadata();
/// ```
pub struct Memoizer {
    inner: Box<dyn FormatReader>,
    file_path: Option<PathBuf>,
    /// Cached metadata for all series (loaded from cache or from inner reader).
    cached_meta: Vec<ImageMetadata>,
    current_series: usize,
}

impl Memoizer {
    /// Create a memoizer wrapping a specific reader.
    pub fn new(inner: Box<dyn FormatReader>) -> Self {
        Memoizer {
            inner,
            file_path: None,
            cached_meta: Vec::new(),
            current_series: 0,
        }
    }

    /// Convenience: open a file with auto-detection and memoization.
    pub fn open(path: &Path) -> Result<Self> {
        let r = crate::registry::open_reader(path)?;
        let mut memo = Memoizer::new(r);
        memo.initialize_opened_reader(path)?;
        Ok(memo)
    }

    fn cache_path(file_path: &Path) -> PathBuf {
        let mut p = file_path.to_path_buf();
        let name = p
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        p.set_file_name(format!(".{}.bfmemo", name));
        p
    }

    fn file_stamp(path: &Path) -> Option<(u64, u64)> {
        let md = std::fs::metadata(path).ok()?;
        let size = md.len();
        let mtime = md
            .modified()
            .ok()?
            .duration_since(SystemTime::UNIX_EPOCH)
            .ok()?
            .as_secs();
        Some((size, mtime))
    }

    fn try_load_cache(file_path: &Path) -> Option<MemoCache> {
        let cache_path = Self::cache_path(file_path);
        let data = std::fs::read(&cache_path).ok()?;
        let cache: MemoCache = bincode::deserialize(&data).ok()?;
        // Validate against current file
        let (size, mtime) = Self::file_stamp(file_path)?;
        if cache.file_size == size && cache.mtime_secs == mtime {
            Some(cache)
        } else {
            None
        }
    }

    fn save_cache(&self) {
        let Some(file_path) = &self.file_path else {
            return;
        };
        let Some((size, mtime)) = Self::file_stamp(file_path) else {
            return;
        };
        let cache = MemoCache {
            file_size: size,
            mtime_secs: mtime,
            series_count: self.cached_meta.len(),
            series_metadata: self.cached_meta.clone(),
        };
        if let Ok(data) = bincode::serialize(&cache) {
            let _ = std::fs::write(Self::cache_path(file_path), data);
        }
    }

    fn initialize_opened_reader(&mut self, path: &Path) -> Result<()> {
        self.file_path = Some(path.to_path_buf());

        let sc = self.inner.series_count();
        self.cached_meta.clear();
        for s in 0..sc {
            self.inner.set_series(s)?;
            self.cached_meta.push(self.inner.metadata().clone());
        }
        if sc > 0 {
            self.inner.set_series(0)?;
        }
        self.current_series = 0;

        self.save_cache();
        Ok(())
    }
}

impl FormatReader for Memoizer {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        self.inner.is_this_type_by_name(path)
    }
    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        self.inner.is_this_type_by_bytes(header)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.file_path = Some(path.to_path_buf());

        // Try loading from cache
        if let Some(cache) = Self::try_load_cache(path) {
            // We still need to open the inner reader for pixel data access
            self.inner.set_id(path)?;
            self.cached_meta = cache.series_metadata;
            self.current_series = 0;
            return Ok(());
        }

        // No valid cache — parse normally
        self.inner.set_id(path)?;
        let sc = self.inner.series_count();
        self.cached_meta.clear();
        for s in 0..sc {
            self.inner.set_series(s)?;
            self.cached_meta.push(self.inner.metadata().clone());
        }
        if sc > 0 {
            self.inner.set_series(0)?;
        }
        self.current_series = 0;

        // Save cache for next time
        self.save_cache();
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.cached_meta.clear();
        self.file_path = None;
        self.inner.close()
    }

    fn series_count(&self) -> usize {
        self.cached_meta.len()
    }

    fn set_series(&mut self, series: usize) -> Result<()> {
        if series >= self.cached_meta.len() {
            return Err(BioFormatsError::SeriesOutOfRange(series));
        }
        self.current_series = series;
        self.inner.set_series(series)
    }

    fn series(&self) -> usize {
        self.current_series
    }

    fn metadata(&self) -> &ImageMetadata {
        &self.cached_meta[self.current_series]
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        self.inner.open_bytes(plane_index)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
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
    fn ome_metadata(&self) -> Option<OmeMetadata> {
        self.inner.ome_metadata()
    }
}

#[cfg(test)]
mod tests {
    use super::Memoizer;
    use crate::common::reader::FormatReader;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("bioformats_memoizer_{nanos}_{name}"))
    }

    #[test]
    fn open_uses_image_reader_extension_fallback_after_magic_set_id_error() {
        let path = temp_path("magic_png_but_fake.fake");
        std::fs::write(&path, b"\x89PNG\r\n\x1a\nnot enough png data").unwrap();

        let reader = Memoizer::open(&path).expect("fake extension fallback failed");

        assert_eq!(reader.metadata().size_x, 512);
        assert_eq!(reader.metadata().size_y, 512);
        let _ = std::fs::remove_file(Memoizer::cache_path(&path));
        let _ = std::fs::remove_file(path);
    }
}
