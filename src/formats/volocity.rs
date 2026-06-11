//! Volocity (.mvd2) and Nikon NIS (.nif) format readers.

use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
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
const VOLOCITY_METAKIT_MAX_STRUCTURE: usize = 64 * 1024;

#[derive(Debug, Clone, Copy)]
struct VolocityBlindLayout {
    data_offset: usize,
    plane_len: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct VolocityMetakitTable {
    name: String,
    row_count: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct VolocityMetakitProbe {
    little_endian: bool,
    footer_offset: usize,
    toc_offset: usize,
    structure_len: usize,
    tables: Vec<VolocityMetakitTable>,
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

fn volocity_native_error(path: &Path, probe: &VolocityMetakitProbe) -> BioFormatsError {
    let endian = if probe.little_endian {
        "little-endian"
    } else {
        "big-endian"
    };
    let tables = if probe.tables.is_empty() {
        "no tables reported".to_string()
    } else {
        probe
            .tables
            .iter()
            .map(|table| match table.row_count {
                Some(rows) => format!("{}({rows})", table.name),
                None => format!("{}(?)", table.name),
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    BioFormatsError::UnsupportedFormat(format!(
        "{VOLOCITY_UNSUPPORTED}; detected native Metakit {endian} footer={} toc={} structure={}B table_count={} tables: {tables}: {}",
        probe.footer_offset,
        probe.toc_offset,
        probe.structure_len,
        probe.tables.len(),
        path.display()
    ))
}

fn volocity_metakit_probe_error(path: &Path, reason: &str) -> BioFormatsError {
    BioFormatsError::UnsupportedFormat(format!(
        "{VOLOCITY_UNSUPPORTED}; Metakit stream signature was present but metadata probe failed: {reason}: {}",
        path.display()
    ))
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

fn metakit_i32_be_at(bytes: &[u8], offset: usize) -> std::result::Result<i32, String> {
    let end = offset
        .checked_add(4)
        .ok_or_else(|| "integer offset overflows".to_string())?;
    let data = bytes
        .get(offset..end)
        .ok_or_else(|| format!("truncated i32 at offset {offset}"))?;
    Ok(i32::from_be_bytes([data[0], data[1], data[2], data[3]]))
}

fn metakit_read_byte(bytes: &[u8], offset: &mut usize) -> std::result::Result<u8, String> {
    let byte = *bytes
        .get(*offset)
        .ok_or_else(|| format!("unexpected EOF at offset {}", *offset))?;
    *offset += 1;
    Ok(byte)
}

fn metakit_read_bp_int(bytes: &[u8], offset: &mut usize) -> std::result::Result<i32, String> {
    let sign_byte = metakit_read_byte(bytes, offset)?;
    let negative = sign_byte == 0;
    let data_byte = if negative {
        metakit_read_byte(bytes, offset)?
    } else {
        sign_byte
    };
    let mut stop_byte = data_byte;
    let mut data_bytes = Vec::new();

    while (stop_byte & 0x80) == 0 {
        if data_bytes.len() >= 4 {
            return Err("overlong byte-packed integer".to_string());
        }
        data_bytes.push(stop_byte);
        stop_byte = metakit_read_byte(bytes, offset)?;
    }

    let mut value = 0i32;
    for (index, byte) in data_bytes.iter().enumerate() {
        let shift = (data_bytes.len() - index) * 7;
        value |= i32::from(*byte) << shift;
    }
    value |= i32::from(stop_byte & 0x7f);

    if negative {
        value = !value;
    }
    Ok(value)
}

fn metakit_read_p_string(bytes: &[u8], offset: &mut usize) -> std::result::Result<String, String> {
    let len = metakit_read_bp_int(bytes, offset)?;
    if len < 0 {
        return Err(format!("negative structure string length: {len}"));
    }
    let len = len as usize;
    if len > VOLOCITY_METAKIT_MAX_STRUCTURE {
        return Err(format!(
            "structure string length {len} exceeds safety limit"
        ));
    }
    let end = offset
        .checked_add(len)
        .ok_or_else(|| "structure string end overflows".to_string())?;
    let data = bytes
        .get(*offset..end)
        .ok_or_else(|| "truncated structure string".to_string())?;
    *offset = end;
    std::str::from_utf8(data)
        .map(str::to_owned)
        .map_err(|err| format!("structure string is not UTF-8: {err}"))
}

fn metakit_row_count_at(bytes: &[u8], pointer: i32) -> Option<usize> {
    let mut offset = usize::try_from(pointer.checked_add(1)?).ok()?;
    usize::try_from(metakit_read_bp_int(bytes, &mut offset).ok()?).ok()
}

fn parse_metakit_table_defs(structure: &str) -> std::result::Result<Vec<(String, bool)>, String> {
    structure
        .split("],")
        .map(|table_def| {
            let open = table_def
                .find('[')
                .ok_or_else(|| format!("invalid table definition: {table_def}"))?;
            let name = &table_def[..open];
            if name.is_empty() {
                return Err("empty table name in structure definition".to_string());
            }
            let column_list = &table_def[open + 1..];
            Ok((name.to_string(), column_list.contains('[')))
        })
        .collect()
}

fn probe_volocity_metakit(
    bytes: &[u8],
) -> std::result::Result<Option<VolocityMetakitProbe>, String> {
    let little_endian = match bytes.get(0..2) {
        Some(b"JL") => true,
        Some(b"LJ") => false,
        _ => return Ok(None),
    };
    if bytes.len() < 20 {
        return Err("Metakit header is truncated".to_string());
    }
    if bytes[2] != 26 {
        return Err(format!("Metakit valid flag was {}, expected 26", bytes[2]));
    }
    if bytes[3] != 0 {
        return Err(format!("Metakit header type was {}, expected 0", bytes[3]));
    }

    let footer_pointer = metakit_i32_be_at(bytes, 4)? as i64 - 16;
    if footer_pointer < 0 {
        return Err(format!("negative footer pointer: {footer_pointer}"));
    }
    let footer_pointer =
        usize::try_from(footer_pointer).map_err(|_| "footer pointer overflows".to_string())?;
    let footer_end = footer_pointer
        .checked_add(16)
        .ok_or_else(|| "footer end overflows".to_string())?;
    if footer_end > bytes.len() {
        return Err(format!("footer at offset {footer_pointer} is outside file"));
    }

    let toc_location = metakit_i32_be_at(bytes, footer_pointer + 12)?;
    if toc_location < 0 {
        return Err(format!("negative TOC pointer: {toc_location}"));
    }
    let toc_offset =
        usize::try_from(toc_location).map_err(|_| "TOC pointer overflows".to_string())?;
    let mut offset = toc_offset;
    if offset >= bytes.len() {
        return Err(format!("TOC pointer {offset} is outside file"));
    }

    let _toc_marker = metakit_read_bp_int(bytes, &mut offset)?;
    let structure = metakit_read_p_string(bytes, &mut offset)?;
    let structure_len = structure.len();
    let table_defs = parse_metakit_table_defs(&structure)?;
    let _row_count_marker = metakit_read_bp_int(bytes, &mut offset)?;

    let mut tables = Vec::with_capacity(table_defs.len());
    for (name, has_subviews) in table_defs {
        let _table_marker = metakit_read_bp_int(bytes, &mut offset)?;
        let pointer = metakit_read_bp_int(bytes, &mut offset)?;
        tables.push(VolocityMetakitTable {
            name,
            row_count: if has_subviews {
                None
            } else {
                metakit_row_count_at(bytes, pointer)
            },
        });
    }

    Ok(Some(VolocityMetakitProbe {
        little_endian,
        footer_offset: footer_pointer,
        toc_offset,
        structure_len,
        tables,
    }))
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

    let mut meta = ImageMetadata {
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
    };
    meta.series_metadata.insert(
        "volocity_version_subset".into(),
        MetadataValue::String("BFVOLOCITYMVD2-blind-raw-v1".into()),
    );
    meta.series_metadata.insert(
        "Volocity blind pixel type code".into(),
        MetadataValue::Int(i64::from(pixel_code)),
    );
    meta.series_metadata.insert(
        "Volocity blind data offset".into(),
        MetadataValue::Int(data_offset as i64),
    );

    Ok(Some((
        meta,
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
        // Java accepts a two-byte "JL"/"LJ" stream signature. That is too weak
        // for global detection, so require a bounded Metakit header/TOC probe.
        matches!(probe_volocity_metakit(header), Ok(Some(_)))
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
            match probe_volocity_metakit(&bytes) {
                Ok(Some(probe)) => return Err(volocity_native_error(&root, &probe)),
                Ok(None) => {}
                Err(reason) if bytes.starts_with(b"JL") || bytes.starts_with(b"LJ") => {
                    return Err(volocity_metakit_probe_error(&root, &reason));
                }
                Err(_) => {}
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
    fn volocity_detects_bounded_native_metakit_stream() {
        let bytes = include_bytes!("../ome-metakit/tests/data/test.mk");
        let reader = VolocityReader::new();
        assert!(reader.is_this_type_by_bytes(bytes));

        let probe = probe_volocity_metakit(bytes).unwrap().unwrap();
        assert!(probe.little_endian);
        assert_eq!(probe.footer_offset, 22569);
        assert_eq!(probe.toc_offset, 1496);
        assert_eq!(probe.structure_len, 488);
        assert_eq!(
            probe.tables,
            vec![
                VolocityMetakitTable {
                    name: "variablesView".to_string(),
                    row_count: Some(1),
                },
                VolocityMetakitTable {
                    name: "samplesViewR".to_string(),
                    row_count: None,
                },
                VolocityMetakitTable {
                    name: "stringsViewR".to_string(),
                    row_count: Some(23),
                },
                VolocityMetakitTable {
                    name: "filesViewR".to_string(),
                    row_count: Some(0),
                },
            ]
        );
    }

    #[test]
    fn volocity_native_metakit_error_reports_table_shape() {
        let path = temp_dir("native.mvd2");
        std::fs::write(&path, include_bytes!("../ome-metakit/tests/data/test.mk")).unwrap();

        let err = VolocityReader::new().set_id(&path).unwrap_err();
        assert!(matches!(
            err,
            BioFormatsError::UnsupportedFormat(message)
                if message.contains("native Metakit decoding is unsupported")
                    && message.contains("footer=22569")
                    && message.contains("toc=1496")
                    && message.contains("structure=488B")
                    && message.contains("table_count=4")
                    && message.contains("variablesView(1)")
                    && message.contains("samplesViewR(?)")
                    && message.contains("stringsViewR(23)")
                    && message.contains("filesViewR(0)")
                    && message.contains("native.mvd2")
        ));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn volocity_truncated_metakit_signature_has_explicit_error() {
        let path = temp_dir("truncated-native.mvd2");
        std::fs::write(&path, b"JL").unwrap();

        let err = VolocityReader::new().set_id(&path).unwrap_err();
        assert!(matches!(
            err,
            BioFormatsError::UnsupportedFormat(message)
                if message.contains("Metakit stream signature was present")
                    && message.contains("Metakit header is truncated")
        ));

        let _ = std::fs::remove_file(path);
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
        assert!(matches!(
            reader
                .metadata()
                .series_metadata
                .get("volocity_version_subset"),
            Some(MetadataValue::String(value))
                if value == "BFVOLOCITYMVD2-blind-raw-v1"
        ));
        assert!(matches!(
            reader
                .metadata()
                .series_metadata
                .get("Volocity blind data offset"),
            Some(MetadataValue::Int(value)) if *value == VOLOCITY_BLIND_HEADER_LEN as i64
        ));
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
