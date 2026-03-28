//! Memoizer — caches parsed reader metadata to disk for fast re-opening.
//!
//! Equivalent to Java Bio-Formats' `Memoizer` class. On the first open of a
//! file, the inner reader parses the file normally and the metadata is
//! serialized to a `.bfmemo` cache file. On subsequent opens, if the cache is
//! valid (same file size and mtime), the cached metadata is loaded instead of
//! re-parsing.

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

/// Reader wrapper that caches parsed metadata to disk.
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
        let reader = crate::registry::ImageReader::open(path)?;
        // ImageReader doesn't expose its inner reader as Box<dyn FormatReader>.
        // Instead, use the registry directly to get a boxed reader.
        // We'll re-open through the memoizer's set_id.
        let header = crate::common::io::peek_header(path, 512).unwrap_or_default();
        let all = crate::registry::all_readers_pub();
        let mut found: Option<Box<dyn FormatReader>> = None;
        for r in all {
            if r.is_this_type_by_bytes(&header) {
                found = Some(r);
                break;
            }
        }
        if found.is_none() {
            let all2 = crate::registry::all_readers_pub();
            for r in all2 {
                if r.is_this_type_by_name(path) {
                    found = Some(r);
                    break;
                }
            }
        }
        let r = found.ok_or_else(|| BioFormatsError::UnsupportedFormat(path.display().to_string()))?;
        drop(reader); // close the initial reader
        let mut memo = Memoizer::new(r);
        memo.set_id(path)?;
        Ok(memo)
    }

    fn cache_path(file_path: &Path) -> PathBuf {
        let mut p = file_path.to_path_buf();
        let name = p.file_name().unwrap_or_default().to_string_lossy().to_string();
        p.set_file_name(format!(".{}.bfmemo", name));
        p
    }

    fn file_stamp(path: &Path) -> Option<(u64, u64)> {
        let md = std::fs::metadata(path).ok()?;
        let size = md.len();
        let mtime = md.modified().ok()?
            .duration_since(SystemTime::UNIX_EPOCH).ok()?
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
        let Some(file_path) = &self.file_path else { return };
        let Some((size, mtime)) = Self::file_stamp(file_path) else { return };
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
}

impl FormatReader for Memoizer {
    fn is_this_type_by_name(&self, path: &Path) -> bool { self.inner.is_this_type_by_name(path) }
    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool { self.inner.is_this_type_by_bytes(header) }

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

    fn series_count(&self) -> usize { self.cached_meta.len() }

    fn set_series(&mut self, series: usize) -> Result<()> {
        if series >= self.cached_meta.len() {
            return Err(BioFormatsError::SeriesOutOfRange(series));
        }
        self.current_series = series;
        self.inner.set_series(series)
    }

    fn series(&self) -> usize { self.current_series }

    fn metadata(&self) -> &ImageMetadata {
        &self.cached_meta[self.current_series]
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

    fn resolution_count(&self) -> usize { self.inner.resolution_count() }
    fn set_resolution(&mut self, level: usize) -> Result<()> { self.inner.set_resolution(level) }
    fn resolution(&self) -> usize { self.inner.resolution() }
    fn ome_metadata(&self) -> Option<OmeMetadata> { self.inner.ome_metadata() }
}
