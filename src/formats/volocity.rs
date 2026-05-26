//! Volocity (.mvd2) and Nikon NIS (.nif) format readers.

use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::ImageMetadata;
use crate::common::reader::FormatReader;

// --- Volocity .mvd2 -----------------------------------------------------------
//
// Volocity (PerkinElmer) stores 3D/4D microscopy data in .mvd2 files.
// Java Bio-Formats treats .mvd2 as the library root and .aisf/.aiix/.dat/.atsf
// files as companion files below the library's Data tree. The actual .mvd2
// metadata tables are Metakit-backed, so this reader translates the Java
// detection and companion routing contract but keeps pixel parsing unsupported.

const VOLOCITY_UNSUPPORTED: &str = "Volocity MVD2 format reading is not yet implemented; parsing requires the Java Bio-Formats Metakit-backed library reader";
const VOLOCITY_SUFFIXES: &[&str] = &["mvd2", "aisf", "aiix", "dat", "atsf"];

fn ext_lower(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
}

fn is_volocity_companion_suffix(ext: Option<&str>) -> bool {
    matches!(ext, Some(suffix) if VOLOCITY_SUFFIXES[1..].contains(&suffix))
}

fn volocity_library_from_companion(path: &Path) -> Option<PathBuf> {
    // Java VolocityReader walks three parents from a companion file and then
    // expects "<library>/<library>.mvd2".
    let library_dir = path.parent()?.parent()?.parent()?;
    let library_name = library_dir.file_name()?;
    let candidate = library_dir.join(format!("{}.mvd2", library_name.to_string_lossy()));
    candidate.exists().then_some(candidate)
}

fn volocity_error(path: Option<&Path>) -> BioFormatsError {
    let detail = match path {
        Some(path) => format!("{VOLOCITY_UNSUPPORTED}: {}", path.display()),
        None => VOLOCITY_UNSUPPORTED.to_string(),
    };
    BioFormatsError::UnsupportedFormat(detail)
}

pub struct VolocityReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl VolocityReader {
    pub fn new() -> Self {
        VolocityReader {
            path: None,
            meta: None,
        }
    }
}
impl Default for VolocityReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for VolocityReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = ext_lower(path);
        match ext.as_deref() {
            Some("mvd2") => true,
            suffix if is_volocity_companion_suffix(suffix) => {
                volocity_library_from_companion(path).is_some()
            }
            _ => false,
        }
    }
    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.len() >= 2 && (&header[..2] == b"JL" || &header[..2] == b"LJ")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let root = if ext_lower(path).as_deref() == Some("mvd2") {
            path.to_path_buf()
        } else {
            volocity_library_from_companion(path).unwrap_or_else(|| path.to_path_buf())
        };
        Err(volocity_error(Some(&root)))
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        Ok(())
    }
    fn series_count(&self) -> usize {
        1
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if s != 0 {
            Err(BioFormatsError::SeriesOutOfRange(s))
        } else {
            Ok(())
        }
    }
    fn series(&self) -> usize {
        0
    }
    fn metadata(&self) -> &ImageMetadata {
        self.meta.as_ref().expect("set_id not called")
    }
    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        let _ = p;
        Err(volocity_error(self.path.as_deref()))
    }
    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let _ = (p, x, y, w, h);
        Err(volocity_error(self.path.as_deref()))
    }
    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        let _ = p;
        Err(volocity_error(self.path.as_deref()))
    }
}

// --- Nikon NIS-Elements .nif --------------------------------------------------
//
// Nikon NIS-Elements Image File (.nif) — TIFF-based format.
// Delegates to TiffReader for pixel data.

pub struct NikonNisReader {
    inner: crate::tiff::TiffReader,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("bioformats_volocity_{nanos}_{name}"))
    }

    #[test]
    fn volocity_matches_java_stream_signature() {
        let reader = VolocityReader::new();
        assert!(reader.is_this_type_by_bytes(b"JLabcdef"));
        assert!(reader.is_this_type_by_bytes(b"LJabcdef"));
        assert!(!reader.is_this_type_by_bytes(b"JXabcdef"));
        assert!(!reader.is_this_type_by_bytes(b"J"));
    }

    #[test]
    fn volocity_companion_detection_requires_owning_mvd2() {
        let root = temp_dir("companion");
        let library = root.join("Library");
        let stack_dir = library.join("Data").join("Stack");
        std::fs::create_dir_all(&stack_dir).unwrap();
        let companion = stack_dir.join("1.aisf");
        std::fs::write(&companion, b"JL").unwrap();

        let reader = VolocityReader::new();
        assert!(!reader.is_this_type_by_name(&companion));

        let mvd2 = library.join("Library.mvd2");
        std::fs::write(&mvd2, b"JL").unwrap();
        assert!(reader.is_this_type_by_name(&mvd2));
        assert!(reader.is_this_type_by_name(&companion));

        let err = VolocityReader::new().set_id(&companion).unwrap_err();
        assert!(matches!(
            err,
            BioFormatsError::UnsupportedFormat(message)
                if message.contains("Metakit-backed")
                    && message.contains("Library.mvd2")
        ));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn volocity_suffix_contract_matches_java_reader() {
        for suffix in VOLOCITY_SUFFIXES {
            assert!(!suffix.is_empty());
        }
        let reader = VolocityReader::new();
        assert!(reader.is_this_type_by_name(Path::new("sample.mvd2")));
        assert!(!reader.is_this_type_by_name(Path::new("orphan.aisf")));
    }
}

impl NikonNisReader {
    pub fn new() -> Self {
        NikonNisReader {
            inner: crate::tiff::TiffReader::new(),
        }
    }
}
impl Default for NikonNisReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for NikonNisReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("nif") | Some("nd2") )
        // .nd2 is already handled by bioformats-nd2, so effectively only .nif here
        && matches!(ext.as_deref(), Some("nif"))
    }
    fn is_this_type_by_bytes(&self, _: &[u8]) -> bool {
        false
    }
    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.inner.set_id(path)
    }
    fn close(&mut self) -> Result<()> {
        self.inner.close()
    }
    fn series_count(&self) -> usize {
        self.inner.series_count()
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        self.inner.set_series(s)
    }
    fn series(&self) -> usize {
        self.inner.series()
    }
    fn metadata(&self) -> &ImageMetadata {
        self.inner.metadata()
    }
    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        self.inner.open_bytes(p)
    }
    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        self.inner.open_bytes_region(p, x, y, w, h)
    }
    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        self.inner.open_thumb_bytes(p)
    }
    fn resolution_count(&self) -> usize {
        self.inner.resolution_count()
    }
    fn set_resolution(&mut self, l: usize) -> Result<()> {
        self.inner.set_resolution(l)
    }
    fn resolution(&self) -> usize {
        self.inner.resolution()
    }
}
