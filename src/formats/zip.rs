//! ZIP container reader.
//!
//! Detects ZIP files by magic bytes PK\x03\x04. Following the Java `ZipReader`,
//! this wraps an inner auto-detecting `ImageReader` over the archive entries:
//! all entries are extracted to a temporary directory, the "primary" entry is
//! selected (the one whose name starts with the archive's base name, else the
//! first entry), and the matching format reader is chosen by auto-detection
//! (not restricted to TIFF).

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
        let file = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
        let mut archive = zip::ZipArchive::new(file)
            .map_err(|e| BioFormatsError::Format(format!("ZIP open error: {e}")))?;

        // Per the Java ZipReader, the preferred ("primary") entry is the one
        // whose name begins with the archive's base name (file name minus the
        // ".zip" suffix); otherwise the first regular entry is used.
        let inner_base = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.strip_suffix(".zip").or_else(|| n.strip_suffix(".ZIP")).unwrap_or(n))
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
            // Flatten the entry name to a safe leaf file name, but keep enough
            // of the original to match the primary-entry rule.
            let leaf = Path::new(&name)
                .file_name()
                .unwrap_or_else(|| std::ffi::OsStr::new("entry"))
                .to_string_lossy()
                .to_string();
            let out_path = dir.join(format!("{i}_{leaf}"));

            let mut buf = Vec::new();
            entry.read_to_end(&mut buf).map_err(BioFormatsError::Io)?;
            std::fs::write(&out_path, &buf).map_err(BioFormatsError::Io)?;

            if first_entry.is_none() {
                first_entry = Some(out_path.clone());
            }
            if primary_entry.is_none() && !inner_base.is_empty() && name.starts_with(&inner_base) {
                primary_entry = Some(out_path.clone());
            }
            extracted_files.push(out_path);
        }

        let primary = primary_entry.or(first_entry).ok_or_else(|| {
            BioFormatsError::UnsupportedFormat(
                "Zip file does not contain any valid files".to_string(),
            )
        })?;

        self.extracted_dir = Some(dir);
        self.extracted_files = extracted_files;

        // Delegate to the auto-detecting ImageReader, which picks the matching
        // format reader for the primary entry (TIFF, PNG, JPEG, ND2, ...).
        match ImageReader::open(&primary) {
            Ok(reader) => {
                self.inner = Some(reader);
                Ok(())
            }
            Err(e) => {
                // Clean up extracted files on failure.
                let _ = self.close();
                Err(e)
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
        self.inner().map(|r| r.series_count()).unwrap_or(1)
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
            .expect("set_id not called")
            .metadata()
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
