//! ZIP container reader.
//!
//! Detects ZIP files by magic bytes PK\x03\x04. Following the Java `ZipReader`,
//! this wraps an inner auto-detecting `ImageReader` over the archive entries:
//! all entries are extracted to a temporary directory preserving their safe
//! relative paths, the "primary" entry is selected (the first one whose name
//! starts with the archive's base name, else the first entry), and that entry
//! is delegated to the auto-detecting image reader.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::ImageMetadata;
use crate::common::reader::FormatReader;
use crate::registry::ImageReader;

pub struct ZipReader {
    /// Directory holding the extracted entries; removed on close.
    extracted_dir: Option<PathBuf>,
    /// All files extracted from the archive (absolute paths), in archive order.
    extracted_files: Vec<PathBuf>,
    /// Inner auto-detecting reader operating on the primary entry.
    inner: Option<ImageReader>,
}

impl ZipReader {
    pub fn new() -> Self {
        ZipReader {
            extracted_dir: None,
            extracted_files: Vec::new(),
            inner: None,
        }
    }

    fn inner(&self) -> Result<&ImageReader> {
        self.inner.as_ref().ok_or(BioFormatsError::NotInitialized)
    }

    fn inner_mut(&mut self) -> Result<&mut ImageReader> {
        self.inner.as_mut().ok_or(BioFormatsError::NotInitialized)
    }

    fn safe_entry_path(name: &str) -> Option<PathBuf> {
        let mut rel = PathBuf::new();
        for component in Path::new(name).components() {
            match component {
                std::path::Component::Normal(part) => rel.push(part),
                std::path::Component::CurDir => {}
                _ => return None,
            }
        }
        (!rel.as_os_str().is_empty()).then_some(rel)
    }

    fn primary_name_matches_base(name: &str, base: &str) -> bool {
        !base.is_empty() && name.starts_with(base)
    }
}

impl Default for ZipReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for ZipReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("zip"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.len() >= 4 && header[0..4] == [0x50, 0x4B, 0x03, 0x04]
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let file = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
        let mut archive = zip::ZipArchive::new(file)
            .map_err(|e| BioFormatsError::Format(format!("ZIP open error: {e}")))?;

        // Per the Java ZipReader, the preferred ("primary") entry is the first
        // entry whose name starts with the archive's base name (file name minus
        // the ".zip" suffix).
        let inner_base = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| {
                let suffix_start = n.len().saturating_sub(4);
                if n.get(suffix_start..)
                    .is_some_and(|suffix| suffix.eq_ignore_ascii_case(".zip"))
                {
                    &n[..suffix_start]
                } else {
                    n
                }
            })
            .unwrap_or("")
            .to_string();

        // Unique temp directory for this archive.
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "bioformats_zip_{}_{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).map_err(BioFormatsError::Io)?;

        let mut extracted_files: Vec<PathBuf> = Vec::new();
        let mut first_entry: Option<PathBuf> = None;
        let mut primary_entry: Option<PathBuf> = None;

        for i in 0..archive.len() {
            let mut entry = archive
                .by_index(i)
                .map_err(|e| BioFormatsError::Format(format!("ZIP entry error: {e}")))?;
            if entry.is_dir() {
                continue;
            }
            let name = entry.name().to_string();
            let rel_path = match Self::safe_entry_path(&name) {
                Some(path) => path,
                None => continue,
            };
            let out_path = dir.join(rel_path);
            if let Some(parent) = out_path.parent() {
                std::fs::create_dir_all(parent).map_err(BioFormatsError::Io)?;
            }

            let mut buf = Vec::new();
            entry.read_to_end(&mut buf).map_err(BioFormatsError::Io)?;
            std::fs::write(&out_path, &buf).map_err(BioFormatsError::Io)?;

            if first_entry.is_none() {
                first_entry = Some(out_path.clone());
            }
            if primary_entry.is_none() && Self::primary_name_matches_base(&name, &inner_base) {
                primary_entry = Some(out_path.clone());
            }
            extracted_files.push(out_path);
        }

        let primary = match primary_entry.or(first_entry) {
            Some(primary) => primary,
            None => {
                let _ = std::fs::remove_dir_all(&dir);
                return Err(BioFormatsError::UnsupportedFormat(
                    "Zip file does not contain any valid files".to_string(),
                ));
            }
        };

        self.extracted_dir = Some(dir);
        self.extracted_files = extracted_files;

        // Java ZipReader delegates exactly the selected entry to ImageReader.
        // It does not fall through to later archive entries if that primary
        // entry is not recognized.
        match ImageReader::open(&primary) {
            Ok(reader) => {
                self.inner = Some(reader);
                Ok(())
            }
            Err(e) => {
                let _ = self.close();
                Err(BioFormatsError::UnsupportedFormat(format!(
                    "Zip primary entry is not a recognized image: {e}"
                )))
            }
        }
    }

    fn close(&mut self) -> Result<()> {
        if let Some(inner) = self.inner.as_mut() {
            let _ = inner.close();
        }
        self.inner = None;
        for f in self.extracted_files.drain(..) {
            let _ = std::fs::remove_file(f);
        }
        if let Some(dir) = self.extracted_dir.take() {
            let _ = std::fs::remove_dir_all(dir);
        }
        Ok(())
    }

    fn series_count(&self) -> usize {
        self.inner().map(|r| r.series_count()).unwrap_or(0)
    }

    fn set_series(&mut self, series: usize) -> Result<()> {
        self.inner_mut()?.set_series(series)
    }

    fn series(&self) -> usize {
        self.inner().map(|r| r.series()).unwrap_or(0)
    }

    fn metadata(&self) -> &ImageMetadata {
        self.inner
            .as_ref()
            .map(|inner| inner.metadata())
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        self.inner_mut()?.open_bytes(plane_index)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        self.inner_mut()?.open_bytes_region(plane_index, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        self.inner_mut()?.open_thumb_bytes(plane_index)
    }
}
