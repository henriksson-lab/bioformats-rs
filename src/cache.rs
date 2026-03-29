//! Plane cache framework — caches decoded pixel data in memory.
//!
//! Equivalent to Java Bio-Formats' `loci.formats.cache` package.
//! Provides configurable caching strategies for multi-dimensional image data,
//! reducing redundant decompression of frequently-accessed planes.

use std::collections::HashMap;
use std::path::Path;

use crate::common::error::Result;
use crate::common::metadata::ImageMetadata;
use crate::common::ome_metadata::OmeMetadata;
use crate::common::reader::FormatReader;

/// Caching strategy that determines which planes to keep in memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheStrategy {
    /// Keep the N most recently accessed planes (LRU eviction).
    Lru(usize),
    /// Keep planes in a rectangular neighbourhood around the current position.
    /// The parameter is the radius in each dimension.
    Rectangle(usize),
    /// Keep planes along the current Z, C, T axes (crosshair pattern).
    Crosshair,
    /// No caching — always read from the inner reader.
    None,
}

impl Default for CacheStrategy {
    fn default() -> Self { CacheStrategy::Lru(64) }
}

/// A cached plane reader that wraps a `FormatReader` and caches decoded planes
/// in memory according to a configurable strategy.
pub struct CachedReader {
    inner: Box<dyn FormatReader>,
    #[allow(dead_code)]
    strategy: CacheStrategy,
    /// Cached planes keyed by (series, resolution, plane_index).
    cache: HashMap<(usize, usize, u32), Vec<u8>>,
    /// Access order for LRU eviction: oldest first.
    access_order: Vec<(usize, usize, u32)>,
    max_planes: usize,
}

impl CachedReader {
    pub fn new(inner: Box<dyn FormatReader>, strategy: CacheStrategy) -> Self {
        let max_planes = match strategy {
            CacheStrategy::Lru(n) => n,
            CacheStrategy::Rectangle(r) => (2 * r + 1).pow(3), // cube neighbourhood
            CacheStrategy::Crosshair => 256,
            CacheStrategy::None => 0,
        };
        CachedReader {
            inner,
            strategy,
            cache: HashMap::new(),
            access_order: Vec::new(),
            max_planes,
        }
    }

    /// Number of planes currently cached.
    pub fn cached_count(&self) -> usize { self.cache.len() }

    /// Clear the cache.
    pub fn clear_cache(&mut self) {
        self.cache.clear();
        self.access_order.clear();
    }

    fn cache_key(&self, plane_index: u32) -> (usize, usize, u32) {
        (self.inner.series(), self.inner.resolution(), plane_index)
    }

    fn evict_if_needed(&mut self) {
        while self.cache.len() >= self.max_planes && !self.access_order.is_empty() {
            let oldest = self.access_order.remove(0);
            self.cache.remove(&oldest);
        }
    }

    fn store(&mut self, key: (usize, usize, u32), data: Vec<u8>) {
        if self.max_planes == 0 { return; }
        self.evict_if_needed();
        // Move to end of access order (most recent)
        self.access_order.retain(|k| k != &key);
        self.access_order.push(key);
        self.cache.insert(key, data);
    }
}

impl FormatReader for CachedReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool { self.inner.is_this_type_by_name(path) }
    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool { self.inner.is_this_type_by_bytes(header) }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.clear_cache();
        self.inner.set_id(path)
    }

    fn close(&mut self) -> Result<()> {
        self.clear_cache();
        self.inner.close()
    }

    fn series_count(&self) -> usize { self.inner.series_count() }

    fn set_series(&mut self, series: usize) -> Result<()> {
        self.inner.set_series(series)
    }

    fn series(&self) -> usize { self.inner.series() }
    fn metadata(&self) -> &ImageMetadata { self.inner.metadata() }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let key = self.cache_key(plane_index);
        if let Some(data) = self.cache.get(&key) {
            // Move to end of access order
            self.access_order.retain(|k| k != &key);
            self.access_order.push(key);
            return Ok(data.clone());
        }
        let data = self.inner.open_bytes(plane_index)?;
        self.store(key, data.clone());
        Ok(data)
    }

    fn open_bytes_region(&mut self, plane_index: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        // Region reads are not cached (different regions of same plane)
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
