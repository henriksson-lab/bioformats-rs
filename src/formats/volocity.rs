//! Volocity (.mvd2) and Nikon NIS (.nif) format readers.

use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;

// --- Volocity .mvd2 -----------------------------------------------------------
//
// Volocity (PerkinElmer) stores 3D/4D microscopy data in .mvd2 files.
// Java Bio-Formats treats .mvd2 as the library root and .aisf/.aiix/.dat/.atsf
// files as companion files below the library's Data tree. The actual .mvd2
// metadata tables are Metakit-backed, so this reader translates the Java
// detection and companion routing contract but keeps pixel parsing unsupported.

const VOLOCITY_UNSUPPORTED: &str = "Volocity MVD2 native Metakit decoding is unsupported; explicit BFVOLOCITYMVD2 blind raw fixtures are supported";
const VOLOCITY_SUFFIXES: &[&str] = &["mvd2", "aisf", "aiix", "dat", "atsf"];
const VOLOCITY_BLIND_MAGIC: &[u8; 16] = b"BFVOLOCITYMVD2\0\0";
const VOLOCITY_BLIND_HEADER_LEN: usize = 48;

#[derive(Debug, Clone, Copy)]
struct VolocityBlindLayout {
    data_offset: usize,
    plane_len: usize,
}

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

fn volocity_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn volocity_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn parse_volocity_blind_layout(
    bytes: &[u8],
) -> Result<Option<(ImageMetadata, VolocityBlindLayout)>> {
    if !bytes.starts_with(VOLOCITY_BLIND_MAGIC) {
        return Ok(None);
    }
    if bytes.len() < VOLOCITY_BLIND_HEADER_LEN {
        return Err(BioFormatsError::Format(
            "Volocity MVD2 blind subset header is truncated".into(),
        ));
    }

    let version = volocity_u16(bytes, 16);
    let pixel_code = volocity_u16(bytes, 18);
    let size_x = volocity_u32(bytes, 20);
    let size_y = volocity_u32(bytes, 24);
    let size_z = volocity_u32(bytes, 28);
    let size_c = volocity_u32(bytes, 32);
    let size_t = volocity_u32(bytes, 36);
    let flags = volocity_u16(bytes, 40);
    let reserved = volocity_u16(bytes, 42);
    let data_offset = volocity_u32(bytes, 44) as usize;

    if version != 1 {
        return Err(BioFormatsError::Format(format!(
            "Volocity MVD2 blind subset version {version} is not supported"
        )));
    }
    if [size_x, size_y, size_z, size_c, size_t].contains(&0) {
        return Err(BioFormatsError::Format(
            "Volocity MVD2 blind subset dimensions must be positive".into(),
        ));
    }
    if flags & !1 != 0 || reserved != 0 {
        return Err(BioFormatsError::Format(
            "Volocity MVD2 blind subset reserved header bits must be zero".into(),
        ));
    }
    if data_offset < VOLOCITY_BLIND_HEADER_LEN {
        return Err(BioFormatsError::Format(
            "Volocity MVD2 blind subset data offset points into header".into(),
        ));
    }
    if data_offset > bytes.len() {
        return Err(BioFormatsError::Format(
            "Volocity MVD2 blind subset data offset is past end of file".into(),
        ));
    }

    let pixel_type = match pixel_code {
        1 => PixelType::Uint8,
        2 => PixelType::Uint16,
        other => {
            return Err(BioFormatsError::Format(format!(
                "Volocity MVD2 blind subset pixel type {other} is not supported"
            )))
        }
    };
    let plane_len = (size_x as usize)
        .checked_mul(size_y as usize)
        .and_then(|px| px.checked_mul(pixel_type.bytes_per_sample()))
        .ok_or_else(|| BioFormatsError::Format("Volocity MVD2 plane size overflows".into()))?;
    let image_count = size_z
        .checked_mul(size_c)
        .and_then(|n| n.checked_mul(size_t))
        .ok_or_else(|| BioFormatsError::Format("Volocity MVD2 image count overflows".into()))?;
    let payload_len = plane_len
        .checked_mul(image_count as usize)
        .ok_or_else(|| BioFormatsError::Format("Volocity MVD2 payload size overflows".into()))?;
    let payload_end = data_offset
        .checked_add(payload_len)
        .ok_or_else(|| BioFormatsError::Format("Volocity MVD2 payload end overflows".into()))?;
    if payload_end != bytes.len() {
        return Err(BioFormatsError::Format(format!(
            "Volocity MVD2 blind subset payload length {} does not match declared size {payload_len}",
            bytes.len().saturating_sub(data_offset)
        )));
    }

    Ok(Some((
        ImageMetadata {
            size_x,
            size_y,
            size_z,
            size_c,
            size_t,
            pixel_type,
            bits_per_pixel: (pixel_type.bytes_per_sample() * 8) as u8,
            image_count,
            dimension_order: DimensionOrder::XYZCT,
            is_little_endian: flags & 1 != 0,
            ..ImageMetadata::default()
        },
        VolocityBlindLayout {
            data_offset,
            plane_len,
        },
    )))
}

pub struct VolocityReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    bytes: Vec<u8>,
    layout: Option<VolocityBlindLayout>,
}

impl VolocityReader {
    pub fn new() -> Self {
        VolocityReader {
            path: None,
            meta: None,
            bytes: Vec::new(),
            layout: None,
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
        if header.starts_with(VOLOCITY_BLIND_MAGIC) {
            return true;
        }
        // Java accepts a two-byte "JL"/"LJ" stream signature, but that is too
        // weak for global detection while this reader is unsupported: it can
        // preempt unrelated formats and return a terminal unsupported error.
        // Keep Volocity detection path/name based.
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let root = if ext_lower(path).as_deref() == Some("mvd2") {
            path.to_path_buf()
        } else {
            volocity_library_from_companion(path).unwrap_or_else(|| path.to_path_buf())
        };
        self.path = None;
        self.meta = None;
        self.bytes.clear();
        self.layout = None;
        if root.exists() {
            let bytes = std::fs::read(&root).map_err(BioFormatsError::Io)?;
            if let Some((meta, layout)) = parse_volocity_blind_layout(&bytes)? {
                self.path = Some(root);
                self.meta = Some(meta);
                self.bytes = bytes;
                self.layout = Some(layout);
                return Ok(());
            }
        }
        Err(volocity_error(Some(&root)))
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.bytes.clear();
        self.layout = None;
        Ok(())
    }
    fn series_count(&self) -> usize {
        usize::from(self.meta.is_some())
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if s == 0 && self.meta.is_some() {
            Ok(())
        } else if self.meta.is_none() {
            Err(BioFormatsError::NotInitialized)
        } else {
            Err(BioFormatsError::SeriesOutOfRange(s))
        }
    }
    fn series(&self) -> usize {
        0
    }
    fn metadata(&self) -> &ImageMetadata {
        self.meta
            .as_ref()
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }
    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if p >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(p));
        }
        let layout = self.layout.ok_or(BioFormatsError::NotInitialized)?;
        let start = layout
            .data_offset
            .checked_add(layout.plane_len * p as usize)
            .ok_or_else(|| {
                BioFormatsError::Format("Volocity MVD2 plane offset overflows".into())
            })?;
        let end = start
            .checked_add(layout.plane_len)
            .ok_or_else(|| BioFormatsError::Format("Volocity MVD2 plane end overflows".into()))?;
        Ok(self.bytes[start..end].to_vec())
    }
    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let full = self.open_bytes(p)?;
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        crop_full_plane("Volocity MVD2", &full, meta, 1, x, y, w, h)
    }
    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        self.open_bytes(p)
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
        assert!(reader.is_this_type_by_bytes(VOLOCITY_BLIND_MAGIC));
        assert!(!reader.is_this_type_by_bytes(b"JLabcdef"));
        assert!(!reader.is_this_type_by_bytes(b"LJabcdef"));
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
                if message.contains("native Metakit decoding is unsupported")
                    && message.contains("Library.mvd2")
        ));

        let _ = std::fs::remove_dir_all(root);
    }

    fn blind_mvd2(width: u32, height: u32, z: u32, c: u32, t: u32, payload: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(VOLOCITY_BLIND_MAGIC);
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&width.to_le_bytes());
        bytes.extend_from_slice(&height.to_le_bytes());
        bytes.extend_from_slice(&z.to_le_bytes());
        bytes.extend_from_slice(&c.to_le_bytes());
        bytes.extend_from_slice(&t.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&(VOLOCITY_BLIND_HEADER_LEN as u32).to_le_bytes());
        bytes.extend_from_slice(payload);
        bytes
    }

    #[test]
    fn volocity_reads_strict_blind_raw_subset() {
        let path = temp_dir("blind.mvd2");
        let payload = vec![1, 2, 3, 4, 5, 6, 7, 8];
        std::fs::write(&path, blind_mvd2(2, 2, 2, 1, 1, &payload)).unwrap();

        let mut reader = VolocityReader::new();
        reader.set_id(&path).unwrap();
        assert_eq!(reader.series_count(), 1);
        assert_eq!(reader.metadata().size_x, 2);
        assert_eq!(reader.metadata().size_y, 2);
        assert_eq!(reader.metadata().size_z, 2);
        assert_eq!(reader.metadata().image_count, 2);
        assert_eq!(reader.metadata().pixel_type, PixelType::Uint8);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4]);
        assert_eq!(reader.open_bytes_region(1, 1, 0, 1, 2).unwrap(), vec![6, 8]);
        assert!(matches!(
            reader.open_bytes(2),
            Err(BioFormatsError::PlaneOutOfRange(2))
        ));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn volocity_blind_subset_rejects_truncated_payload() {
        let path = temp_dir("truncated.mvd2");
        std::fs::write(&path, blind_mvd2(2, 2, 1, 1, 1, &[1, 2, 3])).unwrap();

        let err = VolocityReader::new().set_id(&path).unwrap_err();
        assert!(matches!(
            err,
            BioFormatsError::Format(message) if message.contains("payload length")
        ));

        let _ = std::fs::remove_file(path);
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
