//! bioformats-mias — format readers:
//!
//! - CellWorxReader: CellWorX HCS (.htd / .pnl)
//! - Al3dReader: 3D image format (.al3d) with "AL3D" magic
//! - OxfordInstrumentsReader: Oxford Instruments SEM/AFM (.top)
//! - FeiSerReader: FEI SER electron-microscopy series (.ser)

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;

// ── Helpers ───────────────────────────────────────────────────────────────────

fn simple_meta(w: u32, h: u32, z: u32, pt: PixelType) -> ImageMetadata {
    let bps = pt.bytes_per_sample();
    ImageMetadata {
        size_x: w,
        size_y: h,
        size_z: z,
        size_c: 1,
        size_t: 1,
        pixel_type: pt,
        bits_per_pixel: (bps * 8) as u8,
        image_count: z,
        dimension_order: DimensionOrder::XYZCT,
        is_rgb: false,
        is_interleaved: false,
        is_indexed: false,
        is_little_endian: true,
        resolution_count: 1,
        series_metadata: HashMap::new(),
        lookup_table: None,
        modulo_z: None,
        modulo_c: None,
        modulo_t: None,
    }
}

fn checked_payload_len(meta: &ImageMetadata) -> Result<u64> {
    let bps = meta.pixel_type.bytes_per_sample() as u64;
    (meta.size_x as u64)
        .checked_mul(meta.size_y as u64)
        .and_then(|px| px.checked_mul(bps))
        .and_then(|plane| plane.checked_mul(meta.image_count as u64))
        .ok_or_else(|| BioFormatsError::Format("declared image payload size overflows".into()))
}

// ── CellWorxReader ────────────────────────────────────────────────────────────

pub struct CellWorxReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    pixels: Vec<u8>,
    plane_len: usize,
}

impl CellWorxReader {
    pub fn new() -> Self {
        CellWorxReader {
            path: None,
            meta: None,
            pixels: Vec::new(),
            plane_len: 0,
        }
    }
}

impl Default for CellWorxReader {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy)]
struct CellWorxLayout {
    image_count: u32,
    raw: Option<CellWorxRawLayout>,
}

#[derive(Debug, Clone, Copy)]
struct CellWorxRawLayout {
    width: u32,
    height: u32,
    pixel_type: PixelType,
    little_endian: bool,
}

fn parse_htd(path: &Path) -> Result<(CellWorxLayout, Option<String>)> {
    let content = std::fs::read_to_string(path).map_err(BioFormatsError::Io)?;
    let mut x_sites = None;
    let mut y_sites = None;
    let mut timepoints = None;
    let mut z_steps = None;
    let mut wavelengths = None;
    let mut has_strict_raw_marker = false;
    let mut raw_width = None;
    let mut raw_height = None;
    let mut raw_pixel_type = None;
    let mut raw_little_endian = None;
    let mut raw_file = None;

    for line in content.lines() {
        let line = line.trim();
        if line == "BF_CELLWORX_RAW_V1" {
            has_strict_raw_marker = true;
        } else if let Some(v) = htd_kv(line, "BioFormatsRaw") {
            has_strict_raw_marker = parse_bool_htd("BioFormatsRaw", v)?;
        } else if let Some(v) = htd_kv(line, "XSites") {
            x_sites = Some(parse_positive_htd_u32("XSites", v)?);
        } else if let Some(v) = htd_kv(line, "YSites") {
            y_sites = Some(parse_positive_htd_u32("YSites", v)?);
        } else if let Some(v) = htd_kv(line, "TimePoints") {
            timepoints = Some(parse_positive_htd_u32("TimePoints", v)?);
        } else if let Some(v) = htd_kv(line, "ZSteps") {
            z_steps = Some(parse_positive_htd_u32("ZSteps", v)?);
        } else if let Some(v) = htd_kv(line, "Wavelengths") {
            wavelengths = Some(parse_positive_htd_u32("Wavelengths", v)?);
        } else if let Some(v) = htd_kv(line, "RawWidth") {
            raw_width = Some(parse_positive_htd_u32("RawWidth", v)?);
        } else if let Some(v) = htd_kv(line, "RawHeight") {
            raw_height = Some(parse_positive_htd_u32("RawHeight", v)?);
        } else if let Some(v) = htd_kv(line, "RawPixelType") {
            raw_pixel_type = Some(parse_cellworx_pixel_type(v)?);
        } else if let Some(v) = htd_kv(line, "RawLittleEndian") {
            raw_little_endian = Some(parse_bool_htd("RawLittleEndian", v)?);
        } else if let Some(v) = htd_kv(line, "RawFile") {
            raw_file = Some(v.split(',').next().unwrap_or(v).trim().to_string());
        }
    }

    let x_sites = x_sites.ok_or_else(|| {
        BioFormatsError::UnsupportedFormat("CellWorX HTD header missing XSites".into())
    })?;
    let y_sites = y_sites.ok_or_else(|| {
        BioFormatsError::UnsupportedFormat("CellWorX HTD header missing YSites".into())
    })?;
    let timepoints = timepoints.unwrap_or(1);
    let z_steps = z_steps.unwrap_or(1);
    let wavelengths = wavelengths.unwrap_or(1);
    let image_count = x_sites
        .checked_mul(y_sites)
        .and_then(|n| n.checked_mul(timepoints))
        .and_then(|n| n.checked_mul(z_steps))
        .and_then(|n| n.checked_mul(wavelengths))
        .ok_or_else(|| BioFormatsError::Format("CellWorX HTD image count overflows".into()))?;
    let raw = if has_strict_raw_marker {
        Some(CellWorxRawLayout {
            width: raw_width.ok_or_else(|| {
                BioFormatsError::UnsupportedFormat(
                    "CellWorX strict raw HTD header missing RawWidth".into(),
                )
            })?,
            height: raw_height.ok_or_else(|| {
                BioFormatsError::UnsupportedFormat(
                    "CellWorX strict raw HTD header missing RawHeight".into(),
                )
            })?,
            pixel_type: raw_pixel_type.ok_or_else(|| {
                BioFormatsError::UnsupportedFormat(
                    "CellWorX strict raw HTD header missing RawPixelType".into(),
                )
            })?,
            little_endian: raw_little_endian.unwrap_or(true),
        })
    } else {
        None
    };

    Ok((CellWorxLayout { image_count, raw }, raw_file))
}

fn htd_kv<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let stripped = line.strip_prefix(key)?.trim_start();
    Some(stripped.strip_prefix(',')?.trim_start())
}

fn parse_positive_htd_u32(key: &str, value: &str) -> Result<u32> {
    let value = value.split(',').next().unwrap_or(value).trim();
    let n = value.parse::<u32>().map_err(|_| {
        BioFormatsError::UnsupportedFormat(format!("CellWorX HTD header has invalid {key}"))
    })?;
    if n == 0 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "CellWorX HTD header has zero {key}"
        )));
    }
    Ok(n)
}

fn parse_bool_htd(key: &str, value: &str) -> Result<bool> {
    let value = value.split(',').next().unwrap_or(value).trim();
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" => Ok(true),
        "0" | "false" | "no" => Ok(false),
        _ => Err(BioFormatsError::UnsupportedFormat(format!(
            "CellWorX HTD header has invalid {key}"
        ))),
    }
}

fn parse_cellworx_pixel_type(value: &str) -> Result<PixelType> {
    let value = value.split(',').next().unwrap_or(value).trim();
    match value.to_ascii_lowercase().as_str() {
        "uint8" | "u8" => Ok(PixelType::Uint8),
        "uint16" | "u16" => Ok(PixelType::Uint16),
        "int16" | "i16" => Ok(PixelType::Int16),
        "uint32" | "u32" => Ok(PixelType::Uint32),
        "int32" | "i32" => Ok(PixelType::Int32),
        "float32" | "f32" => Ok(PixelType::Float32),
        _ => Err(BioFormatsError::UnsupportedFormat(
            "CellWorX strict raw HTD header has unsupported RawPixelType".into(),
        )),
    }
}

fn resolve_cellworx_raw_path(cfg_path: &Path, raw_file: Option<String>) -> Result<PathBuf> {
    let raw_file = raw_file.ok_or_else(|| {
        BioFormatsError::UnsupportedFormat("CellWorX strict raw HTD header missing RawFile".into())
    })?;
    let relative = Path::new(&raw_file);
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::RootDir))
    {
        return Err(BioFormatsError::Format(
            "CellWorX strict raw RawFile must stay beside the HTD header".into(),
        ));
    }
    Ok(cfg_path
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .join(relative))
}

impl FormatReader for CellWorxReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("htd") | Some("pnl"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        // If .pnl, look for companion .htd
        let cfg_path = if path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("pnl"))
            .unwrap_or(false)
        {
            path.with_extension("htd")
        } else {
            path.to_path_buf()
        };

        if !cfg_path.exists() {
            return Err(BioFormatsError::UnsupportedFormat(
                "CellWorX HTD/PNL companion header is missing".to_string(),
            ));
        }
        let (layout, raw_file) = parse_htd(&cfg_path)?;
        let Some(raw) = layout.raw else {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "CellWorX HTD/PNL parsed {} declared planes; native companion image payload decoding is unsupported unless explicit BF_CELLWORX_RAW_V1 sidecar fields are present",
                layout.image_count
            )));
        };

        let raw_path = resolve_cellworx_raw_path(&cfg_path, raw_file)?;
        let mut meta = simple_meta(raw.width, raw.height, layout.image_count, raw.pixel_type);
        meta.is_little_endian = raw.little_endian;
        meta.series_metadata.insert(
            "CellWorX strict raw file".into(),
            MetadataValue::String(raw_path.to_string_lossy().into_owned()),
        );
        let expected = checked_payload_len(&meta)?;
        let pixels = std::fs::read(&raw_path).map_err(BioFormatsError::Io)?;
        if pixels.len() as u64 != expected {
            return Err(BioFormatsError::Format(format!(
                "CellWorX strict raw payload length {} does not match declared length {expected}",
                pixels.len()
            )));
        }
        self.plane_len = expected as usize / meta.image_count as usize;
        self.path = Some(cfg_path);
        self.meta = Some(meta);
        self.pixels = pixels;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.pixels.clear();
        self.plane_len = 0;
        Ok(())
    }

    fn series_count(&self) -> usize {
        usize::from(self.meta.is_some())
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.meta.is_none() {
            Err(BioFormatsError::NotInitialized)
        } else if s == 0 {
            Ok(())
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

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let start = plane_index as usize * self.plane_len;
        Ok(self.pixels[start..start + self.plane_len].to_vec())
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
        crop_full_plane("CellWorX", &full, meta, 1, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        self.open_bytes(plane_index)
    }
}

// ── Al3dReader ────────────────────────────────────────────────────────────────

const AL3D_MAGIC: &[u8] = b"AL3D";
const AL3D_DATA_OFFSET: u64 = 512;

pub struct Al3dReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl Al3dReader {
    pub fn new() -> Self {
        Al3dReader {
            path: None,
            meta: None,
        }
    }
}

impl Default for Al3dReader {
    fn default() -> Self {
        Self::new()
    }
}

fn parse_al3d(path: &Path) -> Result<ImageMetadata> {
    let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
    if data.len() < AL3D_DATA_OFFSET as usize {
        return Err(BioFormatsError::UnsupportedFormat(
            "AL3D file too short for declared header offset".into(),
        ));
    }
    if &data[..4] != AL3D_MAGIC {
        return Err(BioFormatsError::UnsupportedFormat(
            "AL3D file is missing AL3D magic".into(),
        ));
    }
    // Offset 8: width (u32 LE), 12: height (u32 LE), 16: depth (u32 LE)
    let width = u32::from_le_bytes([data[8], data[9], data[10], data[11]]);
    let height = u32::from_le_bytes([data[12], data[13], data[14], data[15]]);
    let depth = u32::from_le_bytes([data[16], data[17], data[18], data[19]]);
    if width == 0 || height == 0 || depth == 0 {
        return Err(BioFormatsError::UnsupportedFormat(
            "AL3D file has zero image dimensions".into(),
        ));
    }
    // Offset 20: data_type (u16 LE)
    let data_type = u16::from_le_bytes([data[20], data[21]]);
    let pixel_type = match data_type {
        0 => PixelType::Uint8,
        1 => PixelType::Uint16,
        2 => PixelType::Float32,
        other => {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "AL3D data type {other} is not supported"
            )));
        }
    };
    let meta = simple_meta(width, height, depth, pixel_type);
    let required_len = AL3D_DATA_OFFSET
        .checked_add(checked_payload_len(&meta)?)
        .ok_or_else(|| BioFormatsError::Format("AL3D file size overflows".into()))?;
    if (data.len() as u64) < required_len {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "AL3D pixel payload is shorter than declared ({} < {required_len})",
            data.len()
        )));
    }
    Ok(meta)
}

impl FormatReader for Al3dReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("al3d"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.len() >= 4 && header[0..4] == *AL3D_MAGIC
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let meta = parse_al3d(path)?;
        self.path = Some(path.to_path_buf());
        self.meta = Some(meta);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        Ok(())
    }

    fn series_count(&self) -> usize {
        usize::from(self.meta.is_some())
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.meta.is_none() {
            Err(BioFormatsError::NotInitialized)
        } else if s == 0 {
            Ok(())
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

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let bps = meta.pixel_type.bytes_per_sample();
        let plane_bytes = meta.size_x as usize * meta.size_y as usize * bps;
        let plane_offset = AL3D_DATA_OFFSET + plane_index as u64 * plane_bytes as u64;
        let path = self
            .path
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?
            .clone();
        let mut f = std::fs::File::open(&path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(plane_offset))
            .map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; plane_bytes];
        f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
        Ok(buf)
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
        crop_full_plane("AL3D", &full, meta, 1, x, y, w, h)
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

// ── FeiSerReader ──────────────────────────────────────────────────────────────

/// FEI SER format: electron-microscopy image series from TEM/STEM systems.
/// Magic: bytes 0-1 == 0x97 0x01 (series file signature).
pub struct FeiSerReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    data_offsets: Vec<u64>,
}

impl FeiSerReader {
    pub fn new() -> Self {
        FeiSerReader {
            path: None,
            meta: None,
            data_offsets: Vec::new(),
        }
    }
}

impl Default for FeiSerReader {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
struct SerParseResult {
    meta: ImageMetadata,
    data_offsets: Vec<u64>,
}

const SER_MAGIC: u16 = 0x0197;
const SER_2D_IMAGE_DATA_TYPE: u32 = 0x4122;
const SER_LONG_OFFSET_VERSION: u16 = 0x0220;
const SER_2D_ELEMENT_HEADER_LEN: u64 = 50;

fn read_u16_le(data: &[u8], offset: usize, label: &str) -> Result<u16> {
    let bytes = data.get(offset..offset + 2).ok_or_else(|| {
        BioFormatsError::UnsupportedFormat(format!("FEI SER header is too short for {label}"))
    })?;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn read_u32_le(data: &[u8], offset: usize, label: &str) -> Result<u32> {
    let bytes = data.get(offset..offset + 4).ok_or_else(|| {
        BioFormatsError::UnsupportedFormat(format!("FEI SER header is too short for {label}"))
    })?;
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn read_u64_le(data: &[u8], offset: usize, label: &str) -> Result<u64> {
    let bytes = data.get(offset..offset + 8).ok_or_else(|| {
        BioFormatsError::UnsupportedFormat(format!("FEI SER header is too short for {label}"))
    })?;
    Ok(u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ]))
}

fn ser_pixel_type(dtype: u16) -> Result<PixelType> {
    match dtype {
        1 => Ok(PixelType::Uint8),
        2 => Ok(PixelType::Uint16),
        3 => Ok(PixelType::Uint32),
        4 => Ok(PixelType::Int8),
        5 => Ok(PixelType::Int16),
        6 => Ok(PixelType::Int32),
        7 => Ok(PixelType::Float32),
        8 => Ok(PixelType::Float64),
        _ => Err(BioFormatsError::UnsupportedFormat(format!(
            "FEI SER unsupported element pixel type {dtype}"
        ))),
    }
}

fn parse_ser_element_header(data: &[u8], offset: u64) -> Result<(u32, u32, PixelType, u64)> {
    let offset_usize = usize::try_from(offset)
        .map_err(|_| BioFormatsError::Format("FEI SER element offset overflows".into()))?;
    let end = offset
        .checked_add(SER_2D_ELEMENT_HEADER_LEN)
        .ok_or_else(|| BioFormatsError::Format("FEI SER element header offset overflows".into()))?;
    if end > data.len() as u64 {
        return Err(BioFormatsError::UnsupportedFormat(
            "FEI SER image element header is shorter than declared".into(),
        ));
    }
    let dtype = read_u16_le(data, offset_usize + 40, "element pixel type")?;
    let width = read_u32_le(data, offset_usize + 42, "element width")?;
    let height = read_u32_le(data, offset_usize + 46, "element height")?;
    if width == 0 || height == 0 {
        return Err(BioFormatsError::UnsupportedFormat(
            "FEI SER image element has zero image dimensions".into(),
        ));
    }
    Ok((width, height, ser_pixel_type(dtype)?, end))
}

fn parse_ser(path: &Path) -> Result<SerParseResult> {
    let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
    if data.len() < 28 {
        return Err(BioFormatsError::UnsupportedFormat(
            "FEI SER header is too short for safe image decoding".to_string(),
        ));
    }
    let series_id = read_u16_le(&data, 0, "series id")?;
    if series_id != SER_MAGIC {
        return Err(BioFormatsError::UnsupportedFormat(
            "FEI SER header is missing 0x0197 magic".into(),
        ));
    }
    let version = read_u16_le(&data, 2, "series version")?;
    let data_type_id = read_u32_le(&data, 4, "data type id")?;
    if data_type_id != SER_2D_IMAGE_DATA_TYPE {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "FEI SER only supports 2D image data elements, found type 0x{data_type_id:04x}"
        )));
    }
    let tag_type_id = read_u32_le(&data, 8, "tag type id")?;
    let total = read_u32_le(&data, 12, "total element count")?;
    let valid = read_u32_le(&data, 16, "valid element count")?;
    if total == 0 || valid == 0 || valid > total {
        return Err(BioFormatsError::UnsupportedFormat(
            "FEI SER header has invalid element counts".into(),
        ));
    }

    let (offset_array_offset, number_dimensions_offset) = if version >= SER_LONG_OFFSET_VERSION {
        (read_u64_le(&data, 20, "offset array offset")?, 28usize)
    } else {
        (
            read_u32_le(&data, 20, "offset array offset")? as u64,
            24usize,
        )
    };
    let number_dimensions = read_u32_le(&data, number_dimensions_offset, "dimension count")?;
    if number_dimensions > 16 {
        return Err(BioFormatsError::UnsupportedFormat(
            "FEI SER header has implausible dimension count".into(),
        ));
    }
    if offset_array_offset == 0 || offset_array_offset >= data.len() as u64 {
        return Err(BioFormatsError::UnsupportedFormat(
            "FEI SER offset array is missing or outside the file".into(),
        ));
    }

    let offset_size = if version >= SER_LONG_OFFSET_VERSION {
        8u64
    } else {
        4u64
    };
    let offset_array_bytes = (valid as u64)
        .checked_mul(offset_size)
        .ok_or_else(|| BioFormatsError::Format("FEI SER offset array size overflows".into()))?;
    let offset_array_end = offset_array_offset
        .checked_add(offset_array_bytes)
        .ok_or_else(|| BioFormatsError::Format("FEI SER offset array end overflows".into()))?;
    if offset_array_end > data.len() as u64 {
        return Err(BioFormatsError::UnsupportedFormat(
            "FEI SER offset array is shorter than declared".into(),
        ));
    }

    let mut data_offsets = Vec::with_capacity(valid as usize);
    let base = usize::try_from(offset_array_offset)
        .map_err(|_| BioFormatsError::Format("FEI SER offset array offset overflows".into()))?;
    for i in 0..valid as usize {
        let entry_offset = base + i * offset_size as usize;
        let element_offset = if offset_size == 8 {
            read_u64_le(&data, entry_offset, "element offset")?
        } else {
            read_u32_le(&data, entry_offset, "element offset")? as u64
        };
        if element_offset == 0 || element_offset >= data.len() as u64 {
            return Err(BioFormatsError::UnsupportedFormat(
                "FEI SER image element offset is missing or outside the file".into(),
            ));
        }
        data_offsets.push(element_offset);
    }

    let (width, height, pixel_type, first_payload_offset) =
        parse_ser_element_header(&data, data_offsets[0])?;
    let plane_bytes = (width as u64)
        .checked_mul(height as u64)
        .and_then(|n| n.checked_mul(pixel_type.bytes_per_sample() as u64))
        .ok_or_else(|| BioFormatsError::Format("FEI SER plane size overflows".into()))?;
    let first_payload_end = first_payload_offset
        .checked_add(plane_bytes)
        .ok_or_else(|| BioFormatsError::Format("FEI SER payload end overflows".into()))?;
    if first_payload_end > data.len() as u64 {
        return Err(BioFormatsError::UnsupportedFormat(
            "FEI SER image payload is shorter than declared".into(),
        ));
    }
    for &offset in data_offsets.iter().skip(1) {
        let (frame_w, frame_h, frame_pixel_type, payload_offset) =
            parse_ser_element_header(&data, offset)?;
        if frame_w != width || frame_h != height || frame_pixel_type != pixel_type {
            return Err(BioFormatsError::UnsupportedFormat(
                "FEI SER mixed image element dimensions or pixel types are not supported".into(),
            ));
        }
        let payload_end = payload_offset
            .checked_add(plane_bytes)
            .ok_or_else(|| BioFormatsError::Format("FEI SER payload end overflows".into()))?;
        if payload_end > data.len() as u64 {
            return Err(BioFormatsError::UnsupportedFormat(
                "FEI SER image payload is shorter than declared".into(),
            ));
        }
    }

    let mut meta = simple_meta(width, height, valid, pixel_type);
    meta.series_metadata.insert(
        "format".to_string(),
        MetadataValue::String("FEI SER".to_string()),
    );
    meta.series_metadata.insert(
        "ser_version".to_string(),
        MetadataValue::Int(version as i64),
    );
    meta.series_metadata.insert(
        "ser_tag_type_id".to_string(),
        MetadataValue::Int(tag_type_id as i64),
    );
    meta.series_metadata.insert(
        "ser_number_dimensions".to_string(),
        MetadataValue::Int(number_dimensions as i64),
    );
    Ok(SerParseResult { meta, data_offsets })
}

impl FormatReader for FeiSerReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("ser"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.len() >= 2 && header[0] == 0x97 && header[1] == 0x01
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let parsed = parse_ser(path)?;
        self.path = Some(path.to_path_buf());
        self.meta = Some(parsed.meta);
        self.data_offsets = parsed.data_offsets;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.data_offsets.clear();
        Ok(())
    }
    fn series_count(&self) -> usize {
        if self.meta.is_some() {
            1
        } else {
            0
        }
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.meta.is_some() && s == 0 {
            Ok(())
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

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let path = self
            .path
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?
            .clone();
        let offset = *self
            .data_offsets
            .get(plane_index as usize)
            .ok_or(BioFormatsError::PlaneOutOfRange(plane_index))?;
        let payload_offset = offset
            .checked_add(SER_2D_ELEMENT_HEADER_LEN)
            .ok_or_else(|| BioFormatsError::Format("FEI SER payload offset overflows".into()))?;
        let plane_bytes =
            meta.size_x as usize * meta.size_y as usize * meta.pixel_type.bytes_per_sample();
        let mut f = std::fs::File::open(&path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(payload_offset))
            .map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; plane_bytes];
        f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
        Ok(buf)
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
        crop_full_plane("FEI SER", &full, meta, 1, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        self.open_bytes(plane_index)
    }
}

// ── OxfordInstrumentsReader ───────────────────────────────────────────────────

const OXFORD_DATA_OFFSET: u64 = 128;

pub struct OxfordInstrumentsReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl OxfordInstrumentsReader {
    pub fn new() -> Self {
        OxfordInstrumentsReader {
            path: None,
            meta: None,
        }
    }
}

impl Default for OxfordInstrumentsReader {
    fn default() -> Self {
        Self::new()
    }
}

fn parse_oxford(path: &Path) -> Result<ImageMetadata> {
    let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
    if data.len() < 12 {
        return Err(BioFormatsError::UnsupportedFormat(
            "Oxford TOP header is too short for safe image decoding".to_string(),
        ));
    }
    // Offset 4: width (u16 LE), 6: height (u16 LE), 8: data_type (u16 LE)
    let width = u16::from_le_bytes([data[4], data[5]]) as u32;
    let height = u16::from_le_bytes([data[6], data[7]]) as u32;
    let dtype = u16::from_le_bytes([data[8], data[9]]);
    let pixel_type = match dtype {
        0 => PixelType::Uint8,
        1 => PixelType::Uint16,
        2 => PixelType::Float32,
        other => {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "Oxford TOP data type {other} is not supported"
            )));
        }
    };
    if width == 0 || height == 0 {
        return Err(BioFormatsError::UnsupportedFormat(
            "Oxford TOP header is missing image dimensions".to_string(),
        ));
    }
    let meta = simple_meta(width, height, 1, pixel_type);
    let required_len = OXFORD_DATA_OFFSET
        .checked_add(checked_payload_len(&meta)?)
        .ok_or_else(|| BioFormatsError::Format("Oxford TOP file size overflows".into()))?;
    if (data.len() as u64) < required_len {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "Oxford TOP pixel payload is shorter than declared ({} < {required_len})",
            data.len()
        )));
    }
    Ok(meta)
}

impl FormatReader for OxfordInstrumentsReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("top"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let meta = parse_oxford(path)?;
        self.path = Some(path.to_path_buf());
        self.meta = Some(meta);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        Ok(())
    }

    fn series_count(&self) -> usize {
        usize::from(self.meta.is_some())
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if s == 0 && self.meta.is_some() {
            Ok(())
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

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index != 0 {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let bps = meta.pixel_type.bytes_per_sample();
        let plane_bytes = meta.size_x as usize * meta.size_y as usize * bps;
        let path = self
            .path
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?
            .clone();
        let mut f = std::fs::File::open(&path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(OXFORD_DATA_OFFSET))
            .map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; plane_bytes];
        f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
        Ok(buf)
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
        crop_full_plane("Oxford Instruments", &full, meta, 1, x, y, w, h)
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

// ── MIASReader ────────────────────────────────────────────────────────────────
//
// MIAS (Maia Scientific) HCS reader, ported from the upstream Java MIASReader.
// A dataset is a directory hierarchy:
//
//   <experiment>/<plate>/Well<xxxx>/mode<c>_z<zzz>_t<ttt>_im<r>_<col>.tif
//
// Each TIFF contains a single grayscale plane.  The "mode" block is the
// channel, "z"/"t" are the Z section and timepoint, and "im<r>_<col>" gives the
// tile coordinates within a mosaic.  One series is produced per well.
//
// This implementation handles the common (non-tiled, single-plane-per-file)
// case faithfully; tiled mosaics fall back to reading the first tile.

/// Per-well TIFF planes plus the parsed dimension structure.
struct MiasWell {
    /// Sorted TIFF file paths (one plane each).
    tiffs: Vec<PathBuf>,
    size_z: u32,
    size_c: u32,
    size_t: u32,
    well_number: i64,
}

pub struct MiasReader {
    wells: Vec<MiasWell>,
    series: Vec<ImageMetadata>,
    current_series: usize,
    tile_rows: u32,
    tile_cols: u32,
    tiff_reader: crate::tiff::TiffReader,
    tiff_loaded: bool,
}

impl MiasReader {
    pub fn new() -> Self {
        MiasReader {
            wells: Vec::new(),
            series: Vec::new(),
            current_series: 0,
            tile_rows: 1,
            tile_cols: 1,
            tiff_reader: crate::tiff::TiffReader::new(),
            tiff_loaded: false,
        }
    }
}

impl Default for MiasReader {
    fn default() -> Self {
        Self::new()
    }
}

fn is_mias_tiff(name: &str) -> bool {
    let l = name.to_ascii_lowercase();
    l.ends_with(".tif") || l.ends_with(".tiff")
}

/// Extract the integer following a `<prefix>` block in a MIAS filename, e.g.
/// `mode2_z003_t001_...` -> for prefix "z" returns Some(3).
fn mias_block(name: &str, prefix: &str) -> Option<i64> {
    let lname = name.to_ascii_lowercase();
    for part in lname.split('_') {
        if let Some(rest) = part.strip_prefix(prefix) {
            if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
                return rest.parse::<i64>().ok();
            }
        }
    }
    None
}

/// Extract the trailing tile-column index from a MIAS tile filename, e.g.
/// `mode2_z003_t001_im0_2.tif` -> the bare `2` block after `im<r>_` -> Some(2).
/// In the MIAS convention the last underscore-separated block before the
/// extension is the tile column (a bare integer with no alphabetic prefix).
fn mias_trailing_col(name: &str) -> Option<i64> {
    // Strip extension.
    let stem = name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name);
    let last = stem.rsplit('_').next()?;
    if !last.is_empty() && last.chars().all(|c| c.is_ascii_digit()) {
        last.parse::<i64>().ok()
    } else {
        None
    }
}

/// Identify whether a directory name is a MIAS well directory.
fn is_well_dir_name(name: &str) -> bool {
    if name.starts_with("Well") {
        return true;
    }
    // Four-digit well directory in the alternate layout.
    name.len() == 4 && name.chars().all(|c| c.is_ascii_digit())
}

fn well_number_from_name(name: &str) -> i64 {
    let stripped = name.trim_start_matches("Well");
    stripped.trim().parse::<i64>().map(|v| v - 1).unwrap_or(0)
}

impl MiasReader {
    /// Locate the plate directory and enumerate well directories given a TIFF
    /// (or well directory) path inside a MIAS hierarchy.
    fn build(&mut self, id: &Path) -> Result<()> {
        let base = id.canonicalize().unwrap_or_else(|_| id.to_path_buf());

        // The well directory is the parent of a TIFF, or `id` itself when a
        // directory is given.  The plate directory is the parent of the well.
        let well_dir = if base.is_dir() {
            base.clone()
        } else {
            base.parent()
                .map(|p| p.to_path_buf())
                .unwrap_or(base.clone())
        };
        let plate_dir = well_dir
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or(well_dir.clone());

        // Enumerate well directories under the plate.
        let mut well_dirs: Vec<PathBuf> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&plate_dir) {
            let mut names: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
            names.sort();
            for p in names {
                if p.is_dir() {
                    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    if is_well_dir_name(name) && dir_has_tiff_or_subdir(&p) {
                        well_dirs.push(p);
                    }
                }
            }
        }
        // Fallback: treat the single given well directory as the only well.
        if well_dirs.is_empty() {
            well_dirs.push(well_dir.clone());
        }

        let mut wells = Vec::new();
        for wd in &well_dirs {
            let mut tiffs = collect_well_tiffs(wd);
            tiffs.sort();
            if tiffs.is_empty() {
                continue;
            }

            // Determine the dimension counts from distinct block values.
            let mut z_vals: Vec<i64> = Vec::new();
            let mut t_vals: Vec<i64> = Vec::new();
            let mut c_vals: Vec<i64> = Vec::new();
            let mut im_rows: Vec<i64> = Vec::new();
            let mut im_cols: Vec<i64> = Vec::new();
            for t in &tiffs {
                let name = t.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if let Some(z) = mias_block(name, "z") {
                    if !z_vals.contains(&z) {
                        z_vals.push(z);
                    }
                }
                if let Some(tt) = mias_block(name, "t") {
                    if !t_vals.contains(&tt) {
                        t_vals.push(tt);
                    }
                }
                if let Some(c) = mias_block(name, "mode") {
                    if !c_vals.contains(&c) {
                        c_vals.push(c);
                    }
                }
                if let Some(im) = mias_block(name, "im") {
                    if !im_rows.contains(&im) {
                        im_rows.push(im);
                    }
                    // The tile column is the trailing bare-integer block; it is
                    // only meaningful for tiled mosaics (those with an "im" row
                    // block), per MIASReader's FilePattern handling.
                    if let Some(col) = mias_trailing_col(name) {
                        if !im_cols.contains(&col) {
                            im_cols.push(col);
                        }
                    }
                }
            }
            let size_z = (z_vals.len() as u32).max(1);
            let size_t = (t_vals.len() as u32).max(1);
            let size_c = (c_vals.len() as u32).max(1);
            if im_rows.len() as u32 > self.tile_rows {
                self.tile_rows = im_rows.len() as u32;
            }
            if im_cols.len() as u32 > self.tile_cols {
                self.tile_cols = im_cols.len() as u32;
            }

            let name = wd.file_name().and_then(|n| n.to_str()).unwrap_or("");
            wells.push(MiasWell {
                tiffs,
                size_z,
                size_c,
                size_t,
                well_number: well_number_from_name(name),
            });
        }

        if wells.is_empty() {
            return Err(BioFormatsError::UnsupportedFormat(
                "MIAS: no TIFF files found in any well directory".into(),
            ));
        }

        if self.tile_cols == 0 {
            self.tile_cols = 1;
        }
        if self.tile_rows == 0 {
            self.tile_rows = 1;
        }

        // Probe the first TIFF for pixel parameters (assume uniform).
        self.tiff_reader.set_id(&wells[0].tiffs[0])?;
        let tm = self.tiff_reader.metadata();
        let tile_w = tm.size_x;
        let tile_h = tm.size_y;
        let pixel_type = tm.pixel_type;
        let bits = tm.bits_per_pixel;
        let little_endian = tm.is_little_endian;
        let tiff_c = tm.size_c.max(1);
        let is_rgb = tm.is_rgb;
        let _ = self.tiff_reader.close();

        for w in &wells {
            let logical_planes = w
                .size_z
                .checked_mul(w.size_t)
                .and_then(|n| n.checked_mul(w.size_c))
                .ok_or_else(|| BioFormatsError::Format("MIAS: image count overflows".into()))?;
            let expected_tiffs = logical_planes
                .checked_mul(self.tile_rows.max(1))
                .and_then(|n| n.checked_mul(self.tile_cols.max(1)))
                .ok_or_else(|| BioFormatsError::Format("MIAS: TIFF count overflows".into()))?;
            if w.tiffs.len() != expected_tiffs as usize {
                return Err(BioFormatsError::Format(format!(
                    "MIAS: well {} references {} TIFF file(s), expected {expected_tiffs}",
                    w.well_number,
                    w.tiffs.len()
                )));
            }
            for tiff in &w.tiffs {
                self.tiff_reader.set_id(tiff)?;
                let tm = self.tiff_reader.metadata();
                let (size_x, size_y, this_pixel_type, this_bits, pages) = (
                    tm.size_x,
                    tm.size_y,
                    tm.pixel_type,
                    tm.bits_per_pixel,
                    tm.image_count.max(1),
                );
                let _ = self.tiff_reader.close();
                if size_x != tile_w || size_y != tile_h {
                    return Err(BioFormatsError::Format(format!(
                        "MIAS: companion TIFF {} has dimensions {}x{}, expected {tile_w}x{tile_h}",
                        tiff.display(),
                        size_x,
                        size_y
                    )));
                }
                if this_pixel_type != pixel_type || this_bits != bits {
                    return Err(BioFormatsError::Format(format!(
                        "MIAS: companion TIFF {} has inconsistent pixel type",
                        tiff.display()
                    )));
                }
                if pages != 1 {
                    return Err(BioFormatsError::Format(format!(
                        "MIAS: companion TIFF {} has {} page(s), expected 1",
                        tiff.display(),
                        pages
                    )));
                }
            }
        }

        let mut series = Vec::with_capacity(wells.len());
        for w in &wells {
            let size_c = w.size_c * tiff_c;
            let mut meta_map = HashMap::new();
            meta_map.insert(
                "format".to_string(),
                crate::common::metadata::MetadataValue::String("MIAS".into()),
            );
            meta_map.insert(
                "well_number".to_string(),
                crate::common::metadata::MetadataValue::Int(w.well_number),
            );
            let image_count = (w.size_z * w.size_t * w.size_c).max(1);
            let size_x = tile_w
                .checked_mul(self.tile_cols)
                .ok_or_else(|| BioFormatsError::Format("MIAS: mosaic width overflows".into()))?;
            let size_y = tile_h
                .checked_mul(self.tile_rows)
                .ok_or_else(|| BioFormatsError::Format("MIAS: mosaic height overflows".into()))?;
            series.push(ImageMetadata {
                size_x,
                size_y,
                size_z: w.size_z,
                size_c,
                size_t: w.size_t,
                pixel_type,
                bits_per_pixel: bits,
                image_count,
                dimension_order: DimensionOrder::XYZCT,
                is_rgb,
                is_interleaved: false,
                is_indexed: false,
                is_little_endian: little_endian,
                resolution_count: 1,
                series_metadata: meta_map,
                lookup_table: None,
                modulo_z: None,
                modulo_c: None,
                modulo_t: None,
            });
        }

        self.wells = wells;
        self.series = series;
        self.current_series = 0;
        Ok(())
    }
}

fn dir_has_tiff_or_subdir(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries.flatten().any(|e| {
                let p = e.path();
                p.is_dir()
                    || p.file_name()
                        .and_then(|n| n.to_str())
                        .map(is_mias_tiff)
                        .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

/// Collect TIFFs from a well directory; if none are present, descend into
/// single-character channel subdirectories (the alternate MIAS layout).
fn collect_well_tiffs(well_dir: &Path) -> Vec<PathBuf> {
    let mut tiffs = Vec::new();
    if let Ok(entries) = std::fs::read_dir(well_dir) {
        let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        paths.sort();
        for p in &paths {
            if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                if is_mias_tiff(name) {
                    tiffs.push(p.clone());
                }
            }
        }
        if tiffs.is_empty() {
            for p in &paths {
                if p.is_dir() {
                    if let Ok(sub) = std::fs::read_dir(p) {
                        let mut subpaths: Vec<PathBuf> = sub.flatten().map(|e| e.path()).collect();
                        subpaths.sort();
                        for sp in subpaths {
                            if let Some(name) = sp.file_name().and_then(|n| n.to_str()) {
                                if is_mias_tiff(name) {
                                    tiffs.push(sp);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    tiffs
}

impl FormatReader for MiasReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        // A MIAS TIFF lives in a Well<xxxx> directory and uses the
        // mode/z/t naming convention.
        if !path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("tif") || e.eq_ignore_ascii_case("tiff"))
            .unwrap_or(false)
        {
            return false;
        }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let in_well_dir = path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .map(is_well_dir_name)
            .unwrap_or(false);
        in_well_dir && (mias_block(name, "mode").is_some() || mias_block(name, "z").is_some())
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        // Robustly reject any .tif/.tiff that is not a genuine MIAS dataset so
        // that plain TIFFs fall through to the generic TiffReader. A real MIAS
        // file lives in a Well<xxxx> directory and uses the mode/z/t naming
        // convention (the same guard the registry uses before the TIFF magic
        // pass). Directory inputs (a well/plate dir) are allowed through.
        if !path.is_dir() && !self.is_this_type_by_name(path) {
            return Err(BioFormatsError::UnsupportedFormat(
                "MIAS: file is not a Well<xxxx>/mode<c>_z<zzz>_t<ttt> TIFF dataset".into(),
            ));
        }
        self.tile_rows = 1;
        self.tile_cols = 1;
        self.build(path)?;
        self.tiff_loaded = false;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.wells.clear();
        self.series.clear();
        self.current_series = 0;
        self.tile_rows = 1;
        self.tile_cols = 1;
        if self.tiff_loaded {
            let _ = self.tiff_reader.close();
            self.tiff_loaded = false;
        }
        Ok(())
    }

    fn series_count(&self) -> usize {
        self.series.len()
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.series.is_empty() {
            return Err(BioFormatsError::NotInitialized);
        }
        if s >= self.series_count() {
            Err(BioFormatsError::SeriesOutOfRange(s))
        } else {
            self.current_series = s;
            Ok(())
        }
    }

    fn series(&self) -> usize {
        self.current_series
    }

    fn metadata(&self) -> &ImageMetadata {
        self.series
            .get(self.current_series)
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self
            .series
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let tile_rows = self.tile_rows.max(1);
        let tile_cols = self.tile_cols.max(1);

        // Non-tiled case: plane index maps directly to tiffs[series][no].
        if tile_rows == 1 && tile_cols == 1 {
            let well = self
                .wells
                .get(self.current_series)
                .ok_or(BioFormatsError::NotInitialized)?;
            let tiff_path = well
                .tiffs
                .get(plane_index as usize)
                .ok_or(BioFormatsError::PlaneOutOfRange(plane_index))?
                .clone();
            if self.tiff_loaded {
                let _ = self.tiff_reader.close();
            }
            self.tiff_reader.set_id(&tiff_path)?;
            self.tiff_loaded = true;
            return self.tiff_reader.open_bytes(0);
        }

        // Tiled mosaic: assemble all tiles of this plane into the full plane.
        // Tile (row, col) is the TIFF at index (no*tileRows + row)*tileCols + col
        // and is placed at output position (col*tileWidth, row*tileHeight),
        // matching MIASReader.openBytes / getTile.
        let full_w = meta.size_x as usize;
        let full_h = meta.size_y as usize;
        let bps = meta.pixel_type.bytes_per_sample();
        let rgb = meta.is_rgb;
        let samples = if rgb { meta.size_c.max(1) as usize } else { 1 };
        // bytes per output (full) row across all samples for the non-interleaved
        // layout used by the underlying TIFF reader is handled per-tile below.
        let mut out = vec![0u8; full_w * full_h * bps * samples];
        let out_row_len = full_w * bps * samples;

        for row in 0..tile_rows {
            for col in 0..tile_cols {
                let tile_index = ((plane_index * tile_rows + row) * tile_cols + col) as usize;
                let tiff_path = {
                    let well = self
                        .wells
                        .get(self.current_series)
                        .ok_or(BioFormatsError::NotInitialized)?;
                    match well.tiffs.get(tile_index) {
                        Some(p) => p.clone(),
                        None => continue, // missing tile -> leave zero-filled
                    }
                };
                if self.tiff_loaded {
                    let _ = self.tiff_reader.close();
                }
                self.tiff_reader.set_id(&tiff_path)?;
                self.tiff_loaded = true;
                let tile = self.tiff_reader.open_bytes(0)?;

                let tm = self.tiff_reader.metadata();
                let tile_w = tm.size_x as usize;
                let tile_h = tm.size_y as usize;
                let tile_row_len = tile_w * bps * samples;

                let x_off = col as usize * tile_w * bps * samples;
                let y_off = row as usize * tile_h;
                // Copy each tile row into the output, clipping at the edges.
                for trow in 0..tile_h {
                    let out_y = y_off + trow;
                    if out_y >= full_h {
                        break;
                    }
                    let src = &tile[trow * tile_row_len..(trow + 1) * tile_row_len];
                    let dst_start = out_y * out_row_len + x_off;
                    let copy_len = tile_row_len.min(out_row_len.saturating_sub(x_off));
                    out[dst_start..dst_start + copy_len].copy_from_slice(&src[..copy_len]);
                }
            }
        }
        Ok(out)
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
        let meta = self
            .series
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        crop_full_plane("MIAS", &full, meta, 1, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self
            .series
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}
