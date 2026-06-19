//! Memoizer — caches parsed reader metadata to disk for fast re-opening.
//!
//! Equivalent to Java Bio-Formats' `Memoizer` class. On the first open of a
//! file, the inner reader parses the file normally and core series metadata is
//! serialized to a `.bfmemo` cache file. On subsequent opens, if the cache is
//! valid (same file size and mtime), that cached core metadata is reused for
//! [`FormatReader::metadata`]. Rich OME metadata is cached when available, so
//! [`FormatReader::ome_metadata`] can also be served from a valid memo file. The
//! wrapped reader is opened lazily only if pixel access, thumbnails, or
//! resolution changes are requested.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{ImageMetadata, MetadataOptions};
use crate::common::ome_metadata::OmeMetadata;
use crate::common::reader::FormatReader;

/// Cached state that can be serialized/deserialized.
#[derive(Serialize, Deserialize)]
struct MemoCache {
    /// File size at time of caching.
    file_size: u64,
    /// File modification time (seconds since epoch).
    mtime_secs: u64,
    /// Nanosecond sub-second component of the file modification time.
    #[serde(default)]
    mtime_nanos: u32,
    /// Number of series.
    series_count: usize,
    /// Metadata per series.
    series_metadata: Vec<ImageMetadata>,
    /// Rich OME metadata captured from the initialized reader, if available.
    #[serde(default)]
    ome_metadata: Option<OmeMetadata>,
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
    inner_opened: bool,
    /// Cached metadata for all series (loaded from cache or from inner reader).
    cached_meta: Vec<ImageMetadata>,
    cached_ome_metadata: Option<OmeMetadata>,
    empty_meta: ImageMetadata,
    metadata_options: MetadataOptions,
    current_series: usize,
}

impl Memoizer {
    /// Create a memoizer wrapping a specific reader.
    pub fn new(inner: Box<dyn FormatReader>) -> Self {
        Memoizer {
            inner,
            file_path: None,
            inner_opened: false,
            cached_meta: Vec::new(),
            cached_ome_metadata: None,
            empty_meta: ImageMetadata::default(),
            metadata_options: MetadataOptions::default(),
            current_series: 0,
        }
    }

    /// Convenience: open a file with auto-detection and memoization.
    pub fn open(path: &Path) -> Result<Self> {
        let cache = Self::try_load_cache(path);
        let r = if cache.is_some() {
            crate::registry::detect_reader_without_set_id(path)?
        } else {
            crate::registry::open_reader(path)?
        };
        let mut memo = Memoizer::new(r);
        memo.file_path = Some(path.to_path_buf());
        if let Some(cache) = cache {
            memo.cached_meta = cache.series_metadata;
            memo.cached_ome_metadata = cache.ome_metadata;
            memo.current_series = 0;
        } else {
            memo.inner_opened = true;
            memo.initialize_opened_reader(path)?;
        }
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

    fn file_stamp(path: &Path) -> Option<(u64, u64, u32)> {
        let md = std::fs::metadata(path).ok()?;
        let size = md.len();
        let mtime = md
            .modified()
            .ok()?
            .duration_since(SystemTime::UNIX_EPOCH)
            .ok()?;
        Some((size, mtime.as_secs(), mtime.subsec_nanos()))
    }

    fn cache_shape_is_valid(cache: &MemoCache) -> bool {
        cache.series_count == cache.series_metadata.len() && !cache.series_metadata.is_empty()
    }

    fn cache_matches_stamp(cache: &MemoCache, stamp: (u64, u64, u32)) -> bool {
        let (size, mtime_secs, mtime_nanos) = stamp;
        cache.file_size == size
            && cache.mtime_secs == mtime_secs
            && cache.mtime_nanos == mtime_nanos
    }

    fn cache_from_reader(&mut self) -> Result<()> {
        let sc = self.inner.series_count();
        let mut series_metadata = Vec::with_capacity(sc);
        for s in 0..sc {
            self.inner.set_series(s)?;
            series_metadata.push(self.inner.metadata().clone());
        }
        let ome_metadata = self.inner.ome_metadata();
        if sc > 0 {
            self.inner.set_series(0)?;
        }
        self.cached_meta = series_metadata;
        self.cached_ome_metadata = ome_metadata;
        self.current_series = 0;
        Ok(())
    }

    fn ensure_inner_opened(&mut self) -> Result<()> {
        if self.inner_opened {
            return Ok(());
        }
        let path = self
            .file_path
            .clone()
            .ok_or(BioFormatsError::NotInitialized)?;
        match self.inner.set_id(&path) {
            Ok(()) => {
                self.inner_opened = true;
            }
            Err(first_err) => {
                let mut reopened = match crate::registry::open_reader(&path) {
                    Ok(reader) => reader,
                    Err(_) => return Err(first_err),
                };
                reopened.set_metadata_options(self.metadata_options.clone());
                self.inner = reopened;
                self.inner_opened = true;
            }
        }
        if self.inner.series_count() != self.cached_meta.len() {
            self.cache_from_reader()?;
            self.save_cache();
        } else if self.current_series < self.cached_meta.len() {
            self.inner.set_series(self.current_series)?;
        }
        Ok(())
    }

    fn try_load_cache(file_path: &Path) -> Option<MemoCache> {
        let cache_path = Self::cache_path(file_path);
        let data = std::fs::read(&cache_path).ok()?;
        let cache: MemoCache = match bincode::deserialize(&data) {
            Ok(cache) => cache,
            Err(_) => {
                let _ = std::fs::remove_file(&cache_path);
                return None;
            }
        };
        if !Self::cache_shape_is_valid(&cache) {
            return None;
        }
        // Validate against current file
        if Self::cache_matches_stamp(&cache, Self::file_stamp(file_path)?) {
            Some(cache)
        } else {
            None
        }
    }

    fn save_cache(&self) {
        let Some(file_path) = &self.file_path else {
            return;
        };
        let Some((size, mtime_secs, mtime_nanos)) = Self::file_stamp(file_path) else {
            return;
        };
        let cache = MemoCache {
            file_size: size,
            mtime_secs,
            mtime_nanos,
            series_count: self.cached_meta.len(),
            series_metadata: self.cached_meta.clone(),
            ome_metadata: self.cached_ome_metadata.clone(),
        };
        if let Ok(data) = bincode::serialize(&cache) {
            let _ = std::fs::write(Self::cache_path(file_path), data);
        }
    }

    fn initialize_opened_reader(&mut self, path: &Path) -> Result<()> {
        self.file_path = Some(path.to_path_buf());

        self.cache_from_reader()?;
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
            self.inner = crate::registry::detect_reader_without_set_id(path)?;
            self.inner
                .set_metadata_options(self.metadata_options.clone());
            self.inner_opened = false;
            self.cached_meta = cache.series_metadata;
            self.cached_ome_metadata = cache.ome_metadata;
            self.current_series = 0;
            return Ok(());
        }

        // No valid cache — parse normally
        self.inner.set_id(path)?;
        self.inner_opened = true;
        self.cache_from_reader()?;
        // Save cache for next time
        self.save_cache();
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.cached_meta.clear();
        self.cached_ome_metadata = None;
        self.file_path = None;
        self.inner_opened = false;
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
        if self.inner_opened {
            self.inner.set_series(series)?;
        }
        Ok(())
    }

    fn series(&self) -> usize {
        self.current_series
    }

    fn metadata(&self) -> &ImageMetadata {
        self.cached_meta
            .get(self.current_series)
            .unwrap_or(&self.empty_meta)
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        self.ensure_inner_opened()?;
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
        self.ensure_inner_opened()?;
        self.inner.open_bytes_region(plane_index, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        self.ensure_inner_opened()?;
        self.inner.open_thumb_bytes(plane_index)
    }

    fn resolution_count(&self) -> usize {
        if !self.inner_opened {
            return self.metadata().resolution_count.max(1) as usize;
        }
        self.inner.resolution_count()
    }
    fn set_resolution(&mut self, level: usize) -> Result<()> {
        self.ensure_inner_opened()?;
        self.inner.set_resolution(level)
    }
    fn resolution(&self) -> usize {
        if !self.inner_opened {
            return 0;
        }
        self.inner.resolution()
    }
    fn set_metadata_options(&mut self, options: MetadataOptions) {
        self.metadata_options = options.clone();
        self.inner.set_metadata_options(options);
    }
    fn ome_metadata(&self) -> Option<OmeMetadata> {
        if !self.inner_opened {
            return self
                .cached_ome_metadata
                .clone()
                .or_else(|| Some(OmeMetadata::from_image_metadata(self.metadata())));
        }
        self.inner
            .ome_metadata()
            .or_else(|| self.cached_ome_metadata.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::{MemoCache, Memoizer};
    use crate::common::error::{BioFormatsError, Result};
    use crate::common::metadata::{DimensionOrder, ImageMetadata};
    use crate::common::pixel_type::PixelType;
    use crate::common::reader::FormatReader;
    use crate::writer_registry::ImageWriter;
    use std::path::Path;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("bioformats_memoizer_{nanos}_{name}"))
    }

    struct FailingSeriesReader {
        meta: ImageMetadata,
    }

    impl FormatReader for FailingSeriesReader {
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
            2
        }
        fn set_series(&mut self, series: usize) -> Result<()> {
            if series == 1 {
                Err(BioFormatsError::SeriesOutOfRange(series))
            } else {
                Ok(())
            }
        }
        fn series(&self) -> usize {
            0
        }
        fn metadata(&self) -> &ImageMetadata {
            &self.meta
        }
        fn open_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
            Err(BioFormatsError::NotInitialized)
        }
        fn open_bytes_region(
            &mut self,
            _plane_index: u32,
            _x: u32,
            _y: u32,
            _w: u32,
            _h: u32,
        ) -> Result<Vec<u8>> {
            Err(BioFormatsError::NotInitialized)
        }
        fn open_thumb_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
            Err(BioFormatsError::NotInitialized)
        }
    }

    #[test]
    fn cache_refresh_failure_keeps_existing_metadata() {
        let mut existing_meta = ImageMetadata::default();
        existing_meta.size_x = 77;
        let mut replacement_meta = ImageMetadata::default();
        replacement_meta.size_x = 88;

        let mut memo = Memoizer::new(Box::new(FailingSeriesReader {
            meta: replacement_meta,
        }));
        memo.cached_meta = vec![existing_meta];

        let err = memo.cache_from_reader().unwrap_err();
        assert!(
            err.to_string().contains("Series index 1 out of range"),
            "unexpected error: {err}"
        );
        assert_eq!(memo.cached_meta.len(), 1);
        assert_eq!(memo.cached_meta[0].size_x, 77);
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

    #[test]
    fn open_uses_valid_cache_metadata() {
        let path = temp_path("cached.fake");
        std::fs::write(&path, b"fake").unwrap();

        let first = Memoizer::open(&path).expect("initial fake open failed");
        assert_eq!(first.metadata().size_x, 512);

        let (file_size, mtime_secs, mtime_nanos) = Memoizer::file_stamp(&path).unwrap();
        let mut cached_meta = first.metadata().clone();
        cached_meta.size_x = 123;
        let cache = MemoCache {
            file_size,
            mtime_secs,
            mtime_nanos,
            series_count: 1,
            series_metadata: vec![cached_meta],
            ome_metadata: None,
        };
        std::fs::write(
            Memoizer::cache_path(&path),
            bincode::serialize(&cache).unwrap(),
        )
        .unwrap();

        let second = Memoizer::open(&path).expect("cached fake open failed");
        assert_eq!(second.metadata().size_x, 123);

        let _ = std::fs::remove_file(Memoizer::cache_path(&path));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn cache_hit_uses_extension_fallback_without_upfront_set_id() {
        let path = temp_path("cached_magic_png_but_fake.fake");
        std::fs::write(&path, b"\x89PNG\r\n\x1a\nnot enough png data").unwrap();

        let first = Memoizer::open(&path).expect("initial fake open failed");
        let (file_size, mtime_secs, mtime_nanos) = Memoizer::file_stamp(&path).unwrap();
        let mut cached_meta = first.metadata().clone();
        cached_meta.size_x = 321;
        let cache = MemoCache {
            file_size,
            mtime_secs,
            mtime_nanos,
            series_count: 1,
            series_metadata: vec![cached_meta],
            ome_metadata: None,
        };
        std::fs::write(
            Memoizer::cache_path(&path),
            bincode::serialize(&cache).unwrap(),
        )
        .unwrap();

        let mut second = Memoizer::open(&path).expect("cached extension fallback failed");
        assert_eq!(second.metadata().size_x, 321);
        assert_eq!(
            second.open_bytes(0).expect("lazy fake open failed").len(),
            512 * 512
        );

        let _ = std::fs::remove_file(Memoizer::cache_path(&path));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn cache_hit_preserves_magic_reader_when_magic_set_id_succeeds() {
        let path = temp_path("cached_valid_tiff_but_fake.fake");
        let tiff_path = path.with_extension("tif");
        let meta = ImageMetadata {
            size_x: 1,
            size_y: 1,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type: PixelType::Uint8,
            bits_per_pixel: 8,
            image_count: 1,
            dimension_order: DimensionOrder::XYZCT,
            ..Default::default()
        };
        ImageWriter::save(&tiff_path, &meta, &[vec![7]]).unwrap();
        std::fs::rename(&tiff_path, &path).unwrap();

        let first = Memoizer::open(&path).expect("initial TIFF open failed");
        assert_eq!(first.metadata().size_x, 1);
        assert_eq!(first.metadata().size_y, 1);

        let second = Memoizer::open(&path).expect("cached TIFF open failed");
        assert_eq!(second.metadata().size_x, 1);
        assert_eq!(second.metadata().size_y, 1);

        let _ = std::fs::remove_file(Memoizer::cache_path(&path));
        let _ = std::fs::remove_file(tiff_path);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn cache_hit_preserves_rich_ome_metadata_without_reopen() {
        let path = temp_path("rich.ome");
        let xml = r#"<OME><Image ID="Image:0"><Pixels ID="Pixels:0" DimensionOrder="XYZCT" Type="uint8" PhysicalSizeX="0.25" PhysicalSizeXUnit="µm" SizeX="1" SizeY="1" SizeZ="1" SizeC="1" SizeT="1"><Channel ID="Channel:0:0" Name="DAPI" SamplesPerPixel="1"/><BinData BigEndian="false">Kg==</BinData></Pixels></Image></OME>"#;
        std::fs::write(&path, xml).unwrap();

        let first = Memoizer::open(&path).expect("initial OME open failed");
        let first_ome = first.ome_metadata().expect("initial rich OME missing");
        assert_eq!(first_ome.images[0].physical_size_x, Some(0.25));
        assert_eq!(
            first_ome.images[0].channels[0].name.as_deref(),
            Some("DAPI")
        );

        let second = Memoizer::open(&path).expect("cached OME open failed");
        let cached_ome = second.ome_metadata().expect("cached rich OME missing");

        assert_eq!(cached_ome.images[0].physical_size_x, Some(0.25));
        assert_eq!(
            cached_ome.images[0].channels[0].name.as_deref(),
            Some("DAPI")
        );

        let _ = std::fs::remove_file(Memoizer::cache_path(&path));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn open_rejects_cache_with_mismatched_series_count() {
        let path = temp_path("bad_shape.fake");
        std::fs::write(&path, b"fake").unwrap();

        let first = Memoizer::open(&path).expect("initial fake open failed");
        let (file_size, mtime_secs, mtime_nanos) = Memoizer::file_stamp(&path).unwrap();
        let mut cached_meta = first.metadata().clone();
        cached_meta.size_x = 999;
        let cache = MemoCache {
            file_size,
            mtime_secs,
            mtime_nanos,
            series_count: 2,
            series_metadata: vec![cached_meta],
            ome_metadata: None,
        };
        std::fs::write(
            Memoizer::cache_path(&path),
            bincode::serialize(&cache).unwrap(),
        )
        .unwrap();

        let second = Memoizer::open(&path).expect("bad-shape cache should be ignored");
        assert_eq!(second.metadata().size_x, 512);

        let _ = std::fs::remove_file(Memoizer::cache_path(&path));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn corrupt_cache_is_deleted_and_regenerated() {
        let path = temp_path("corrupt_cache.fake");
        std::fs::write(&path, b"fake").unwrap();
        let cache_path = Memoizer::cache_path(&path);
        std::fs::write(&cache_path, b"not a bincode memo").unwrap();

        let reader = Memoizer::open(&path).expect("corrupt cache should fall back");

        assert_eq!(reader.metadata().size_x, 512);
        let regenerated = std::fs::read(&cache_path).expect("cache should be regenerated");
        assert_ne!(regenerated, b"not a bincode memo");

        let _ = std::fs::remove_file(cache_path);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn metadata_after_close_returns_empty_fallback() {
        let path = temp_path("close.fake");
        std::fs::write(&path, b"fake").unwrap();

        let mut reader = Memoizer::open(&path).expect("fake open failed");
        reader.close().unwrap();

        assert_eq!(reader.series_count(), 0);
        assert_eq!(reader.metadata().size_x, 0);

        let _ = std::fs::remove_file(Memoizer::cache_path(&path));
        let _ = std::fs::remove_file(path);
    }
}
