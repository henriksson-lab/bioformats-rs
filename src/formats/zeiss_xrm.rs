//! Zeiss XRM/TXRM X-ray tomography format reader.
//!
//! Bio-Formats' Java `ZeissXRMReader` reads these files as CFB/OLE2 compound
//! documents.  This Rust reader implements the same bounded core path:
//! dimensions and datatype from `Root Entry/ImageInfo/*`, and uncompressed
//! plane streams from `Root Entry/ImageData/ImageN`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::ole::{cfb_path_without_root, OleFile};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;

const IMAGE_DATA: &str = "/ImageData/";

pub struct ZeissXrmReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    image_paths: Vec<String>,
}

impl ZeissXrmReader {
    pub fn new() -> Self {
        ZeissXrmReader {
            path: None,
            meta: None,
            image_paths: Vec::new(),
        }
    }
}

impl Default for ZeissXrmReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for ZeissXrmReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("xrm") | Some("txrm") | Some("txm"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let (meta, image_paths) = parse_xrm(path)?;
        self.path = Some(path.to_path_buf());
        self.meta = Some(meta);
        self.image_paths = image_paths;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.image_paths.clear();
        Ok(())
    }

    fn series_count(&self) -> usize {
        usize::from(self.meta.is_some())
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.meta.is_none() || s != 0 {
            Err(BioFormatsError::SeriesOutOfRange(s))
        } else {
            Ok(())
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

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let stream_path = self
            .image_paths
            .get(plane_index as usize)
            .ok_or(BioFormatsError::PlaneOutOfRange(plane_index))?
            .clone();
        let path = self
            .path
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?
            .clone();

        let mut ole = OleFile::open(&path)?;
        let raw = ole.document_bytes(&stream_path)?;

        xrm_flip_rows(&raw, meta)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let full = self.open_bytes(plane_index)?;
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        crop_full_plane("XRM", &full, meta, 1, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}

fn parse_xrm(path: &Path) -> Result<(ImageMetadata, Vec<String>)> {
    let mut ole = OleFile::open(path)?;

    // Java keys metadata emission off the .txm/.txrm suffix (initFile: isTXM/isTXRM).
    let suffix = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    let is_txm = suffix.as_deref() == Some("txm");
    // Java initFile: paramsPrefix = isTXM ? GENERAL_PARAMS : PROJECTION.
    let params_prefix = if is_txm {
        "General Parameters: "
    } else {
        "Projection Info: "
    };

    let mut size_x = None;
    let mut size_y = None;
    let mut pixel_type = None;
    let mut metadata = HashMap::new();
    let mut image_paths = Vec::new();

    let paths: Vec<String> = ole
        .document_list()
        .into_iter()
        .map(|path| normalize_cfb_path(&path))
        .collect();

    for path in paths {
        if path.starts_with(IMAGE_DATA) {
            image_paths.push(path);
        } else if path == "/ImageInfo/ImageWidth" {
            let v = read_xrm_i32(&mut ole, &path)?;
            size_x = Some(v);
            metadata.insert(
                "Image Details: Image width (pixels)".into(),
                MetadataValue::Int(v as i64),
            );
        } else if path == "/ImageInfo/ImageHeight" {
            let v = read_xrm_i32(&mut ole, &path)?;
            size_y = Some(v);
            metadata.insert(
                "Image Details: Image height (pixels)".into(),
                MetadataValue::Int(v as i64),
            );
        } else if path == "/ImageInfo/DataType" {
            let code = read_xrm_i32(&mut ole, &path)?;
            let (ty, label) = xrm_pixel_type(code)?;
            pixel_type = Some(ty);
            if is_txm {
                metadata.insert(
                    "Reconstruction Settings: Output data type".into(),
                    MetadataValue::String(label.into()),
                );
            }
            metadata.insert(
                "Image Details: Data type".into(),
                MetadataValue::String(label.into()),
            );
        } else if path == "/ImageInfo/FileType" {
            if let Ok(value) = read_xrm_string(&mut ole, &path) {
                metadata.insert(
                    "Image Details: File type".into(),
                    MetadataValue::String(value),
                );
            }
        } else if path == "/ImageInfo/PixelSize" {
            if let Ok(value) = read_xrm_f32(&mut ole, &path) {
                metadata.insert(
                    "Image Details: Pixel size (um)".into(),
                    MetadataValue::Float(value as f64),
                );
            }
        } else if path == "/ImageInfo/AcquisitionMode" {
            let mode = read_xrm_i32(&mut ole, &path)?;
            let mode_value = match mode {
                0 => "Tomography".to_string(),
                10 => "Recon".to_string(),
                other => other.to_string(),
            };
            metadata.insert(
                "Image Details: Acquisition mode".into(),
                MetadataValue::String(mode_value),
            );
        } else if path == "/ImageInfo/SourceFilterName" {
            if let Ok(value) = read_xrm_string(&mut ole, &path) {
                metadata.insert(
                    "Source Assembly Info: Source Filter Name".into(),
                    MetadataValue::String(value.clone()),
                );
                metadata.insert(
                    format!("{params_prefix}Source filter name"),
                    MetadataValue::String(value),
                );
            }
        } else if path == "/ImageInfo/Voltage" {
            if let Ok(value) = read_xrm_f32(&mut ole, &path) {
                metadata.insert(
                    "Source Assembly Info: Voltage (kV)".into(),
                    MetadataValue::Float(value as f64),
                );
            }
        } else if path == "/exeVersion" {
            if let Ok(value) = read_xrm_string(&mut ole, &path) {
                metadata.insert(
                    "Dataset Info: Executable version".into(),
                    MetadataValue::String(value),
                );
            }
        } else if path == "/DetAssemblyInfo/LensInfo/LensName" {
            if let Ok(value) = read_xrm_string(&mut ole, &path) {
                metadata.insert(
                    format!("{params_prefix}Objective name"),
                    MetadataValue::String(value),
                );
            }
        } else if path == "/ImageInfo/CameraNumberOfFramesPerImage" {
            let v = read_xrm_i32(&mut ole, &path)?;
            metadata.insert(
                format!("{params_prefix}Frames per image"),
                MetadataValue::Int(v as i64),
            );
        } else if path == "/ImageInfo/NoOfImagesAveraged" {
            let v = read_xrm_i32(&mut ole, &path)?;
            metadata.insert(
                format!("{params_prefix}Images per projection"),
                MetadataValue::Int(v as i64),
            );
        } else if path == "/ImageInfo/CameraBinning" {
            let v = read_xrm_i32(&mut ole, &path)?;
            metadata.insert(
                format!("{params_prefix}Camera binning"),
                MetadataValue::Int(v as i64),
            );
        }
    }

    image_paths.sort_by_key(|p| xrm_image_index(p).unwrap_or(u32::MAX));
    if image_paths.is_empty() {
        return Err(BioFormatsError::UnsupportedFormat(
            "Zeiss XRM/TXRM contains no Root Entry/ImageData/ImageN streams".into(),
        ));
    }

    let size_x = size_x.ok_or_else(|| {
        BioFormatsError::UnsupportedFormat("Zeiss XRM/TXRM missing ImageInfo/ImageWidth".into())
    })?;
    let size_y = size_y.ok_or_else(|| {
        BioFormatsError::UnsupportedFormat("Zeiss XRM/TXRM missing ImageInfo/ImageHeight".into())
    })?;
    if size_x <= 0 || size_y <= 0 {
        return Err(BioFormatsError::UnsupportedFormat(
            "Zeiss XRM/TXRM has invalid non-positive image dimensions".into(),
        ));
    }
    let size_x = size_x as u32;
    let size_y = size_y as u32;
    let pixel_type = pixel_type.ok_or_else(|| {
        BioFormatsError::UnsupportedFormat("Zeiss XRM/TXRM missing ImageInfo/DataType".into())
    })?;

    let bits = (pixel_type.bytes_per_sample() * 8) as u8;
    let image_count = image_paths.len() as u32;
    let plane_bytes = size_x
        .checked_mul(size_y)
        .and_then(|px| px.checked_mul(pixel_type.bytes_per_sample() as u32))
        .ok_or_else(|| BioFormatsError::Format("XRM plane size overflows".into()))?
        as usize;
    for stream_path in &image_paths {
        let raw = ole.document_bytes(stream_path)?;
        if raw.len() < plane_bytes {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "Zeiss XRM/TXRM stream {stream_path} is shorter than declared: got {}, expected {plane_bytes}",
                raw.len()
            )));
        }
    }
    let meta = ImageMetadata {
        size_x,
        size_y,
        size_z: image_count,
        size_c: 1,
        size_t: 1,
        pixel_type,
        bits_per_pixel: bits,
        image_count,
        dimension_order: DimensionOrder::XYZTC,
        is_rgb: false,
        is_interleaved: false,
        is_indexed: false,
        is_little_endian: true,
        resolution_count: 1,
        thumbnail: false,
        series_metadata: metadata,
        lookup_table: None,
        modulo_z: None,
        modulo_c: None,
        modulo_t: None,
    };
    Ok((meta, image_paths))
}

fn normalize_cfb_path(path: &str) -> String {
    format!("/{}", cfb_path_without_root(path))
}

fn read_xrm_stream(ole: &mut OleFile, path: &str) -> Result<Vec<u8>> {
    ole.document_bytes(path)
}

fn read_xrm_i32(ole: &mut OleFile, path: &str) -> Result<i32> {
    let data = read_xrm_stream(ole, path)?;
    let bytes = data
        .get(..4)
        .ok_or_else(|| BioFormatsError::Format(format!("XRM stream {path} is shorter than i32")))?;
    Ok(i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn read_xrm_f32(ole: &mut OleFile, path: &str) -> Result<f32> {
    let data = read_xrm_stream(ole, path)?;
    let bytes = data
        .get(..4)
        .ok_or_else(|| BioFormatsError::Format(format!("XRM stream {path} is shorter than f32")))?;
    Ok(f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn read_xrm_string(ole: &mut OleFile, path: &str) -> Result<String> {
    let data = read_xrm_stream(ole, path)?;
    Ok(String::from_utf8_lossy(&data)
        .trim_matches(char::from(0))
        .trim()
        .to_string())
}

fn xrm_pixel_type(data_type: i32) -> Result<(PixelType, &'static str)> {
    match data_type {
        2 => Ok((PixelType::Int8, "byte")),
        3 => Ok((PixelType::Uint8, "ubyte")),
        4 => Ok((PixelType::Int16, "short")),
        5 => Ok((PixelType::Uint16, "ushort")),
        6 => Ok((PixelType::Int32, "int")),
        7 => Ok((PixelType::Uint32, "uint")),
        10 => Ok((PixelType::Float32, "float")),
        11 => Ok((PixelType::Float64, "double")),
        other => Err(BioFormatsError::UnsupportedFormat(format!(
            "Zeiss XRM/TXRM unsupported data type: {other}"
        ))),
    }
}

fn xrm_image_index(path: &str) -> Option<u32> {
    let tail = path.rsplit('/').next()?;
    let digits = tail.strip_prefix("Image")?;
    digits.parse().ok()
}

fn xrm_flip_rows(raw: &[u8], meta: &ImageMetadata) -> Result<Vec<u8>> {
    let row_len = meta
        .size_x
        .checked_mul(meta.pixel_type.bytes_per_sample() as u32)
        .ok_or_else(|| BioFormatsError::Format("XRM row size overflows".into()))?
        as usize;
    let expected = row_len
        .checked_mul(meta.size_y as usize)
        .ok_or_else(|| BioFormatsError::Format("XRM plane size overflows".into()))?;
    if raw.len() < expected {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "Zeiss XRM/TXRM plane is shorter than declared: got {}, expected {expected}",
            raw.len()
        )));
    }

    let mut out = Vec::with_capacity(expected);
    for row in (0..meta.size_y as usize).rev() {
        let start = row * row_len;
        out.extend_from_slice(&raw[start..start + row_len]);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::reader::FormatReader;
    use std::io::Write;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("bioformats_xrm_{nanos}_{name}"))
    }

    fn write_stream(comp: &mut cfb::CompoundFile<std::fs::File>, path: &str, data: &[u8]) {
        if let Some(parent) = Path::new(path).parent() {
            comp.create_storage_all(parent).unwrap();
        }
        comp.create_stream(path).unwrap().write_all(data).unwrap();
    }

    fn write_i32_stream(comp: &mut cfb::CompoundFile<std::fs::File>, path: &str, value: i32) {
        write_stream(comp, path, &value.to_le_bytes());
    }

    #[test]
    fn xrm_reads_cfb_imageinfo_and_flipped_image_planes() {
        let path = temp_path("synthetic.txrm");
        {
            let mut comp = cfb::create(&path).unwrap();
            write_i32_stream(&mut comp, "/ImageInfo/ImageWidth", 3);
            write_i32_stream(&mut comp, "/ImageInfo/ImageHeight", 2);
            write_i32_stream(&mut comp, "/ImageInfo/DataType", 3);
            write_stream(&mut comp, "/ImageInfo/FileType", b"txrm\0");
            write_stream(&mut comp, "/ImageData/Image2", &[21, 22, 23, 24, 25, 26]);
            write_stream(&mut comp, "/ImageData/Image1", &[1, 2, 3, 4, 5, 6]);
        }

        let mut reader = ZeissXrmReader::new();
        reader.set_id(&path).unwrap();
        let meta = reader.metadata();
        assert_eq!(meta.size_x, 3);
        assert_eq!(meta.size_y, 2);
        assert_eq!(meta.size_z, 2);
        assert_eq!(meta.pixel_type, PixelType::Uint8);
        assert_eq!(meta.image_count, 2);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![4, 5, 6, 1, 2, 3]);
        assert_eq!(reader.open_bytes(1).unwrap(), vec![24, 25, 26, 21, 22, 23]);
        assert_eq!(
            reader.open_bytes_region(0, 1, 0, 2, 2).unwrap(),
            vec![5, 6, 2, 3]
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn xrm_captures_named_global_metadata_keys() {
        // .txrm: paramsPrefix == "Projection Info: ", no "Output data type".
        let txrm = temp_path("named_meta.txrm");
        {
            let mut comp = cfb::create(&txrm).unwrap();
            write_i32_stream(&mut comp, "/ImageInfo/ImageWidth", 2);
            write_i32_stream(&mut comp, "/ImageInfo/ImageHeight", 2);
            write_i32_stream(&mut comp, "/ImageInfo/DataType", 5);
            write_i32_stream(&mut comp, "/ImageInfo/AcquisitionMode", 0);
            write_stream(&mut comp, "/ImageInfo/SourceFilterName", b"LE1\0");
            write_stream(&mut comp, "/ImageInfo/Voltage", &40.0f32.to_le_bytes());
            write_stream(&mut comp, "/exeVersion", b"1.2.3\0");
            write_stream(&mut comp, "/DetAssemblyInfo/LensInfo/LensName", b"20X\0");
            write_i32_stream(&mut comp, "/ImageInfo/CameraNumberOfFramesPerImage", 4);
            write_i32_stream(&mut comp, "/ImageInfo/NoOfImagesAveraged", 3);
            write_i32_stream(&mut comp, "/ImageInfo/CameraBinning", 2);
            write_stream(&mut comp, "/ImageData/Image1", &[0u8; 8]);
        }

        let mut reader = ZeissXrmReader::new();
        reader.set_id(&txrm).unwrap();
        let md = &reader.metadata().series_metadata;
        assert_eq!(
            md.get("Image Details: Acquisition mode").map(|v| v.to_string()),
            Some("Tomography".to_string())
        );
        assert_eq!(
            md.get("Source Assembly Info: Source Filter Name")
                .map(|v| v.to_string()),
            Some("LE1".to_string())
        );
        assert_eq!(
            md.get("Projection Info: Source filter name")
                .map(|v| v.to_string()),
            Some("LE1".to_string())
        );
        assert!(md.contains_key("Source Assembly Info: Voltage (kV)"));
        assert_eq!(
            md.get("Dataset Info: Executable version")
                .map(|v| v.to_string()),
            Some("1.2.3".to_string())
        );
        assert_eq!(
            md.get("Projection Info: Objective name").map(|v| v.to_string()),
            Some("20X".to_string())
        );
        assert_eq!(
            md.get("Projection Info: Frames per image").map(|v| v.to_string()),
            Some("4".to_string())
        );
        assert_eq!(
            md.get("Projection Info: Images per projection")
                .map(|v| v.to_string()),
            Some("3".to_string())
        );
        assert_eq!(
            md.get("Projection Info: Camera binning").map(|v| v.to_string()),
            Some("2".to_string())
        );
        // TXRM must NOT carry the TXM-only "Output data type" key.
        assert!(!md.contains_key("Reconstruction Settings: Output data type"));
        let _ = std::fs::remove_file(txrm);

        // .txm: emits "Output data type" and uses "General Parameters: " prefix.
        let txm = temp_path("named_meta.txm");
        {
            let mut comp = cfb::create(&txm).unwrap();
            write_i32_stream(&mut comp, "/ImageInfo/ImageWidth", 2);
            write_i32_stream(&mut comp, "/ImageInfo/ImageHeight", 2);
            write_i32_stream(&mut comp, "/ImageInfo/DataType", 5);
            write_i32_stream(&mut comp, "/ImageInfo/CameraBinning", 2);
            write_stream(&mut comp, "/ImageData/Image1", &[0u8; 8]);
        }
        let mut reader = ZeissXrmReader::new();
        reader.set_id(&txm).unwrap();
        let md = &reader.metadata().series_metadata;
        assert_eq!(
            md.get("Reconstruction Settings: Output data type")
                .map(|v| v.to_string()),
            Some("ushort".to_string())
        );
        assert_eq!(
            md.get("General Parameters: Camera binning")
                .map(|v| v.to_string()),
            Some("2".to_string())
        );
        let _ = std::fs::remove_file(txm);
    }

    #[test]
    fn xrm_rejects_missing_required_imageinfo() {
        let path = temp_path("missing.txm");
        {
            let mut comp = cfb::create(&path).unwrap();
            write_i32_stream(&mut comp, "/ImageInfo/ImageWidth", 2);
            write_i32_stream(&mut comp, "/ImageInfo/DataType", 5);
            write_stream(&mut comp, "/ImageData/Image1", &[0; 8]);
        }

        let err = ZeissXrmReader::new().set_id(&path).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("ImageHeight")),
            "{err:?}"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn xrm_rejects_non_positive_dimensions_before_casting() {
        let path = temp_path("negative_width.txm");
        {
            let mut comp = cfb::create(&path).unwrap();
            write_i32_stream(&mut comp, "/ImageInfo/ImageWidth", -2);
            write_i32_stream(&mut comp, "/ImageInfo/ImageHeight", 2);
            write_i32_stream(&mut comp, "/ImageInfo/DataType", 3);
            write_stream(&mut comp, "/ImageData/Image1", &[0; 4]);
        }

        let err = ZeissXrmReader::new().set_id(&path).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("non-positive")),
            "{err:?}"
        );
        let _ = std::fs::remove_file(path);
    }
}
