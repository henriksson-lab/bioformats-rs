use std::fs::File;
use std::io::Read;
use std::path::Path;

use crate::common::error::{BioFormatsError, Result};

/// POIService-like wrapper around the `cfb` crate for OLE2/Compound File Binary
/// containers.
pub struct OleFile {
    inner: cfb::CompoundFile<File>,
}

impl OleFile {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let inner = cfb::open(path).map_err(|e| {
            BioFormatsError::Format(format!("OLE2 CFB open {}: {e}", path.display()))
        })?;
        Ok(Self { inner })
    }

    pub fn document_list(&self) -> Vec<String> {
        self.inner
            .walk()
            .filter(|entry| entry.is_stream())
            .map(|entry| poi_style_path(entry.path()))
            .collect()
    }

    pub fn file_size(&self, path: &str) -> Option<u64> {
        candidate_paths(path)
            .into_iter()
            .find_map(|candidate| self.inner.entry(candidate).ok().map(|entry| entry.len()))
    }

    pub fn open_document_stream(&mut self, path: &str) -> Result<cfb::Stream<File>> {
        self.open_stream_with_root_fallback(path)
            .map_err(|e| BioFormatsError::Format(format!("OLE2 stream {path}: {e}")))
    }

    pub fn document_bytes(&mut self, path: &str) -> Result<Vec<u8>> {
        let mut stream = self.open_document_stream(path)?;
        let mut bytes = Vec::new();
        stream
            .read_to_end(&mut bytes)
            .map_err(BioFormatsError::Io)?;
        Ok(bytes)
    }

    pub fn document_bytes_limit(&mut self, path: &str, length: usize) -> Result<Vec<u8>> {
        let mut stream = self.open_document_stream(path)?;
        let mut bytes = Vec::new();
        stream
            .by_ref()
            .take(length as u64)
            .read_to_end(&mut bytes)
            .map_err(BioFormatsError::Io)?;
        Ok(bytes)
    }

    fn open_stream_with_root_fallback(&mut self, path: &str) -> std::io::Result<cfb::Stream<File>> {
        let mut last_error = None;
        for candidate in candidate_paths(path) {
            match self.inner.open_stream(&candidate) {
                Ok(stream) => return Ok(stream),
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error.unwrap_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "empty OLE2 stream path")
        }))
    }
}

pub fn is_ole2_header(header: &[u8]) -> bool {
    header.len() >= 8 && header[..8] == [0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1]
}

fn poi_style_path(path: &Path) -> String {
    let normalized = path.to_string_lossy().replace('\\', "/");
    if normalized.starts_with("/Root Entry/") {
        normalized.trim_start_matches('/').to_string()
    } else if normalized.starts_with('/') {
        format!("Root Entry{normalized}")
    } else if normalized.starts_with("Root Entry/") {
        normalized
    } else {
        format!("Root Entry/{normalized}")
    }
}

pub fn cfb_path_without_root(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    let trimmed = normalized.trim_start_matches('/');
    trimmed
        .strip_prefix("Root Entry/")
        .unwrap_or(trimmed)
        .to_string()
}

fn candidate_paths(path: &str) -> Vec<String> {
    let normalized = path.replace('\\', "/");
    let trimmed = normalized.trim_start_matches('/');
    let without_root = trimmed.strip_prefix("Root Entry/").unwrap_or(trimmed);
    let mut paths = Vec::new();
    push_unique(&mut paths, normalized.clone());
    push_unique(&mut paths, format!("/{without_root}"));
    push_unique(&mut paths, without_root.to_string());
    push_unique(&mut paths, format!("/Root Entry/{without_root}"));
    push_unique(&mut paths, format!("Root Entry/{without_root}"));
    paths
}

fn push_unique(paths: &mut Vec<String>, path: String) {
    if !path.is_empty() && !paths.iter().any(|existing| existing == &path) {
        paths.push(path);
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    #[test]
    fn ole_file_exposes_poi_service_like_document_access() {
        let path =
            std::env::temp_dir().join(format!("bioformats_ole_adapter_{}.cfb", std::process::id()));
        {
            let mut comp = cfb::create(&path).unwrap();
            comp.create_storage("/Image").unwrap();
            comp.create_storage("/Image/Item(1)").unwrap();
            comp.create_stream("/Image/Item(1)/CONTENTS")
                .unwrap()
                .write_all(b"abcdef")
                .unwrap();
        }

        let mut ole = OleFile::open(&path).unwrap();
        let docs = ole.document_list();
        assert!(docs
            .iter()
            .any(|doc| doc == "Root Entry/Image/Item(1)/CONTENTS"));
        assert_eq!(ole.file_size("/Image/Item(1)/CONTENTS"), Some(6));
        assert_eq!(ole.file_size("Root Entry/Image/Item(1)/CONTENTS"), Some(6));
        assert_eq!(
            ole.document_bytes("/Image/Item(1)/CONTENTS").unwrap(),
            b"abcdef"
        );
        assert_eq!(
            ole.document_bytes_limit("Root Entry/Image/Item(1)/CONTENTS", 3)
                .unwrap(),
            b"abc"
        );
        assert_eq!(
            cfb_path_without_root("\\Image\\Item(1)\\CONTENTS"),
            "Image/Item(1)/CONTENTS"
        );
        std::fs::remove_file(path).ok();
    }
}
