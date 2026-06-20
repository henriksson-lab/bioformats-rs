//! ZIP container reader.
//!
//! Detects ZIP files by magic bytes PK\x03\x04. Following the Java `ZipReader`,
//! this wraps an inner auto-detecting `ImageReader` over the archive entries:
//! all entries are extracted to a temporary directory preserving their safe
//! relative paths, the "primary" entry is selected (the first one whose name
//! starts with the archive's base name, else the first entry), and that entry
//! is delegated to the auto-detecting image reader.

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Seek};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::ImageMetadata;
use crate::common::reader::FormatReader;
use crate::registry::ImageReader;

struct ZipEntryInfo {
    index: usize,
    header_start: u64,
}

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
                std::path::Component::CurDir
                | std::path::Component::RootDir
                | std::path::Component::ParentDir => {}
                _ => {}
            }
        }
        (!rel.as_os_str().is_empty()).then_some(rel)
    }

    fn primary_name_matches_base(name: &str, base: &str) -> bool {
        !base.is_empty() && name.starts_with(base)
    }

    fn extract_entry(
        entry_index: usize,
        entry: &mut zip::read::ZipFile<'_>,
        dir: &Path,
        inner_base: &str,
        extracted_files: &mut Vec<PathBuf>,
        extracted_by_name: &mut HashMap<String, PathBuf>,
        occupied_paths: &mut HashMap<PathBuf, String>,
        first_entry: &mut Option<String>,
        primary_entry: &mut Option<String>,
    ) -> Result<()> {
        let name = entry.name().to_string();
        if first_entry.is_none() {
            *first_entry = Some(name.clone());
        }
        if primary_entry.is_none() && Self::primary_name_matches_base(&name, inner_base) {
            *primary_entry = Some(name.clone());
        }
        if entry.is_dir() {
            return Ok(());
        }
        let rel_path = match Self::safe_entry_path(&name) {
            Some(path) => path,
            None => return Ok(()),
        };
        let mut out_path = dir.join(&rel_path);
        if occupied_paths
            .get(&out_path)
            .is_some_and(|existing_name| existing_name != &name)
        {
            out_path = dir
                .join(format!("__bioformats_zip_entry_{}", entry_index))
                .join(&rel_path);
        }
        occupied_paths
            .entry(out_path.clone())
            .or_insert_with(|| name.clone());
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent).map_err(BioFormatsError::Io)?;
        }

        let mut buf = Vec::new();
        entry.read_to_end(&mut buf).map_err(BioFormatsError::Io)?;
        std::fs::write(&out_path, &buf).map_err(BioFormatsError::Io)?;

        extracted_by_name.insert(name, out_path.clone());
        extracted_files.push(out_path);
        Ok(())
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
        let mut extracted_by_name: HashMap<String, PathBuf> = HashMap::new();
        let mut occupied_paths: HashMap<PathBuf, String> = HashMap::new();
        let mut first_entry: Option<String> = None;
        let mut primary_entry: Option<String> = None;

        let file = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
        match zip::ZipArchive::new(file) {
            Ok(mut archive) => {
                let mut entries = Vec::with_capacity(archive.len());
                for i in 0..archive.len() {
                    let entry = archive
                        .by_index(i)
                        .map_err(|e| BioFormatsError::Format(format!("ZIP entry error: {e}")))?;
                    entries.push(ZipEntryInfo {
                        index: i,
                        header_start: entry.header_start(),
                    });
                }
                entries.sort_by_key(|entry| entry.header_start);

                for (entry_index, info) in entries.into_iter().enumerate() {
                    let mut entry = archive
                        .by_index(info.index)
                        .map_err(|e| BioFormatsError::Format(format!("ZIP entry error: {e}")))?;
                    Self::extract_entry(
                        entry_index,
                        &mut entry,
                        &dir,
                        &inner_base,
                        &mut extracted_files,
                        &mut extracted_by_name,
                        &mut occupied_paths,
                        &mut first_entry,
                        &mut primary_entry,
                    )?;
                }
            }
            Err(_) => {
                let mut archive = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
                let archive_len = archive.metadata().map_err(BioFormatsError::Io)?.len();
                let mut entry_index = 0usize;

                loop {
                    let entry_start = archive.stream_position().map_err(BioFormatsError::Io)?;
                    let mut entry = match zip::read::read_zipfile_from_stream(&mut archive) {
                        Ok(Some(entry)) => entry,
                        Ok(None) => break,
                        Err(zip::result::ZipError::Io(err))
                            if err.kind() == ErrorKind::UnexpectedEof
                                && first_entry.is_some()
                                && entry_start == archive_len =>
                        {
                            break;
                        }
                        Err(e) => {
                            return Err(BioFormatsError::Format(format!("ZIP entry error: {e}")));
                        }
                    };
                    Self::extract_entry(
                        entry_index,
                        &mut entry,
                        &dir,
                        &inner_base,
                        &mut extracted_files,
                        &mut extracted_by_name,
                        &mut occupied_paths,
                        &mut first_entry,
                        &mut primary_entry,
                    )?;
                    entry_index += 1;
                }
            }
        }

        let primary_name = match primary_entry.or(first_entry) {
            Some(primary) => primary,
            None => {
                let _ = std::fs::remove_dir_all(&dir);
                return Err(BioFormatsError::UnsupportedFormat(
                    "Zip file does not contain any valid files".to_string(),
                ));
            }
        };
        let primary = match extracted_by_name.get(&primary_name).cloned() {
            Some(primary) => primary,
            None => {
                let _ = std::fs::remove_dir_all(&dir);
                return Err(BioFormatsError::UnsupportedFormat(
                    "Zip primary entry is not a recognized image".to_string(),
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
