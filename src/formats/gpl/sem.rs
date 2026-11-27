//! Scanning Electron Microscopy (SEM) and related format readers.
//!
//! Includes binary readers for INR, FEI/Philips, Veeco/Nanoscope, and several
//! SEM-adjacent formats. Readers without a decoded native layout require
//! explicit strict raw fixtures rather than heuristic dimensions.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, LookupTable, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;

// ===========================================================================
// Real binary reader 1 — INR format
// ===========================================================================

/// INRIMAGE-4 volumetric format (`.inr`).
///
/// Header is 256 ASCII bytes with `#INRIMAGE-4#{` magic, followed by raw
/// pixel data. Key=value pairs in the header define dimensions and pixel type.
pub struct InrReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl InrReader {
    pub fn new() -> Self {
        InrReader {
            path: None,
            meta: None,
        }
    }
}

impl Default for InrReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for InrReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("inr"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.len() >= 9 && &header[0..9] == b"#INRIMAGE"
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;

        // Header is first 256 bytes interpreted as ASCII text
        if data.len() < 256 || !data.starts_with(b"#INRIMAGE-4#{") {
            return Err(BioFormatsError::UnsupportedFormat(
                "INR file is missing the 256-byte INRIMAGE-4 header".into(),
            ));
        }
        let header_bytes = &data[..256];
        let header_text = String::from_utf8_lossy(header_bytes);

        let mut size_x: Option<u32> = None;
        let mut size_y: Option<u32> = None;
        let mut size_z: u32 = 1;
        let mut size_t: u32 = 1;
        let mut bpp: Option<u32> = None;
        let mut is_signed = false;
        let mut is_float = false;
        let mut is_little_endian = false;
        let mut physical_size_x: Option<f64> = None;
        let mut physical_size_y: Option<f64> = None;
        let mut physical_size_z: Option<f64> = None;

        for line in header_text.split('\n') {
            let line = line.trim();
            if let Some(pos) = line.find('=') {
                let key = line[..pos].trim();
                let val = line[pos + 1..].trim();
                match key {
                    "XDIM" => {
                        if let Ok(n) = val.parse::<u32>() {
                            size_x = Some(n);
                        }
                    }
                    "YDIM" => {
                        if let Ok(n) = val.parse::<u32>() {
                            size_y = Some(n);
                        }
                    }
                    "ZDIM" => {
                        if let Ok(n) = val.parse::<u32>() {
                            size_z = n;
                        }
                    }
                    "VDIM" => {
                        // Java INRReader.java:124-126 maps VDIM -> sizeT
                        if let Ok(n) = val.parse::<u32>() {
                            size_t = n;
                        }
                    }
                    "PIXSIZE" => {
                        // Format: "N bits"
                        if let Some(n_str) = val.split_whitespace().next() {
                            if let Ok(n) = n_str.parse::<u32>() {
                                bpp = Some(n);
                            }
                        }
                    }
                    "TYPE" => {
                        let lower = val.to_ascii_lowercase();
                        is_signed = lower.starts_with("signed");
                        is_float = lower.starts_with("float");
                    }
                    "CPU" => {
                        let lower = val.to_ascii_lowercase();
                        is_little_endian = matches!(lower.as_str(), "decm" | "pc" | "alpha");
                    }
                    "VX" => {
                        physical_size_x = val.parse::<f64>().ok();
                    }
                    "VY" => {
                        physical_size_y = val.parse::<f64>().ok();
                    }
                    "VZ" => {
                        physical_size_z = val.parse::<f64>().ok();
                    }
                    _ => {}
                }
            }
        }

        let size_x = size_x
            .filter(|&v| v > 0)
            .ok_or_else(|| BioFormatsError::UnsupportedFormat("INR header missing XDIM".into()))?;
        let size_y = size_y
            .filter(|&v| v > 0)
            .ok_or_else(|| BioFormatsError::UnsupportedFormat("INR header missing YDIM".into()))?;
        let bpp = bpp.ok_or_else(|| {
            BioFormatsError::UnsupportedFormat("INR header missing PIXSIZE".into())
        })?;
        let bytes = bpp / 8;
        let pixel_type = match bytes {
            1 if is_signed => PixelType::Int8,
            1 => PixelType::Uint8,
            2 if is_signed => PixelType::Int16,
            2 => PixelType::Uint16,
            4 if is_float => PixelType::Float32,
            4 if is_signed => PixelType::Int32,
            4 => PixelType::Uint32,
            8 => PixelType::Float64,
            _ => {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "INR unsupported pixel size: {bpp} bits"
                )));
            }
        };

        // Java forces sizeC = 1 and imageCount = sizeZ * sizeT * sizeC.
        let size_c: u32 = 1;
        if size_z == 0 || size_t == 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "INR header dimensions must be positive".into(),
            ));
        }
        let image_count = size_z
            .checked_mul(size_t)
            .and_then(|v| v.checked_mul(size_c))
            .ok_or_else(|| BioFormatsError::Format("INR image count overflows".into()))?;
        let bps = (bpp / 8) as u64;
        let expected = 256u64
            .checked_add(
                (size_x as u64)
                    .checked_mul(size_y as u64)
                    .and_then(|v| v.checked_mul(image_count as u64))
                    .and_then(|v| v.checked_mul(bps))
                    .ok_or_else(|| BioFormatsError::Format("INR image size overflows".into()))?,
            )
            .ok_or_else(|| BioFormatsError::Format("INR image size overflows".into()))?;
        if (data.len() as u64) < expected {
            return Err(BioFormatsError::UnsupportedFormat(
                "INR pixel payload is shorter than declared dimensions".into(),
            ));
        }

        let mut series_metadata = HashMap::new();
        if let Some(v) = physical_size_x.filter(|v| *v > 0.0) {
            series_metadata.insert("PhysicalSizeX".into(), MetadataValue::Float(v));
        }
        if let Some(v) = physical_size_y.filter(|v| *v > 0.0) {
            series_metadata.insert("PhysicalSizeY".into(), MetadataValue::Float(v));
        }
        if let Some(v) = physical_size_z.filter(|v| *v > 0.0) {
            series_metadata.insert("PhysicalSizeZ".into(), MetadataValue::Float(v));
        }

        self.path = Some(path.to_path_buf());
        self.meta = Some(ImageMetadata {
            size_x,
            size_y,
            size_z,
            size_c,
            size_t,
            pixel_type,
            bits_per_pixel: (bpp) as u16,
            image_count,
            dimension_order: DimensionOrder::XYZTC,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian,
            resolution_count: 1,
            thumbnail: false,
            series_metadata,
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        });
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
            return Err(BioFormatsError::NotInitialized);
        }
        if s == 0 {
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
        let bps = (meta.bits_per_pixel / 8) as usize;
        let plane_bytes = meta.size_x as usize * meta.size_y as usize * bps;
        let offset = 256u64 + (plane_index as u64) * (plane_bytes as u64);

        let path = self.path.clone().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = std::fs::File::open(&path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(offset))
            .map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; plane_bytes];
        f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
        Ok(buf)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        _x: u32,
        _y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let meta = self
            .meta
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?
            .clone();
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        // Read full plane then crop (simple approach)
        let full = self.open_bytes(plane_index)?;
        crop_full_plane("INR", &full, &meta, 1, _x, _y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn resolution_count(&self) -> usize {
        1
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if level != 0 {
            Err(BioFormatsError::Format(format!(
                "resolution {} out of range",
                level
            )))
        } else {
            Ok(())
        }
    }
}

// ===========================================================================
// Real binary reader 2 — FEI/Philips XL SEM
// ===========================================================================

const FEI_PHILIPS_MAGIC: &[u8; 2] = b"XL";
const FEI_INVALID_PIXELS: u32 = 112;

/// FEI/Philips XL `.img` SEM files.
///
/// Ported from Bio-Formats `FEIReader`: the header stores the physical scan
/// parameters at fixed offsets, width/height at offset 514, and pixels as an
/// 8-bit grayscale plane split into four row passes and two column passes.
pub struct FeiReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    header_size: u64,
}

impl FeiReader {
    pub fn new() -> Self {
        FeiReader {
            path: None,
            meta: None,
            header_size: 0,
        }
    }
}

impl Default for FeiReader {
    fn default() -> Self {
        Self::new()
    }
}

fn read_le_u16_at(data: &[u8], offset: usize, label: &str) -> Result<u16> {
    let bytes = data.get(offset..offset + 2).ok_or_else(|| {
        BioFormatsError::UnsupportedFormat(format!("FEI/Philips header missing {label}"))
    })?;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn read_le_f32_at(data: &[u8], offset: usize) -> Option<f32> {
    let bytes = data.get(offset..offset + 4)?;
    Some(f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

impl FormatReader for FeiReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("img"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.starts_with(FEI_PHILIPS_MAGIC)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        if !self.is_this_type_by_bytes(&data) {
            return Err(BioFormatsError::UnsupportedFormat(
                "FEI/Philips IMG header does not start with XL".into(),
            ));
        }

        let stored_width = read_le_u16_at(&data, 514, "width")? as u32;
        let height = read_le_u16_at(&data, 516, "height")? as u32;
        let header_size = read_le_u16_at(&data, 522, "pixel offset")? as u64;
        if stored_width <= FEI_INVALID_PIXELS || height == 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "FEI/Philips IMG header contains invalid dimensions or pixel offset".into(),
            ));
        }

        let width = stored_width - FEI_INVALID_PIXELS;
        let mut series_metadata = HashMap::new();
        if let Some(v) = read_le_f32_at(&data, 44) {
            series_metadata.insert("Magnification".into(), MetadataValue::Float(v as f64));
        }
        if let Some(v) = read_le_f32_at(&data, 48) {
            series_metadata.insert("kV".into(), MetadataValue::Float((v / 1000.0) as f64));
        }
        if let Some(v) = read_le_f32_at(&data, 52) {
            series_metadata.insert("Working distance".into(), MetadataValue::Float(v as f64));
        }
        if let Some(v) = read_le_f32_at(&data, 68) {
            series_metadata.insert("Spot".into(), MetadataValue::Float(v as f64));
        }

        self.path = Some(path.to_path_buf());
        self.header_size = header_size;
        self.meta = Some(ImageMetadata {
            size_x: width,
            size_y: height,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type: PixelType::Uint8,
            bits_per_pixel: 8,
            image_count: 1,
            dimension_order: DimensionOrder::XYCZT,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: true,
            resolution_count: 1,
            thumbnail: false,
            series_metadata,
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        });
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.header_size = 0;
        Ok(())
    }

    fn series_count(&self) -> usize {
        usize::from(self.meta.is_some())
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.meta.is_none() {
            return Err(BioFormatsError::NotInitialized);
        }
        if s == 0 {
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

        let path = self.path.clone().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = std::fs::File::open(&path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(self.header_size))
            .map_err(BioFormatsError::Io)?;

        let width = meta.size_x as usize;
        let height = meta.size_y as usize;
        if width % 2 != 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "FEI/Philips IMG width must be even for interlaced decode".into(),
            ));
        }
        let segment_len = width / 2;
        let invalid_len = (FEI_INVALID_PIXELS / 2) as usize;
        let mut segment = vec![0u8; segment_len];
        let mut invalid = vec![0u8; invalid_len];
        let mut plane = vec![0u8; width * height];

        for row_pass in 0..4 {
            let mut row = row_pass;
            while row < height {
                for col_pass in 0..2 {
                    f.read_exact(&mut segment).map_err(BioFormatsError::Io)?;
                    f.read_exact(&mut invalid).map_err(BioFormatsError::Io)?;
                    let mut col = col_pass;
                    while col < width {
                        plane[row * width + col] = segment[col / 2];
                        col += 2;
                    }
                }
                row += 4;
            }
        }

        Ok(plane)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let meta = self
            .meta
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?
            .clone();
        let full = self.open_bytes(plane_index)?;
        crop_full_plane("FEI/Philips IMG", &full, &meta, 1, x, y, w, h)
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

// ===========================================================================
// Real binary reader 3 — Veeco/Nanoscope AFM
// ===========================================================================

/// Veeco/Bruker Nanoscope AFM format (numeric extensions like `.001`, `.afm`).
///
/// Text header followed by raw binary pixel data. Detects via `*` first byte
/// and "NANOSCOPE" in the first 30 bytes.
pub struct VeecoReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    data_offset: usize,
    source: VeecoSource,
    hdf_pixels: Option<Vec<u8>>,
}

impl VeecoReader {
    pub fn new() -> Self {
        VeecoReader {
            path: None,
            meta: None,
            data_offset: 0,
            source: VeecoSource::Nanoscope,
            hdf_pixels: None,
        }
    }
}

impl Default for VeecoReader {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VeecoSource {
    Nanoscope,
    Hdf,
}

fn is_veeco_hdf5_signature(header: &[u8]) -> bool {
    header.len() >= 8 && header[..8] == [0x89, b'H', b'D', b'F', 0x0d, 0x0a, 0x1a, 0x0a]
}

fn veeco_first_2d_hdf5_dataset(
    file: &hdf5_pure_rust::File,
    group_path: &str,
) -> Option<(String, hdf5_pure_rust::Dataset)> {
    let group = file.group(group_path).ok()?;
    let mut members = group.member_names().ok()?;
    members.sort();
    for member in members {
        let path = if group_path == "/" {
            format!("/{member}")
        } else {
            format!("{group_path}/{member}")
        };
        if let Ok(ds) = file.dataset(&path) {
            if ds.shape().ok().is_some_and(|shape| shape.len() == 2) {
                return Some((path, ds));
            }
        } else if let Some(found) = veeco_first_2d_hdf5_dataset(file, &path) {
            return Some(found);
        }
    }
    None
}

fn veeco_unpack_little_endian(values: &[i16]) -> bool {
    let mut native_min = 0i16;
    let mut native_max = 0i16;
    let mut swapped_min = 0i16;
    let mut swapped_max = 0i16;
    for &value in values {
        native_min = native_min.min(value);
        native_max = native_max.max(value);
        let swapped = value.swap_bytes();
        swapped_min = swapped_min.min(swapped);
        swapped_max = swapped_max.max(swapped);
    }
    native_min <= swapped_min && native_max >= swapped_max
}

fn parse_veeco_hdf(path: &Path) -> Result<(ImageMetadata, Vec<u8>)> {
    use hdf5_pure_rust::format::messages::datatype::DatatypeClass;

    let file = hdf5_pure_rust::File::open(path)
        .map_err(|e| BioFormatsError::Format(format!("Veeco HDF5 open error: {e}")))?;
    let (dataset_path, ds) = veeco_first_2d_hdf5_dataset(&file, "/").ok_or_else(|| {
        BioFormatsError::UnsupportedFormat("Veeco HDF: no 2D image dataset found".into())
    })?;
    let shape = ds
        .shape()
        .map_err(|e| BioFormatsError::Format(format!("Veeco HDF shape: {e}")))?;
    let height = shape[0] as u32;
    let width = shape[1] as u32;
    if width == 0 || height == 0 {
        return Err(BioFormatsError::UnsupportedFormat(
            "Veeco HDF image dimensions must be non-zero".into(),
        ));
    }

    let dtype = ds
        .dtype()
        .map_err(|e| BioFormatsError::Format(format!("Veeco HDF dtype: {e}")))?;
    let dtype_size = dtype.size();
    let pixels = match (dtype.class(), dtype_size) {
        (DatatypeClass::FixedPoint, 1) => ds
            .read::<i8>()
            .map_err(|e| BioFormatsError::Format(format!("Veeco HDF read: {e}")))?
            .into_iter()
            .map(|v| v as u8)
            .collect::<Vec<_>>(),
        (DatatypeClass::FixedPoint, 2) => {
            let values = ds
                .read::<i16>()
                .map_err(|e| BioFormatsError::Format(format!("Veeco HDF read: {e}")))?;
            let unpack_little = veeco_unpack_little_endian(&values);
            let mut out = Vec::with_capacity(values.len() * 2);
            for value in values {
                if unpack_little {
                    out.extend_from_slice(&value.to_le_bytes());
                } else {
                    out.extend_from_slice(&value.to_be_bytes());
                }
            }
            out
        }
        (class, size) => {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "Veeco HDF: unsupported image datatype {class:?} ({size} bytes)"
            )));
        }
    };

    let bits_per_pixel = if dtype_size == 1 { 8 } else { 16 };
    let pixel_type = if dtype_size == 1 {
        PixelType::Int8
    } else {
        PixelType::Int16
    };
    let mut series_metadata = HashMap::new();
    series_metadata.insert(
        "Veeco image dataset".into(),
        MetadataValue::String(dataset_path),
    );

    Ok((
        ImageMetadata {
            size_x: width,
            size_y: height,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type,
            bits_per_pixel: bits_per_pixel.max(0) as u16,
            image_count: 1,
            dimension_order: DimensionOrder::XYCZT,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: false,
            resolution_count: 1,
            thumbnail: false,
            series_metadata,
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        },
        pixels,
    ))
}

impl FormatReader for VeecoReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        // Java VeecoReader handles .hdf. Keep the existing extra Nanoscope
        // surface (.afm and numeric extensions) in a separate decode path.
        ext.eq_ignore_ascii_case("hdf")
            || ext.eq_ignore_ascii_case("afm")
            || (!ext.is_empty() && ext.len() <= 3 && ext.chars().all(|c| c.is_ascii_digit()))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        if header.is_empty() || header[0] != b'*' {
            return false;
        }
        let s = String::from_utf8_lossy(&header[..header.len().min(30)]);
        s.to_ascii_uppercase().contains("NANOSCOPE")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        if is_veeco_hdf5_signature(&data) {
            let (meta, pixels) = parse_veeco_hdf(path)?;
            self.path = Some(path.to_path_buf());
            self.meta = Some(meta);
            self.source = VeecoSource::Hdf;
            self.hdf_pixels = Some(pixels);
            return Ok(());
        }

        let text = String::from_utf8_lossy(&data).into_owned();

        let mut width: Option<u32> = None;
        let mut height: Option<u32> = None;
        let mut bpp: Option<u32> = None;
        let mut data_offset: Option<usize> = None;

        for line in text.lines() {
            if line.contains("\\Samps/line:") {
                if let Some(val) = line.split_whitespace().last() {
                    if let Ok(n) = val.parse::<u32>() {
                        width = Some(n);
                    }
                }
            } else if line.contains("\\Number of lines:") {
                if let Some(val) = line.split_whitespace().last() {
                    if let Ok(n) = val.parse::<u32>() {
                        height = Some(n);
                    }
                }
            } else if line.contains("\\Bytes/pixel:") {
                if let Some(val) = line.split_whitespace().last() {
                    if let Ok(n) = val.parse::<u32>() {
                        bpp = Some(n);
                    }
                }
            } else if line.contains("\\Data offset:") {
                if let Some(val) = line.split_whitespace().last() {
                    if let Ok(n) = val.parse::<usize>() {
                        data_offset = Some(n);
                    }
                }
            }
        }

        let width = width.filter(|&v| v > 0).ok_or_else(|| {
            BioFormatsError::UnsupportedFormat("Nanoscope header missing Samps/line".into())
        })?;
        let height = height.filter(|&v| v > 0).ok_or_else(|| {
            BioFormatsError::UnsupportedFormat("Nanoscope header missing Number of lines".into())
        })?;
        let bpp = bpp.filter(|&v| v == 1 || v == 2).ok_or_else(|| {
            BioFormatsError::UnsupportedFormat(
                "Nanoscope header missing supported Bytes/pixel".into(),
            )
        })?;
        let data_offset = data_offset.ok_or_else(|| {
            BioFormatsError::UnsupportedFormat("Nanoscope header missing Data offset".into())
        })?;
        let expected = (data_offset as u64)
            .checked_add(
                (width as u64)
                    .saturating_mul(height as u64)
                    .saturating_mul(bpp as u64),
            )
            .ok_or_else(|| BioFormatsError::Format("Nanoscope plane size overflows".into()))?;
        if expected > data.len() as u64 {
            return Err(BioFormatsError::UnsupportedFormat(
                "Nanoscope pixel payload is shorter than declared dimensions".into(),
            ));
        }

        let pixel_type = if bpp == 1 {
            PixelType::Uint8
        } else {
            PixelType::Uint16
        };
        let bits_per_pixel = (bpp * 8) as u8;

        self.data_offset = data_offset;
        self.path = Some(path.to_path_buf());
        self.source = VeecoSource::Nanoscope;
        self.meta = Some(ImageMetadata {
            size_x: width,
            size_y: height,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type,
            bits_per_pixel: bits_per_pixel.into(),
            image_count: 1,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: true,
            resolution_count: 1,
            thumbnail: false,
            series_metadata: HashMap::new(),
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        });
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.data_offset = 0;
        self.source = VeecoSource::Nanoscope;
        self.hdf_pixels = None;
        Ok(())
    }

    fn series_count(&self) -> usize {
        usize::from(self.meta.is_some())
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.meta.is_none() {
            return Err(BioFormatsError::NotInitialized);
        }
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
        self.meta
            .as_ref()
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let bps = (meta.bits_per_pixel / 8) as usize;
        let n_bytes = meta.size_x as usize * meta.size_y as usize * bps;
        if self.source == VeecoSource::Hdf {
            let pixels = self
                .hdf_pixels
                .as_ref()
                .ok_or(BioFormatsError::NotInitialized)?;
            let row_bytes = meta.size_x as usize * bps;
            let mut out = vec![0u8; n_bytes];
            for dst_y in 0..meta.size_y as usize {
                let src_y = meta.size_y as usize - 1 - dst_y;
                let src = src_y * row_bytes;
                let dst = dst_y * row_bytes;
                out[dst..dst + row_bytes].copy_from_slice(&pixels[src..src + row_bytes]);
            }
            return Ok(out);
        }

        let path = self.path.clone().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = std::fs::File::open(&path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(self.data_offset as u64))
            .map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; n_bytes];
        f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
        Ok(buf)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        _x: u32,
        _y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let meta = self
            .meta
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?
            .clone();
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let full = self.open_bytes(plane_index)?;
        crop_full_plane("Nanoscope", &full, &meta, 1, _x, _y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn resolution_count(&self) -> usize {
        1
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if level != 0 {
            Err(BioFormatsError::Format(format!(
                "resolution {} out of range",
                level
            )))
        } else {
            Ok(())
        }
    }
}

// ===========================================================================
// TIFF wrapper — Zeiss
// ===========================================================================

// ---------------------------------------------------------------------------
// ZeissTiffReader
// ---------------------------------------------------------------------------

/// Zeiss AxioVision TIFF reader (`.tif`, `.xml`).
///
/// Java `ZeissTIFFReader` is a grouped AxioVision TIFF/XML reader rather than
/// a generic TIFF wrapper: `isThisType(name, open)` requires a supported suffix,
/// opening the file, and locating the companion `_meta.xml`. This Rust reader
/// keeps pixel decoding delegated to `TiffReader`, but only initializes when the
/// Java companion-file discovery succeeds. Full AxioVision XML plane grouping is
/// not implemented here; unsupported multifile sets are rejected explicitly.
pub struct ZeissTiffReader {
    inner: crate::tiff::TiffReader,
    meta: Option<ImageMetadata>,
}

#[derive(Debug)]
struct ZeissTiffInfo {
    xml_name: PathBuf,
    original_name: PathBuf,
    base_dir: Option<PathBuf>,
    multifile: bool,
}

impl ZeissTiffReader {
    const XML_NAME: &'static str = "_meta.xml";

    pub fn new() -> Self {
        ZeissTiffReader {
            inner: crate::tiff::TiffReader::new(),
            meta: None,
        }
    }

    fn has_supported_suffix(path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("tif") || e.eq_ignore_ascii_case("xml"))
            .unwrap_or(false)
    }

    fn is_tif(path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("tif"))
            .unwrap_or(false)
    }

    fn is_meta_xml(path: &Path) -> bool {
        path.file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.to_ascii_lowercase().ends_with(Self::XML_NAME))
            .unwrap_or(false)
    }

    fn prefixed_meta_path(path: &Path) -> PathBuf {
        let mut s = path.as_os_str().to_os_string();
        s.push(Self::XML_NAME);
        PathBuf::from(s)
    }

    fn extract_filename_from_xml(xml: &str) -> Option<String> {
        let filename_pos = xml.find(">Filename<")?;
        let before = &xml[..filename_pos];
        let value_start = before.rfind("<V")?;
        let value_open_end = before[value_start..].find('>')? + value_start + 1;
        Some(before[value_open_end..].trim().to_string())
    }

    fn case_insensitive_existing_path(path: &Path) -> PathBuf {
        let Some(parent) = path.parent() else {
            return path.to_path_buf();
        };
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            return path.to_path_buf();
        };
        if let Ok(entries) = std::fs::read_dir(parent) {
            for entry in entries.flatten() {
                if entry
                    .file_name()
                    .to_str()
                    .map(|candidate| candidate.eq_ignore_ascii_case(name))
                    .unwrap_or(false)
                {
                    return entry.path();
                }
            }
        }
        path.to_path_buf()
    }

    fn eval_file(path: &Path) -> Result<ZeissTiffInfo> {
        let abs = std::fs::canonicalize(path).map_err(BioFormatsError::Io)?;
        let mut info = if Self::is_tif(&abs) {
            let xml = Self::prefixed_meta_path(&abs);
            if xml.exists() {
                ZeissTiffInfo {
                    xml_name: xml,
                    original_name: abs.clone(),
                    base_dir: None,
                    multifile: false,
                }
            } else {
                let lower_files = PathBuf::from(format!("{}_files", abs.display()));
                let upper_files = PathBuf::from(format!("{}_Files", abs.display()));
                let base = if lower_files.exists() {
                    lower_files
                } else {
                    upper_files
                };
                let xml = base.join(Self::XML_NAME);
                if base.exists() && xml.exists() {
                    ZeissTiffInfo {
                        xml_name: xml,
                        original_name: abs.clone(),
                        base_dir: Some(base),
                        multifile: true,
                    }
                } else {
                    let parent = abs.parent().unwrap_or_else(|| Path::new("."));
                    let xml = parent.join(Self::XML_NAME);
                    if xml.exists() {
                        ZeissTiffInfo {
                            xml_name: xml.clone(),
                            original_name: xml.clone(),
                            base_dir: Some(parent.to_path_buf()),
                            multifile: true,
                        }
                    } else {
                        return Err(BioFormatsError::UnsupportedFormat(
                            "Zeiss TIFF: XML metadata not found".into(),
                        ));
                    }
                }
            }
        } else if Self::is_meta_xml(&abs) {
            if !abs.exists() {
                return Err(BioFormatsError::UnsupportedFormat(
                    "Zeiss TIFF: XML metadata not found".into(),
                ));
            }
            if abs
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.eq_ignore_ascii_case(Self::XML_NAME))
                .unwrap_or(false)
            {
                let parent = abs.parent().unwrap_or_else(|| Path::new(".")).to_path_buf();
                ZeissTiffInfo {
                    xml_name: abs.clone(),
                    original_name: abs.clone(),
                    base_dir: Some(parent),
                    multifile: true,
                }
            } else {
                let original = {
                    let name = abs.as_os_str().to_string_lossy();
                    let trimmed = &name[..name.len() - Self::XML_NAME.len()];
                    PathBuf::from(trimmed)
                };
                if !original.exists() {
                    return Err(BioFormatsError::UnsupportedFormat(
                        "Zeiss TIFF: TIFF image data not found".into(),
                    ));
                }
                ZeissTiffInfo {
                    xml_name: abs.clone(),
                    original_name: original,
                    base_dir: None,
                    multifile: false,
                }
            }
        } else {
            return Err(BioFormatsError::UnsupportedFormat(
                "Zeiss TIFF: invalid AxioVision TIFF/XML suffix".into(),
            ));
        };

        let xml = std::fs::read_to_string(&info.xml_name).map_err(BioFormatsError::Io)?;
        if let Some(filename) = Self::extract_filename_from_xml(&xml) {
            let candidate = if let Some(base_dir) = &info.base_dir {
                base_dir.join(filename)
            } else {
                info.xml_name
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .join(filename)
            };
            info.original_name = Self::case_insensitive_existing_path(&candidate);
        } else if info.original_name == info.xml_name {
            return Err(BioFormatsError::UnsupportedFormat(
                "Zeiss TIFF: image name not found in XML metadata".into(),
            ));
        }

        Ok(info)
    }
}

impl Default for ZeissTiffReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for ZeissTiffReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        Self::has_supported_suffix(path)
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let info = Self::eval_file(path)?;
        if info.multifile {
            return Err(BioFormatsError::UnsupportedFormat(
                "Zeiss TIFF: multifile AxioVision TIFF/XML sets are not yet supported".into(),
            ));
        }

        self.inner.set_id(&info.original_name)?;
        let mut meta = self.inner.metadata().clone();
        meta.is_interleaved = false;
        meta.series_metadata.insert(
            "format".into(),
            MetadataValue::String("Zeiss AxioVision TIFF".into()),
        );
        meta.series_metadata.insert(
            "Zeiss TIFF XML".into(),
            MetadataValue::String(info.xml_name.to_string_lossy().into_owned()),
        );
        self.meta = Some(meta);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.meta = None;
        self.inner.close()
    }

    fn series_count(&self) -> usize {
        if self.meta.is_some() {
            self.inner.series_count()
        } else {
            0
        }
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.meta.is_none() {
            return Err(BioFormatsError::NotInitialized);
        }
        self.inner.set_series(s)
    }

    fn series(&self) -> usize {
        self.inner.series()
    }

    fn metadata(&self) -> &ImageMetadata {
        self.meta
            .as_ref()
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
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
        self.inner.open_bytes_region(plane_index, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        self.inner.open_thumb_bytes(plane_index)
    }

    fn resolution_count(&self) -> usize {
        self.inner.resolution_count()
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        self.inner.set_resolution(level)
    }
}

// ===========================================================================
// Strict raw readers for formats whose native layout is not decoded.
// ===========================================================================

// ===========================================================================
// Shared strict raw helper.
// ===========================================================================

fn unsupported_raw_sem(format_name: &str) -> BioFormatsError {
    BioFormatsError::UnsupportedFormat(format!(
        "{format_name} native binary layout is unsupported unless explicit strict raw data is present; refusing heuristic dimensions"
    ))
}

const JEOL_STRICT_MAGIC: &[u8] = b"BIOFORMATS-RS-JEOL-SEM-STRICT-RAW-V1\n";
const ZEISS_LMS_STRICT_MAGIC: &[u8] = b"BIOFORMATS-RS-ZEISS-LMS-STRICT-RAW-V1\n";

fn read_le_u32_strict(data: &[u8], offset: usize, label: &str, format_name: &str) -> Result<u32> {
    let bytes = data.get(offset..offset + 4).ok_or_else(|| {
        BioFormatsError::UnsupportedFormat(format!("{format_name} strict header missing {label}"))
    })?;
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn read_le_u16_strict(data: &[u8], offset: usize, label: &str, format_name: &str) -> Result<u16> {
    let bytes = data.get(offset..offset + 2).ok_or_else(|| {
        BioFormatsError::UnsupportedFormat(format!("{format_name} strict header missing {label}"))
    })?;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn parse_strict_sem_raw(
    path: &Path,
    magic: &[u8],
    format_name: &str,
) -> Result<(ImageMetadata, u64)> {
    let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
    if !data.starts_with(magic) {
        return Err(unsupported_raw_sem(format_name));
    }

    let width_offset = magic.len();
    let height_offset = width_offset + 4;
    let pixel_type_offset = height_offset + 4;
    let reserved_offset = pixel_type_offset + 2;
    let data_offset = reserved_offset + 2;
    let width = read_le_u32_strict(&data, width_offset, "width", format_name)?;
    let height = read_le_u32_strict(&data, height_offset, "height", format_name)?;
    if width == 0 || height == 0 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "{format_name} strict header dimensions must be non-zero"
        )));
    }

    let pixel_type_code = read_le_u16_strict(&data, pixel_type_offset, "pixel type", format_name)?;
    let (pixel_type, bits_per_pixel) = match pixel_type_code {
        1 => (PixelType::Uint8, 8),
        2 => (PixelType::Uint16, 16),
        _ => {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "{format_name} strict header has unsupported pixel type code {pixel_type_code}"
            )));
        }
    };
    let reserved = read_le_u16_strict(&data, reserved_offset, "reserved field", format_name)?;
    if reserved != 0 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "{format_name} strict header reserved field must be zero"
        )));
    }

    let payload_len = (width as u64)
        .checked_mul(height as u64)
        .and_then(|n| n.checked_mul(pixel_type.bytes_per_sample() as u64))
        .ok_or_else(|| BioFormatsError::Format(format!("{format_name} payload size overflows")))?;
    let expected_len = (data_offset as u64)
        .checked_add(payload_len)
        .ok_or_else(|| BioFormatsError::Format(format!("{format_name} file size overflows")))?;
    if data.len() as u64 != expected_len {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "{format_name} strict payload length mismatch: got {}, expected {expected_len}",
            data.len()
        )));
    }

    Ok((
        ImageMetadata {
            size_x: width,
            size_y: height,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type,
            bits_per_pixel,
            image_count: 1,
            dimension_order: DimensionOrder::XYCZT,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: true,
            resolution_count: 1,
            thumbnail: false,
            series_metadata: HashMap::new(),
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        },
        data_offset as u64,
    ))
}

// ===========================================================================
// Real binary reader — JEOL SEM
// ===========================================================================

/// JEOL SEM data file reader (`.dat`).
///
/// Supports only the conservative BioFormats-rs strict raw subset identified
/// by `BIOFORMATS-RS-JEOL-SEM-STRICT-RAW-V1\n`.
pub struct JeolReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    data_offset: u64,
    plane_bytes: u64,
}

impl JeolReader {
    pub fn new() -> Self {
        JeolReader {
            path: None,
            meta: None,
            data_offset: 0,
            plane_bytes: 0,
        }
    }

    fn resolve_image_path(path: &Path) -> Result<PathBuf> {
        let is_par = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("par"))
            .unwrap_or(false);
        if !is_par {
            return Ok(path.to_path_buf());
        }

        for ext in ["IMG", "DAT", "img", "dat"] {
            let candidate = path.with_extension(ext);
            if candidate.exists() {
                return Ok(candidate);
            }
        }
        Err(BioFormatsError::UnsupportedFormat(
            "JEOL SEM could not find companion image file for .par".into(),
        ))
    }

    fn parse_native(path: &Path) -> Result<(ImageMetadata, u64, u64)> {
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        let mut series_metadata = HashMap::new();
        let (size_x, size_y, pixel_offset) = if data.starts_with(b"MG") {
            let need = 0x63cusize + 8 + 540;
            if data.len() < need {
                return Err(BioFormatsError::UnsupportedFormat(
                    "JEOL MG header is truncated".into(),
                ));
            }
            let size_x = u32::from_le_bytes(data[0x63c..0x640].try_into().unwrap());
            let size_y = u32::from_le_bytes(data[0x640..0x644].try_into().unwrap());
            (size_x, size_y, (0x644 + 540) as u64)
        } else if data.starts_with(b"IM") {
            if data.len() < 4 + 56 {
                return Err(BioFormatsError::UnsupportedFormat(
                    "JEOL IM header is truncated".into(),
                ));
            }
            let comment_len = u16::from_le_bytes(data[2..4].try_into().unwrap()) as u64;
            let pixel_offset = 4u64
                .checked_add(comment_len)
                .and_then(|v| v.checked_add(56))
                .ok_or_else(|| BioFormatsError::Format("JEOL IM pixel offset overflows".into()))?;
            if pixel_offset > data.len() as u64 {
                return Err(BioFormatsError::UnsupportedFormat(
                    "JEOL IM pixel offset points past end of file".into(),
                ));
            }
            let available = data.len() as u64 - pixel_offset;
            (1024, (available / 1024) as u32, pixel_offset)
        } else if data.len() == 1024 * 1024 {
            (1024, 1024, 0)
        } else {
            return Err(unsupported_raw_sem("JEOL SEM"));
        };

        if size_x == 0 || size_y == 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "JEOL SEM dimensions must be non-zero".into(),
            ));
        }
        let plane_bytes = (size_x as u64)
            .checked_mul(size_y as u64)
            .ok_or_else(|| BioFormatsError::Format("JEOL SEM plane size overflows".into()))?;
        let expected = pixel_offset
            .checked_add(plane_bytes)
            .ok_or_else(|| BioFormatsError::Format("JEOL SEM file size overflows".into()))?;
        if (data.len() as u64) < expected {
            return Err(BioFormatsError::UnsupportedFormat(
                "JEOL SEM pixel payload is shorter than declared dimensions".into(),
            ));
        }
        series_metadata.insert(
            "Pixel data offset".into(),
            MetadataValue::Int(pixel_offset as i64),
        );

        Ok((
            ImageMetadata {
                size_x,
                size_y,
                size_z: 1,
                size_c: 1,
                size_t: 1,
                pixel_type: PixelType::Uint8,
                bits_per_pixel: 8,
                image_count: 1,
                dimension_order: DimensionOrder::XYZCT,
                is_rgb: false,
                is_interleaved: false,
                is_indexed: false,
                is_little_endian: true,
                resolution_count: 1,
                thumbnail: false,
                series_metadata,
                lookup_table: None,
                modulo_z: None,
                modulo_c: None,
                modulo_t: None,
            },
            pixel_offset,
            plane_bytes,
        ))
    }
}

impl Default for JeolReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for JeolReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("dat") | Some("img") | Some("par"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.starts_with(JEOL_STRICT_MAGIC)
            || header.starts_with(b"MG")
            || header.starts_with(b"IM")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let image_path = Self::resolve_image_path(path)?;
        let data = std::fs::read(&image_path).map_err(BioFormatsError::Io)?;
        let (meta, data_offset, plane_bytes) = if data.starts_with(JEOL_STRICT_MAGIC) {
            let (meta, data_offset) =
                parse_strict_sem_raw(&image_path, JEOL_STRICT_MAGIC, "JEOL SEM")?;
            let plane_bytes = (meta.size_x as u64)
                .checked_mul(meta.size_y as u64)
                .and_then(|n| n.checked_mul(meta.pixel_type.bytes_per_sample() as u64))
                .ok_or_else(|| BioFormatsError::Format("JEOL SEM plane size overflows".into()))?;
            (meta, data_offset, plane_bytes)
        } else {
            Self::parse_native(&image_path)?
        };
        self.path = Some(image_path);
        self.meta = Some(meta);
        self.data_offset = data_offset;
        self.plane_bytes = plane_bytes;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.data_offset = 0;
        self.plane_bytes = 0;
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

        let n_bytes = usize::try_from(self.plane_bytes)
            .map_err(|_| BioFormatsError::Format("JEOL SEM plane size overflows".into()))?;
        let path = self.path.clone().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = std::fs::File::open(&path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(self.data_offset))
            .map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; n_bytes];
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
        let meta = self
            .meta
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?
            .clone();
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let full = self.open_bytes(plane_index)?;
        crop_full_plane("JEOL SEM", &full, &meta, 1, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn resolution_count(&self) -> usize {
        1
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if level != 0 {
            Err(BioFormatsError::Format(format!(
                "resolution {} out of range",
                level
            )))
        } else {
            Ok(())
        }
    }
}

// ===========================================================================
// Hitachi S-4800 SEM — INI text file + companion pixels file
// ===========================================================================

/// Hitachi S-4800 SEM reader.
///
/// Ported from `HitachiReader.java`. A Hitachi dataset is a `.txt` INI file
/// whose `[SemImageFile]` section names the actual pixels file (`ImageName`),
/// a similarly-named `.tif`, `.bmp`, or `.jpg` placed alongside the `.txt`.
/// Detection requires the magic string `[SemImageFile]` (the file may be
/// either ASCII or UTF-16 encoded). The pixels are read by delegating to the
/// auto-detecting `ImageReader`, exactly as the Java reader delegates to a
/// helper `ImageReader` with `HitachiReader` removed from the class list.
pub struct HitachiReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    /// Resolved path to the companion pixels file (.tif/.bmp/.jpg).
    pixels_file: Option<PathBuf>,
    /// Parsed `[SemImageFile]` key/value pairs.
    ini: HashMap<String, String>,
}

impl HitachiReader {
    const MAGIC: &'static str = "[SemImageFile]";

    pub fn new() -> Self {
        HitachiReader {
            path: None,
            meta: None,
            pixels_file: None,
            ini: HashMap::new(),
        }
    }

    /// Decode the header text as ASCII, falling back to UTF-16 (matching the
    /// Java reader's `new String(b, ENCODING)` then `new String(b, "UTF-16")`).
    fn decode_header(bytes: &[u8]) -> String {
        let ascii = String::from_utf8_lossy(bytes);
        if ascii.contains(Self::MAGIC) {
            return ascii.into_owned();
        }
        // UTF-16: try little-endian then big-endian.
        for be in [false, true] {
            let units: Vec<u16> = bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| {
                    if be {
                        u16::from_be_bytes([c[0], c[1]])
                    } else {
                        u16::from_le_bytes([c[0], c[1]])
                    }
                })
                .collect();
            let s = String::from_utf16_lossy(&units);
            if s.contains(Self::MAGIC) {
                return s;
            }
        }
        ascii.into_owned()
    }

    /// Parse a flat INI: lines `key=value` after the `[SemImageFile]` header.
    fn parse_ini(text: &str) -> HashMap<String, String> {
        let mut map = HashMap::new();
        let mut in_section = false;
        for line in text.lines() {
            let line = line.trim().trim_start_matches('\u{feff}');
            if line.starts_with('[') && line.ends_with(']') {
                in_section = line.eq_ignore_ascii_case(Self::MAGIC);
                continue;
            }
            if !in_section {
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                map.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
        map
    }
}

impl Default for HitachiReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for HitachiReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("txt"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // Accept ASCII or UTF-16 occurrences of the magic.
        Self::decode_header(header).contains(Self::MAGIC)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        // If handed the companion pixels file, redirect to the sibling .txt
        // (Java initFile() does the same).
        let txt_path = if path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("txt"))
            .unwrap_or(false)
        {
            path.to_path_buf()
        } else {
            path.with_extension("txt")
        };

        let bytes = std::fs::read(&txt_path).map_err(BioFormatsError::Io)?;
        let text = Self::decode_header(&bytes);
        if !text.contains(Self::MAGIC) {
            return Err(BioFormatsError::UnsupportedFormat(
                "Hitachi: missing [SemImageFile] section".into(),
            ));
        }
        let ini = Self::parse_ini(&text);

        // Resolve the pixels file: stored ImageName next to the .txt, else
        // fall back to a same-base .tif/.jpg/.bmp.
        let parent = txt_path.parent().unwrap_or_else(|| Path::new("."));
        let mut pixels_file: Option<PathBuf> = None;
        if let Some(name) = ini.get("ImageName") {
            let candidate = parent.join(name);
            if candidate.exists() {
                pixels_file = Some(candidate);
            }
        }
        if pixels_file.is_none() {
            for ext in ["tif", "jpg", "bmp"] {
                let candidate = txt_path.with_extension(ext);
                if candidate.exists() {
                    pixels_file = Some(candidate);
                    break;
                }
            }
        }
        let pixels_file = pixels_file.ok_or_else(|| {
            BioFormatsError::UnsupportedFormat("Hitachi: could not find pixels file".into())
        })?;

        // Delegate to the auto-detecting reader for the companion image.
        let mut helper = crate::registry::ImageReader::open(&pixels_file)?;
        let mut meta = helper.metadata().clone();
        helper.close().ok();

        // Carry the [SemImageFile] metadata into series_metadata.
        for (k, v) in &ini {
            meta.series_metadata.insert(
                k.clone(),
                crate::common::metadata::MetadataValue::String(v.clone()),
            );
        }
        meta.series_metadata.insert(
            "format".into(),
            crate::common::metadata::MetadataValue::String("Hitachi".into()),
        );

        self.ini = ini;
        self.pixels_file = Some(pixels_file);
        self.path = Some(txt_path);
        self.meta = Some(meta);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.pixels_file = None;
        self.ini.clear();
        Ok(())
    }

    fn series_count(&self) -> usize {
        usize::from(self.meta.is_some())
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.meta.is_none() {
            return Err(BioFormatsError::NotInitialized);
        }
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
        self.meta
            .as_ref()
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let pixels = self
            .pixels_file
            .clone()
            .ok_or(BioFormatsError::NotInitialized)?;
        let mut helper = crate::registry::ImageReader::open(&pixels)?;
        let bytes = helper.open_bytes(plane_index);
        helper.close().ok();
        bytes
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let pixels = self
            .pixels_file
            .clone()
            .ok_or(BioFormatsError::NotInitialized)?;
        let mut helper = crate::registry::ImageReader::open(&pixels)?;
        let bytes = helper.open_bytes_region(plane_index, x, y, w, h);
        helper.close().ok();
        bytes
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let pixels = self
            .pixels_file
            .clone()
            .ok_or(BioFormatsError::NotInitialized)?;
        let mut helper = crate::registry::ImageReader::open(&pixels)?;
        let bytes = helper.open_thumb_bytes(plane_index);
        helper.close().ok();
        bytes
    }

    fn resolution_count(&self) -> usize {
        1
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if level != 0 {
            Err(BioFormatsError::Format(format!(
                "resolution {} out of range",
                level
            )))
        } else {
            Ok(())
        }
    }
}

// ===========================================================================
// LEO / Zeiss EM — TIFF with proprietary LEO tag (34118)
// ===========================================================================

/// LEO EM reader (`.sxm`, `.tif`, `.tiff`).
///
/// Ported from `LEOReader.java` (extends `BaseTiffReader`). LEO files are
/// ordinary TIFFs distinguished by the presence of private tag 34118
/// (`LEO_TAG`), an ISO-8859-1 text blob of `AP_`/`DP_`/`SV_` key/value lines.
/// Pixel reading is plain TIFF; this reader delegates pixels to `TiffReader`
/// and parses the LEO tag for metadata.
pub struct LeoReader {
    inner: crate::tiff::TiffReader,
    meta: Option<ImageMetadata>,
    path: Option<PathBuf>,
}

impl LeoReader {
    const LEO_TAG: u16 = 34118;

    pub fn new() -> Self {
        LeoReader {
            inner: crate::tiff::TiffReader::new(),
            meta: None,
            path: None,
        }
    }

    fn parse_value_line(line: &str, time_or_date: bool) -> Option<(&str, &str)> {
        if time_or_date {
            let colon = line.find(':')?;
            let key = &line[..colon];
            if key.chars().last().map(|c| c.is_whitespace()) != Some(true) {
                return None;
            }
            Some((key.trim_end(), &line[colon + 1..]))
        } else {
            let eq = line.find('=')?;
            let before = &line[..eq];
            let after = &line[eq + 1..];
            if before.chars().last().map(|c| c.is_whitespace()) != Some(true)
                || after.chars().next().map(|c| c.is_whitespace()) != Some(true)
            {
                return None;
            }
            Some((before.trim_end(), after.trim_start()))
        }
    }

    fn parse_length_um(value: &str) -> Option<f64> {
        let trimmed = value.trim();
        let mut end = 0usize;
        for (idx, ch) in trimmed.char_indices() {
            if ch.is_ascii_digit() || matches!(ch, '.' | '+' | '-' | 'e' | 'E') {
                end = idx + ch.len_utf8();
            } else if end > 0 {
                break;
            }
        }
        let number: f64 = trimmed.get(..end)?.trim().parse().ok()?;
        if !number.is_finite() || number <= 0.0 {
            return None;
        }
        let unit = trimmed[end..].trim().to_ascii_lowercase();
        let scale = if unit.starts_with("mm") {
            1000.0
        } else if unit.starts_with("nm") {
            0.001
        } else if unit == "m" || unit.starts_with("meter") || unit.starts_with("metre") {
            1_000_000.0
        } else {
            1.0
        };
        Some(number * scale)
    }

    fn parse_length_number(value: &str) -> Option<f64> {
        let trimmed = value.trim();
        let mut end = 0usize;
        for (idx, ch) in trimmed.char_indices() {
            if ch.is_ascii_digit() || matches!(ch, '.' | '+' | '-' | 'e' | 'E') {
                end = idx + ch.len_utf8();
            } else if end > 0 {
                break;
            }
        }
        trimmed
            .get(..end)?
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite() && *value > 0.0)
    }
}

impl Default for LeoReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for LeoReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("sxm") | Some("tif") | Some("tiff"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        // Like Java (suffixSufficient=false), detection requires opening the
        // TIFF and checking for the LEO tag; header bytes alone are not enough.
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        self.inner.set_id(path)?;

        // Require the LEO private tag in the first IFD.
        let first = self
            .inner
            .ifd(0)
            .ok_or_else(|| BioFormatsError::UnsupportedFormat("LEO: no IFD".into()))?;
        if first.get(Self::LEO_TAG).is_none() {
            let _ = self.inner.close();
            return Err(BioFormatsError::UnsupportedFormat(
                "LEO: TIFF is missing the LEO tag (34118)".into(),
            ));
        }

        let mut meta = self.inner.metadata().clone();
        meta.is_interleaved = false;
        if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
            meta.series_metadata
                .insert("image_name".into(), MetadataValue::String(name.to_string()));
        }
        meta.series_metadata
            .insert("format".into(), MetadataValue::String("LEO".into()));

        // Parse the LEO tag text: lines of `AP_*`/`DP_*`/`SV_*` keys whose
        // value lives on the following line (Java initStandardMetadata()).
        if let Some(tag_text) = first.get_str(Self::LEO_TAG) {
            let lines: Vec<&str> = tag_text.split('\n').collect();
            if let Some(legacy_size) = lines.get(3).and_then(|line| {
                line.trim()
                    .parse::<f64>()
                    .ok()
                    .filter(|value| value.is_finite() && *value > 0.0)
                    .map(|value| value * 1_000_000.0)
            }) {
                meta.series_metadata
                    .insert("PhysicalSizeX".into(), MetadataValue::Float(legacy_size));
                meta.series_metadata
                    .insert("PhysicalSizeY".into(), MetadataValue::Float(legacy_size));
            }
            let mut i = 36usize;
            while i < lines.len() {
                let t = lines[i].trim_end_matches('\r');
                if (t.starts_with("AP_") || t.starts_with("DP_") || t.starts_with("SV_"))
                    && i + 1 < lines.len()
                {
                    let val_line = lines[i + 1].trim_end_matches('\r');
                    if let Some((k, v)) =
                        Self::parse_value_line(val_line, t == "AP_TIME" || t == "AP_DATE")
                    {
                        meta.series_metadata
                            .insert(k.to_string(), MetadataValue::String(v.to_string()));
                    }
                    i += 2;
                } else {
                    i += 1;
                }
            }
        }

        if let Some(MetadataValue::String(pixel_size)) =
            meta.series_metadata.get("Image Pixel Size")
        {
            if let Some(size) = Self::parse_length_number(pixel_size) {
                meta.series_metadata
                    .insert("PhysicalSizeX".into(), MetadataValue::Float(size));
                meta.series_metadata
                    .insert("PhysicalSizeY".into(), MetadataValue::Float(size));
            }
        } else if let Some(MetadataValue::String(pixel_size)) =
            meta.series_metadata.get("Pixel Size")
        {
            if !meta.series_metadata.contains_key("PhysicalSizeX") {
                if let Some(size_um) = Self::parse_length_um(pixel_size) {
                    meta.series_metadata
                        .insert("PhysicalSizeX".into(), MetadataValue::Float(size_um));
                    meta.series_metadata
                        .insert("PhysicalSizeY".into(), MetadataValue::Float(size_um));
                }
            }
        }
        meta.series_metadata.insert(
            "objective.immersion".into(),
            MetadataValue::String("Other".into()),
        );
        meta.series_metadata.insert(
            "objective.correction".into(),
            MetadataValue::String("Other".into()),
        );
        if let Some(MetadataValue::String(working_distance)) = meta.series_metadata.get("WD") {
            if let Some(wd_um) = Self::parse_length_um(working_distance) {
                meta.series_metadata.insert(
                    "objective.working_distance".into(),
                    MetadataValue::Float(wd_um),
                );
            }
        }

        self.path = Some(path.to_path_buf());
        self.meta = Some(meta);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.meta = None;
        self.path = None;
        self.inner.close()
    }

    fn series_count(&self) -> usize {
        if self.meta.is_some() {
            self.inner.series_count()
        } else {
            0
        }
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.meta.is_none() {
            return Err(BioFormatsError::NotInitialized);
        }
        self.inner.set_series(s)
    }

    fn series(&self) -> usize {
        self.inner.series()
    }

    fn metadata(&self) -> &ImageMetadata {
        self.meta
            .as_ref()
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
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
        self.inner.open_bytes_region(plane_index, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        self.inner.open_thumb_bytes(plane_index)
    }

    fn resolution_count(&self) -> usize {
        self.inner.resolution_count()
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        self.inner.set_resolution(level)
    }

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        let meta = self.meta.as_ref()?;
        let mut ome = crate::common::ome_metadata::OmeMetadata::from_image_metadata(meta);
        if let Some(image) = ome.images.get_mut(0) {
            if image.name.is_none() {
                image.name = self
                    .path
                    .as_ref()
                    .and_then(|path| path.file_name())
                    .and_then(|name| name.to_str())
                    .map(|name| name.to_string());
            }
            if let Some(MetadataValue::Float(size_um)) = meta.series_metadata.get("PhysicalSizeX") {
                image.physical_size_x = Some(*size_um);
            }
            if let Some(MetadataValue::Float(size_um)) = meta.series_metadata.get("PhysicalSizeY") {
                image.physical_size_y = Some(*size_um);
            }
            if image.physical_size_x.is_none() || image.physical_size_y.is_none() {
                if let Some(MetadataValue::String(pixel_size)) =
                    meta.series_metadata.get("Image Pixel Size")
                {
                    if let Some(size) = Self::parse_length_number(pixel_size) {
                        image.physical_size_x.get_or_insert(size);
                        image.physical_size_y.get_or_insert(size);
                    }
                } else if let Some(MetadataValue::String(pixel_size)) =
                    meta.series_metadata.get("Pixel Size")
                {
                    if let Some(size) = Self::parse_length_um(pixel_size) {
                        image.physical_size_x.get_or_insert(size);
                        image.physical_size_y.get_or_insert(size);
                    }
                }
            }
            image.instrument_ref = Some(0);
            image.objective_ref = Some(0);
        }
        if ome.instruments.is_empty() {
            let mut instrument = crate::common::ome_metadata::OmeInstrument {
                id: Some(crate::common::ome_metadata::create_lsid("Instrument", &[0])),
                ..Default::default()
            };
            instrument
                .objectives
                .push(crate::common::ome_metadata::OmeObjective {
                    id: Some(crate::common::ome_metadata::create_lsid(
                        "Objective",
                        &[0, 0],
                    )),
                    immersion: Some("Other".into()),
                    correction: Some("Other".into()),
                    working_distance: match meta.series_metadata.get("objective.working_distance") {
                        Some(MetadataValue::Float(value)) => Some(*value),
                        _ => None,
                    },
                    ..Default::default()
                });
            ome.instruments.push(instrument);
        }
        Some(ome)
    }
}

// ===========================================================================
// Real binary reader — Zeiss LMS
// ===========================================================================

/// Zeiss LMS reader (`.lms`).
///
/// Supports only the conservative BioFormats-rs strict raw subset identified
/// by `BIOFORMATS-RS-ZEISS-LMS-STRICT-RAW-V1\n`.
pub struct ZeissLmsReader {
    path: Option<PathBuf>,
    metas: Vec<ImageMetadata>,
    data_offsets: Vec<u64>,
    current_series: usize,
}

impl ZeissLmsReader {
    const CHECK: &'static [u8] = b"LMSFLE";
    const MARKER: &'static [u8] = b"BM6";
    const WIDTH: u32 = 1280;
    const HEIGHT: u32 = 1024;

    pub fn new() -> Self {
        ZeissLmsReader {
            path: None,
            metas: Vec::new(),
            data_offsets: Vec::new(),
            current_series: 0,
        }
    }

    fn next_marker(data: &[u8], start: usize) -> Option<usize> {
        let mut pos = start;
        while pos + 3 <= data.len() {
            if &data[pos..pos + 3] == Self::MARKER {
                return Some(pos + 4);
            }
            pos += 1;
        }
        None
    }

    fn parse_native(path: &Path) -> Result<(Vec<ImageMetadata>, Vec<u64>)> {
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        if data.len() < 16
            || !data[..16]
                .windows(Self::CHECK.len())
                .any(|w| w == Self::CHECK)
        {
            return Err(unsupported_raw_sem("Zeiss LMS"));
        }

        let magnification = if data.len() >= 22 {
            u32::from_le_bytes(data[18..22].try_into().unwrap()) as i64
        } else {
            0
        };

        let thumb_marker = Self::next_marker(&data, 0).ok_or_else(|| {
            BioFormatsError::UnsupportedFormat("Zeiss LMS missing thumbnail marker".into())
        })?;
        let thumb_offset = thumb_marker.checked_add(50).ok_or_else(|| {
            BioFormatsError::Format("Zeiss LMS thumbnail offset overflows".into())
        })?;
        let thumb_bytes = (Self::WIDTH as usize)
            .checked_mul(Self::HEIGHT as usize)
            .and_then(|n| n.checked_mul(3))
            .ok_or_else(|| BioFormatsError::Format("Zeiss LMS thumbnail size overflows".into()))?;
        let after_thumb = thumb_offset
            .checked_add(thumb_bytes)
            .ok_or_else(|| BioFormatsError::Format("Zeiss LMS thumbnail size overflows".into()))?;
        if after_thumb > data.len() {
            return Err(BioFormatsError::UnsupportedFormat(
                "Zeiss LMS thumbnail payload is shorter than declared dimensions".into(),
            ));
        }

        let image_marker = Self::next_marker(&data, after_thumb).ok_or_else(|| {
            BioFormatsError::UnsupportedFormat("Zeiss LMS missing image marker".into())
        })?;
        let lut_offset = image_marker
            .checked_add(50)
            .ok_or_else(|| BioFormatsError::Format("Zeiss LMS LUT offset overflows".into()))?;
        if lut_offset + 1024 > data.len() {
            return Err(BioFormatsError::UnsupportedFormat(
                "Zeiss LMS LUT is truncated".into(),
            ));
        }
        let mut red = Vec::with_capacity(256);
        let mut green = Vec::with_capacity(256);
        let mut blue = Vec::with_capacity(256);
        for i in 0..256 {
            let base = lut_offset + i * 4;
            red.push(data[base] as u16);
            green.push(data[base + 1] as u16);
            blue.push(data[base + 2] as u16);
        }
        let main_offset = lut_offset + 1024;
        let plane_bytes = (Self::WIDTH as u64)
            .checked_mul(Self::HEIGHT as u64)
            .and_then(|n| n.checked_mul(2))
            .ok_or_else(|| BioFormatsError::Format("Zeiss LMS plane size overflows".into()))?;
        let available = data.len() as u64 - main_offset as u64;
        let size_z = available / plane_bytes;
        if size_z == 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "Zeiss LMS has no complete 16-bit image planes".into(),
            ));
        }

        let mut main_metadata = HashMap::new();
        main_metadata.insert(
            "Objective nominal magnification".into(),
            MetadataValue::Int(magnification),
        );
        let main = ImageMetadata {
            size_x: Self::WIDTH,
            size_y: Self::HEIGHT,
            size_z: size_z as u32,
            size_c: 1,
            size_t: 1,
            pixel_type: PixelType::Uint16,
            bits_per_pixel: 16,
            image_count: size_z as u32,
            dimension_order: DimensionOrder::XYCZT,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: true,
            is_little_endian: true,
            resolution_count: 1,
            thumbnail: false,
            series_metadata: main_metadata,
            lookup_table: Some(LookupTable { red, green, blue }),
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        };

        let mut thumb_metadata = HashMap::new();
        thumb_metadata.insert(
            "Objective nominal magnification".into(),
            MetadataValue::Int(magnification),
        );
        let thumb = ImageMetadata {
            size_x: Self::WIDTH,
            size_y: Self::HEIGHT,
            size_z: 1,
            size_c: 3,
            size_t: 1,
            pixel_type: PixelType::Uint8,
            bits_per_pixel: 8,
            image_count: 1,
            dimension_order: DimensionOrder::XYCZT,
            is_rgb: true,
            is_interleaved: true,
            is_indexed: false,
            is_little_endian: true,
            resolution_count: 1,
            thumbnail: false,
            series_metadata: thumb_metadata,
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        };

        Ok((
            vec![main, thumb],
            vec![main_offset as u64, thumb_offset as u64],
        ))
    }
}

impl Default for ZeissLmsReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for ZeissLmsReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("lms"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.starts_with(ZEISS_LMS_STRICT_MAGIC)
            || (header.len() >= 16
                && header[..16]
                    .windows(Self::CHECK.len())
                    .any(|w| w == Self::CHECK))
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        let (metas, data_offsets) = if data.starts_with(ZEISS_LMS_STRICT_MAGIC) {
            let (meta, data_offset) =
                parse_strict_sem_raw(path, ZEISS_LMS_STRICT_MAGIC, "Zeiss LMS")?;
            (vec![meta], vec![data_offset])
        } else {
            Self::parse_native(path)?
        };
        self.path = Some(path.to_path_buf());
        self.metas = metas;
        self.data_offsets = data_offsets;
        self.current_series = 0;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.metas.clear();
        self.data_offsets.clear();
        self.current_series = 0;
        Ok(())
    }

    fn series_count(&self) -> usize {
        self.metas.len()
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.metas.is_empty() {
            Err(BioFormatsError::NotInitialized)
        } else if s < self.metas.len() {
            self.current_series = s;
            Ok(())
        } else {
            Err(BioFormatsError::SeriesOutOfRange(s))
        }
    }

    fn series(&self) -> usize {
        self.current_series
    }

    fn metadata(&self) -> &ImageMetadata {
        self.metas
            .get(self.current_series)
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self
            .metas
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }

        let n_bytes = (meta.size_x as usize)
            .checked_mul(meta.size_y as usize)
            .and_then(|n| {
                if meta.is_rgb {
                    n.checked_mul(meta.size_c as usize)
                } else {
                    Some(n)
                }
            })
            .and_then(|n| n.checked_mul(meta.pixel_type.bytes_per_sample()))
            .ok_or_else(|| BioFormatsError::Format("Zeiss LMS plane size overflows".into()))?;
        let path = self.path.clone().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = std::fs::File::open(&path).map_err(BioFormatsError::Io)?;
        let base = *self
            .data_offsets
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        let offset = base
            .checked_add(
                (n_bytes as u64)
                    .checked_mul(plane_index as u64)
                    .ok_or_else(|| {
                        BioFormatsError::Format("Zeiss LMS plane offset overflows".into())
                    })?,
            )
            .ok_or_else(|| BioFormatsError::Format("Zeiss LMS plane offset overflows".into()))?;
        f.seek(SeekFrom::Start(offset))
            .map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; n_bytes];
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
        let meta = self
            .metas
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?
            .clone();
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let full = self.open_bytes(plane_index)?;
        let channels = if meta.is_rgb { meta.size_c as usize } else { 1 };
        crop_full_plane("Zeiss LMS", &full, &meta, channels, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.metadata();
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn resolution_count(&self) -> usize {
        1
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if level != 0 {
            Err(BioFormatsError::Format(format!(
                "resolution {} out of range",
                level
            )))
        } else {
            Ok(())
        }
    }
}

/// IMOD binary model magic string (big-endian file). See
/// <http://bio3d.colorado.edu/imod/doc/binspec.html>.
const IMOD_MAGIC_STRING: &[u8] = b"IMODV1.2";

/// Minimal big-endian cursor over an in-memory IMOD model file.
///
/// Mirrors the subset of `loci.common.RandomAccessInputStream` used by the
/// Java `IMODReader`: big-endian integers/floats, single-byte reads, and
/// length-bounded string reads.
struct ImodCursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> ImodCursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        ImodCursor { data, pos: 0 }
    }

    fn len(&self) -> usize {
        self.data.len()
    }

    fn pos(&self) -> usize {
        self.pos
    }

    fn seek(&mut self, p: usize) {
        self.pos = p;
    }

    fn eof() -> BioFormatsError {
        BioFormatsError::Format("IMOD model file is truncated".into())
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).ok_or_else(Self::eof)?;
        let s = self.data.get(self.pos..end).ok_or_else(Self::eof)?;
        self.pos = end;
        Ok(s)
    }

    /// Reads `n` bytes as a string, returning fewer bytes at EOF (matching the
    /// behaviour of `RandomAccessInputStream.readString(int)`).
    fn read_string(&mut self, n: usize) -> String {
        let end = self.pos.saturating_add(n).min(self.data.len());
        let s = &self.data[self.pos..end];
        self.pos = end;
        String::from_utf8_lossy(s).into_owned()
    }

    fn read_u8(&mut self) -> Result<u8> {
        let b = *self.data.get(self.pos).ok_or_else(Self::eof)?;
        self.pos += 1;
        Ok(b)
    }

    fn read_i16(&mut self) -> Result<i16> {
        let b = self.take(2)?;
        Ok(i16::from_be_bytes([b[0], b[1]]))
    }

    fn read_i32(&mut self) -> Result<i32> {
        let b = self.take(4)?;
        Ok(i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn read_f32(&mut self) -> Result<f32> {
        let b = self.take(4)?;
        Ok(f32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn skip(&mut self, n: usize) -> Result<()> {
        let end = self.pos.checked_add(n).ok_or_else(Self::eof)?;
        if end > self.data.len() {
            return Err(Self::eof());
        }
        self.pos = end;
        Ok(())
    }
}

/// One parsed IMOD contour (`CONT` chunk).
struct ImodContour {
    closed: bool,
    scattered: bool,
    points: Vec<[f32; 3]>,
}

/// One parsed IMOD mesh (`MESH` chunk).
struct ImodMesh {
    vertices: Vec<[f32; 3]>,
    indices: Vec<i32>,
}

/// One parsed IMOD object (`OBJT` chunk) with its RGB colour, contours, and meshes.
struct ImodObject {
    color: [u8; 3],
    contours: Vec<ImodContour>,
    meshes: Vec<ImodMesh>,
}

/// Reader for IMOD binary model files (`.mod`).
///
/// Faithful port of Java Bio-Formats' `IMODReader`. It parses the IMOD binary
/// model header (magic `IMODV1.2`, big-endian) and walks the object / contour /
/// mesh structure to validate the file, collect global metadata, and rasterize
/// contour/mesh segments onto the corresponding Z slices in `open_bytes`.
pub struct ImodReader {
    meta: Option<ImageMetadata>,
    objects: Vec<ImodObject>,
}

impl ImodReader {
    pub fn new() -> Self {
        ImodReader {
            meta: None,
            objects: Vec::new(),
        }
    }
}

impl Default for ImodReader {
    fn default() -> Self {
        Self::new()
    }
}

fn imod_put_pixel(buf: &mut [u8], width: u32, height: u32, x: i64, y: i64, color: [u8; 3]) {
    if x >= 0 && x < i64::from(width) && y >= 0 && y < i64::from(height) {
        let off = (y as usize * width as usize + x as usize) * 3;
        buf[off..off + 3].copy_from_slice(&color);
    }
}

fn imod_draw_line(
    buf: &mut [u8],
    width: u32,
    height: u32,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    color: [u8; 3],
) {
    if !x0.is_finite() || !y0.is_finite() || !x1.is_finite() || !y1.is_finite() {
        return;
    }
    let min_x = x0.min(x1);
    let max_x = x0.max(x1);
    let min_y = y0.min(y1);
    let max_y = y0.max(y1);
    if max_x < -0.5 || min_x >= width as f32 + 0.5 || max_y < -0.5 || min_y >= height as f32 + 0.5 {
        return;
    }
    let x0_i = x0.round().clamp(-1.0, width as f32) as i64;
    let y0_i = y0.round().clamp(-1.0, height as f32) as i64;
    let x1_i = x1.round().clamp(-1.0, width as f32) as i64;
    let y1_i = y1.round().clamp(-1.0, height as f32) as i64;

    let dx = (x1_i - x0_i).abs();
    let sx = if x0_i < x1_i { 1 } else { -1 };
    let dy = -(y1_i - y0_i).abs();
    let sy = if y0_i < y1_i { 1 } else { -1 };
    let mut err = dx + dy;
    let mut cx = x0_i;
    let mut cy = y0_i;
    let max_steps = (dx - dy + 1) as usize;
    for _ in 0..max_steps {
        imod_put_pixel(buf, width, height, cx, cy, color);
        if cx == x1_i && cy == y1_i {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            cx += sx;
        }
        if e2 <= dx {
            err += dx;
            cy += sy;
        }
    }
}

fn imod_draw_segment_at_z(
    buf: &mut [u8],
    width: u32,
    height: u32,
    p0: [f32; 3],
    p1: [f32; 3],
    target_z: i64,
    z_f: f32,
    color: [u8; 3],
) {
    if !p0.iter().all(|v| v.is_finite()) || !p1.iter().all(|v| v.is_finite()) {
        return;
    }
    let z0 = p0[2].round() as i64;
    let z1 = p1[2].round() as i64;
    if z0 == target_z && z1 == target_z {
        imod_draw_line(buf, width, height, p0[0], p0[1], p1[0], p1[1], color);
    } else if (p0[2] - z_f) * (p1[2] - z_f) <= 0.0 && (p1[2] - p0[2]).abs() > 1e-6 {
        let t = (z_f - p0[2]) / (p1[2] - p0[2]);
        let ix = p0[0] + t * (p1[0] - p0[0]);
        let iy = p0[1] + t * (p1[1] - p0[1]);
        imod_put_pixel(buf, width, height, ix.round() as i64, iy.round() as i64, color);
    }
}

fn imod_slice_triangle_at_z(
    buf: &mut [u8],
    width: u32,
    height: u32,
    a: [f32; 3],
    b: [f32; 3],
    c: [f32; 3],
    target_z: i64,
    z_f: f32,
    color: [u8; 3],
) {
    if !a.iter().chain(b.iter()).chain(c.iter()).all(|v| v.is_finite()) {
        return;
    }
    if a[2].round() as i64 == target_z
        && b[2].round() as i64 == target_z
        && c[2].round() as i64 == target_z
    {
        imod_draw_line(buf, width, height, a[0], a[1], b[0], b[1], color);
        imod_draw_line(buf, width, height, b[0], b[1], c[0], c[1], color);
        imod_draw_line(buf, width, height, c[0], c[1], a[0], a[1], color);
        return;
    }
    let mut hits = [[0.0f32; 2]; 3];
    let mut count = 0usize;
    for (u, v) in [(a, b), (b, c), (c, a)] {
        let du = u[2] - z_f;
        let dv = v[2] - z_f;
        if (du <= 0.0 && dv >= 0.0) || (du >= 0.0 && dv <= 0.0) {
            let dz = v[2] - u[2];
            if dz.abs() > 1e-6 {
                let t = (z_f - u[2]) / dz;
                hits[count] = [u[0] + t * (v[0] - u[0]), u[1] + t * (v[1] - u[1])];
                count += 1;
            }
        }
    }
    if count >= 2 {
        imod_draw_line(
            buf,
            width,
            height,
            hits[0][0],
            hits[0][1],
            hits[1][0],
            hits[1][1],
            color,
        );
    }
}

fn imod_rasterize_mesh_at_z(
    buf: &mut [u8],
    width: u32,
    height: u32,
    mesh: &ImodMesh,
    target_z: i64,
    z_f: f32,
    color: [u8; 3],
) {
    const IMOD_MESH_BGNBIGPOLY: i32 = -21;
    const IMOD_MESH_ENDPOLY: i32 = -22;
    const IMOD_MESH_NORMAL: i32 = -23;
    const IMOD_MESH_BGNPOLY: i32 = -25;
    const IMOD_MESH_END: i32 = -1;

    let mut i = 0usize;
    while i < mesh.indices.len() {
        let op = mesh.indices[i];
        i += 1;
        match op {
            IMOD_MESH_BGNPOLY => {
                let mut tri = Vec::with_capacity(3);
                while i < mesh.indices.len() {
                    let v = mesh.indices[i];
                    if v < 0 {
                        if v == IMOD_MESH_NORMAL {
                            i += 2;
                            continue;
                        }
                        if v == IMOD_MESH_ENDPOLY {
                            i += 1;
                        }
                        break;
                    }
                    i += 1;
                    tri.push(v as usize);
                    if tri.len() == 3 {
                        if let (Some(&a), Some(&b), Some(&c)) = (
                            mesh.vertices.get(tri[0]),
                            mesh.vertices.get(tri[1]),
                            mesh.vertices.get(tri[2]),
                        ) {
                            imod_slice_triangle_at_z(buf, width, height, a, b, c, target_z, z_f, color);
                        }
                        tri.clear();
                    }
                }
            }
            IMOD_MESH_BGNBIGPOLY => {
                let mut tri = Vec::with_capacity(3);
                while i < mesh.indices.len() {
                    let v = mesh.indices[i];
                    if v < 0 {
                        if v == IMOD_MESH_ENDPOLY {
                            i += 1;
                        }
                        break;
                    }
                    // Each vertex in BGNBIGPOLY is preceded by its normal index.
                    if i + 1 >= mesh.indices.len() || mesh.indices[i + 1] < 0 {
                        i += 1;
                        break;
                    }
                    let vert_idx = mesh.indices[i + 1] as usize;
                    i += 2;
                    tri.push(vert_idx);
                    if tri.len() == 3 {
                        if let (Some(&a), Some(&b), Some(&c)) = (
                            mesh.vertices.get(tri[0]),
                            mesh.vertices.get(tri[1]),
                            mesh.vertices.get(tri[2]),
                        ) {
                            imod_slice_triangle_at_z(buf, width, height, a, b, c, target_z, z_f, color);
                        }
                        tri.clear();
                    }
                }
            }
            IMOD_MESH_END => break,
            _ => {}
        }
    }
}

/// Parses the IMOD model header and walks objects/contours/meshes, mirroring
/// `IMODReader.initFile`. Returns the populated [`ImageMetadata`] and parsed objects.
fn parse_imod(data: &[u8]) -> Result<(ImageMetadata, Vec<ImodObject>)> {
    let mut c = ImodCursor::new(data);

    let check = c.read_string(8);
    if check.as_bytes() != IMOD_MAGIC_STRING {
        return Err(BioFormatsError::Format(format!(
            "Invalid IMOD file ID: {check}"
        )));
    }

    let filename = c.read_string(128);
    let size_x = c.read_i32()?;
    let size_y = c.read_i32()?;
    let size_z = c.read_i32()?;

    let n_objects = c.read_i32()?;
    if n_objects < 0 {
        return Err(BioFormatsError::Format(
            "IMOD model declares a negative object count".into(),
        ));
    }

    let flags = c.read_i32()?;
    let draw_mode = c.read_i32()?;
    let mouse_mode = c.read_i32()?;
    let black_level = c.read_i32()?;
    let white_level = c.read_i32()?;

    let x_offset = c.read_f32()?;
    let y_offset = c.read_f32()?;
    let z_offset = c.read_f32()?;

    let x_scale = c.read_f32()?;
    let y_scale = c.read_f32()?;
    let z_scale = c.read_f32()?;

    let _current_object = c.read_i32()?;
    let _current_contour = c.read_i32()?;
    let _current_point = c.read_i32()?;

    let _res = c.read_i32()?;
    let _thresh = c.read_i32()?;

    let _pix_size = c.read_f32()?;
    let pix_size_units = c.read_i32()?;

    let _checksum = c.read_i32()?;

    let alpha = c.read_f32()?;
    let beta = c.read_f32()?;
    let gamma = c.read_f32()?;

    let mut series_metadata: HashMap<String, MetadataValue> = HashMap::new();
    let mut put_s = |k: &str, v: String| {
        series_metadata.insert(k.to_string(), MetadataValue::String(v));
    };
    put_s("Model name", filename);
    let mut put_i = |k: &str, v: i64| {
        series_metadata.insert(k.to_string(), MetadataValue::Int(v));
    };
    put_i("Model flags", flags as i64);
    put_i("Model drawing mode", draw_mode as i64);
    put_i("Mouse mode", mouse_mode as i64);
    put_i("Black level", black_level as i64);
    put_i("White level", white_level as i64);
    let mut put_f = |k: &str, v: f64| {
        series_metadata.insert(k.to_string(), MetadataValue::Float(v));
    };
    put_f("X offset", x_offset as f64);
    put_f("Y offset", y_offset as f64);
    put_f("Z offset", z_offset as f64);
    put_f("X scale", x_scale as f64);
    put_f("Y scale", y_scale as f64);
    put_f("Z scale", z_scale as f64);
    put_f("Alpha", alpha as f64);
    put_f("Beta", beta as f64);
    put_f("Gamma", gamma as f64);

    let mut objects: Vec<ImodObject> = Vec::with_capacity(n_objects as usize);

    'objects: for _obj in 0..n_objects {
        // Skip any inter-object chunks (e.g. IMAT) until the next OBJT.
        let mut objt = c.read_string(4);
        while objt != "OBJT" && c.pos() < c.len() {
            if objt == "IMAT" {
                // ambient, diffuse, specular, shininess, fill r/g/b, sphere quality
                for _ in 0..8 {
                    c.read_u8()?;
                }
                c.skip(4)?;
                c.read_u8()?; // black level
                c.read_u8()?; // white level
                c.skip(2)?;
            }
            objt = c.read_string(4);
        }

        if objt != "OBJT" {
            break 'objects;
        }

        let _obj_name = c.read_string(64);
        c.skip(64)?; // unused

        let n_contours = c.read_i32()?;
        let obj_flags = c.read_i32()?;
        let _axis = c.read_i32()?;
        let _obj_draw_mode = c.read_i32()?;

        let red = c.read_f32()?;
        let green = c.read_f32()?;
        let blue = c.read_f32()?;
        let color = [
            (red.clamp(0.0, 1.0) * 255.0).round() as u8,
            (green.clamp(0.0, 1.0) * 255.0).round() as u8,
            (blue.clamp(0.0, 1.0) * 255.0).round() as u8,
        ];
        let obj_open = (obj_flags & (1 << 3)) != 0;
        let obj_scattered = (obj_flags & (1 << 4)) != 0;

        let _pixel_radius = c.read_i32()?;
        let _pixel_symbol = c.read_u8()?;
        let _symbol_size = c.read_u8()?;
        let _line_width_2d = c.read_u8()?;
        let _line_width_3d = c.read_u8()?;
        let _line_style = c.read_u8()?;
        let _symbol_flags = c.read_u8()?;
        let _symbol_padding = c.read_u8()?;
        let _transparency = c.read_u8()?;

        let n_meshes = c.read_i32()?;
        let _n_surfaces = c.read_i32()?;

        if n_contours < 0 {
            return Err(BioFormatsError::Format(
                "IMOD object declares a negative contour count".into(),
            ));
        }

        let mut contours = Vec::with_capacity(n_contours as usize);
        for _contour in 0..n_contours {
            c.skip(4)?; // CONT

            let mut n_points = c.read_i32()?;
            let mut contour_flags = c.read_i32()?;
            let _time_index = c.read_i32()?;
            let _surface = c.read_i32()?;

            if (n_points as i64) > (c.len() as i64) || n_points < 0 {
                // Resync: scan backwards for the next CONT marker, as Java does.
                let mut guard = 0u32;
                loop {
                    let tag = c.read_string(4);
                    if tag == "CONT" {
                        break;
                    }
                    if c.pos() < 8 {
                        return Err(BioFormatsError::Format(
                            "IMOD contour resync ran past the start of the file".into(),
                        ));
                    }
                    c.seek(c.pos() - 8);
                    guard += 1;
                    if guard > 1_000_000 {
                        return Err(BioFormatsError::Format(
                            "IMOD contour resync did not converge".into(),
                        ));
                    }
                }
                n_points = c.read_i32()?;
                contour_flags = c.read_i32()?;
                let _time_index = c.read_i32()?;
                let _surface = c.read_i32()?;
            }

            if n_points < 0 {
                return Err(BioFormatsError::Format(
                    "IMOD contour declares a negative point count".into(),
                ));
            }
            // Three big-endian floats (x, y, z) per point.
            let bytes = (n_points as usize)
                .checked_mul(12)
                .ok_or_else(|| BioFormatsError::Format("IMOD contour size overflows".into()))?;
            if c.pos() + bytes > c.len() {
                return Err(ImodCursor::eof());
            }
            let mut points = Vec::with_capacity(n_points as usize);
            for _ in 0..n_points {
                let x = c.read_f32()?;
                let y = c.read_f32()?;
                let z = c.read_f32()?;
                points.push([x, y, z]);
            }
            let contour_open = (contour_flags & (1 << 3)) != 0;
            contours.push(ImodContour {
                closed: !obj_open && !obj_scattered && !contour_open && n_points > 2,
                scattered: obj_scattered,
                points,
            });
        }

        if n_meshes < 0 {
            return Err(BioFormatsError::Format(
                "IMOD object declares a negative mesh count".into(),
            ));
        }
        let mut meshes = Vec::with_capacity(n_meshes as usize);
        for _mesh in 0..n_meshes {
            c.skip(4)?; // MESH
            let vsize = c.read_i32()?;
            let lsize = c.read_i32()?;
            let _mesh_flags = c.read_i32()?;
            let _time_index = c.read_i16()?;
            let _surface = c.read_i16()?;
            if vsize < 0 || lsize < 0 {
                return Err(BioFormatsError::Format(
                    "IMOD mesh declares a negative size".into(),
                ));
            }
            let needed = (vsize as usize)
                .checked_mul(12)
                .and_then(|v| v.checked_add((lsize as usize).checked_mul(4)?))
                .ok_or_else(|| BioFormatsError::Format("IMOD mesh size overflows".into()))?;
            if c.pos() + needed > c.len() {
                return Err(ImodCursor::eof());
            }
            let mut vertices = Vec::with_capacity(vsize as usize);
            for _ in 0..vsize {
                let x = c.read_f32()?;
                let y = c.read_f32()?;
                let z = c.read_f32()?;
                vertices.push([x, y, z]);
            }
            let mut indices = Vec::with_capacity(lsize as usize);
            for _ in 0..lsize {
                indices.push(c.read_i32()?);
            }
            meshes.push(ImodMesh { vertices, indices });
        }

        objects.push(ImodObject {
            color,
            contours,
            meshes,
        });
    }

    // Trailing chunks: pick up the physical pixel sizes from MINX.
    let mut physical_x = 0.0f64;
    let mut physical_y = 0.0f64;
    let mut physical_z = 0.0f64;
    while c.pos() + 4 < c.len() {
        let chunk = c.read_string(4);
        match chunk.as_str() {
            "IMAT" => c.skip(20)?,
            "VIEW" => {
                c.skip(4)?;
                if c.read_i32()? != 1 {
                    c.skip(176)?;
                    let bytes_per_view = c.read_i32()?;
                    if bytes_per_view < 0 {
                        return Err(BioFormatsError::Format(
                            "IMOD VIEW chunk declares a negative size".into(),
                        ));
                    }
                    c.skip(bytes_per_view as usize)?;
                }
            }
            "MINX" => {
                c.skip(40)?; // old transformation values
                physical_x = c.read_f32()? as f64;
                physical_y = c.read_f32()? as f64;
                physical_z = c.read_f32()? as f64;
            }
            _ => {}
        }
    }

    if physical_x > 0.0 {
        series_metadata.insert(
            "PhysicalSizeX".to_string(),
            MetadataValue::Float(physical_x),
        );
    }
    if physical_y > 0.0 {
        series_metadata.insert(
            "PhysicalSizeY".to_string(),
            MetadataValue::Float(physical_y),
        );
    }
    if physical_z > 0.0 {
        series_metadata.insert(
            "PhysicalSizeZ".to_string(),
            MetadataValue::Float(physical_z),
        );
    }
    series_metadata.insert(
        "Physical size unit code".to_string(),
        MetadataValue::Int(pix_size_units as i64),
    );

    if size_x < 0 || size_y < 0 || size_z < 0 {
        return Err(BioFormatsError::Format(
            "IMOD model declares negative dimensions".into(),
        ));
    }

    // Core metadata, exactly as the Java reader sets it: an RGB, interleaved,
    // big-endian UINT8 image with one plane per Z (sizeT == 1).
    let size_t: u32 = 1;
    let size_z = size_z as u32;
    let image_count = size_t
        .checked_mul(size_z)
        .ok_or_else(|| BioFormatsError::Format("IMOD plane count overflows".into()))?;

    let meta = ImageMetadata {
        size_x: size_x as u32,
        size_y: size_y as u32,
        size_z,
        size_c: 3,
        size_t,
        pixel_type: PixelType::Uint8,
        bits_per_pixel: 8,
        image_count,
        dimension_order: DimensionOrder::XYCZT,
        is_rgb: true,
        is_interleaved: true,
        is_indexed: false,
        is_little_endian: false,
        resolution_count: 1,
        series_metadata,
        ..ImageMetadata::default()
    };

    Ok((meta, objects))
}

impl FormatReader for ImodReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("mod"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.starts_with(IMOD_MAGIC_STRING)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        let (meta, objects) = parse_imod(&data)?;
        self.meta = Some(meta);
        self.objects = objects;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.meta = None;
        self.objects.clear();
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

        let n_bytes = (meta.size_x as usize)
            .checked_mul(meta.size_y as usize)
            .and_then(|n| n.checked_mul(meta.size_c as usize))
            .and_then(|n| n.checked_mul(meta.pixel_type.bytes_per_sample()))
            .ok_or_else(|| BioFormatsError::Format("IMOD plane size overflows".into()))?;
        let mut buf = vec![0u8; n_bytes];
        if meta.size_x == 0 || meta.size_y == 0 || self.objects.is_empty() {
            return Ok(buf);
        }

        let target_z = i64::from(plane_index);
        let z_f = plane_index as f32;
        for obj in &self.objects {
            for contour in &obj.contours {
                if contour.points.is_empty() {
                    continue;
                }
                if contour.scattered || contour.points.len() == 1 {
                    for p in &contour.points {
                        if p.iter().all(|v| v.is_finite()) && p[2].round() as i64 == target_z {
                            imod_put_pixel(
                                &mut buf,
                                meta.size_x,
                                meta.size_y,
                                p[0].round() as i64,
                                p[1].round() as i64,
                                obj.color,
                            );
                        }
                    }
                } else {
                    for w in contour.points.windows(2) {
                        imod_draw_segment_at_z(
                            &mut buf,
                            meta.size_x,
                            meta.size_y,
                            w[0],
                            w[1],
                            target_z,
                            z_f,
                            obj.color,
                        );
                    }
                    if contour.closed {
                        imod_draw_segment_at_z(
                            &mut buf,
                            meta.size_x,
                            meta.size_y,
                            *contour.points.last().unwrap(),
                            contour.points[0],
                            target_z,
                            z_f,
                            obj.color,
                        );
                    }
                }
            }
            for mesh in &obj.meshes {
                imod_rasterize_mesh_at_z(
                    &mut buf,
                    meta.size_x,
                    meta.size_y,
                    mesh,
                    target_z,
                    z_f,
                    obj.color,
                );
            }
        }
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
        let meta = self
            .meta
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?
            .clone();
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let full = self.open_bytes(plane_index)?;
        crop_full_plane("IMOD", &full, &meta, 3, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn resolution_count(&self) -> usize {
        1
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if level != 0 {
            Err(BioFormatsError::Format(format!(
                "resolution {} out of range",
                level
            )))
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod inr_tests {
    use super::*;
    use std::io::Write;

    /// Build a minimal 256-byte INRIMAGE-4 header followed by raw pixels.
    fn write_inr(dir: &std::path::Path, name: &str, header_body: &str, payload: &[u8]) -> PathBuf {
        let mut header = String::from("#INRIMAGE-4#{\n");
        header.push_str(header_body);
        header.push_str("##}\n");
        let mut bytes = header.into_bytes();
        assert!(bytes.len() <= 256, "test header exceeds 256 bytes");
        bytes.resize(256, b'\n');
        bytes.extend_from_slice(payload);
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(&bytes).unwrap();
        path
    }

    /// VDIM must map to size_t (Java INRReader.java:124-126), size_c forced to 1,
    /// dimension order XYZTC (Java line 160), image_count = z*t*c (line 157).
    #[test]
    fn vdim_maps_to_size_t() {
        let tmp = std::env::temp_dir();
        // 2x2 plane, ZDIM=3, VDIM=4 -> z=3,t=4,c=1,count=12, 8-bit unsigned
        let body = "XDIM=2\nYDIM=2\nZDIM=3\nVDIM=4\nPIXSIZE=8 bits\nTYPE=unsigned fixed\n";
        let payload = vec![0u8; 2 * 2 * 3 * 4];
        let path = write_inr(&tmp, "inr_vdim_test.inr", body, &payload);

        let mut r = InrReader::new();
        r.set_id(&path).unwrap();
        let m = r.metadata();
        assert_eq!(m.size_z, 3);
        assert_eq!(m.size_t, 4, "VDIM should populate size_t");
        assert_eq!(m.size_c, 1, "size_c must be forced to 1");
        assert_eq!(m.image_count, 12, "image_count = z*t*c");
        assert_eq!(m.dimension_order, DimensionOrder::XYZTC);
        assert_eq!(m.pixel_type, PixelType::Uint8);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn inr_detection_accepts_java_magic_prefix() {
        let reader = InrReader::new();
        assert!(reader.is_this_type_by_bytes(b"#INRIMAGE legacy header"));
        assert!(reader.is_this_type_by_bytes(b"#INRIMAGE-4#{"));
        assert!(!reader.is_this_type_by_bytes(b"#INRIMA"));
    }

    #[test]
    fn inr_cpu_header_sets_endianness() {
        let tmp = std::env::temp_dir();
        for (cpu, expected_le) in [("decm", true), ("pc", true), ("alpha", true), ("sun", false)] {
            let body = format!("XDIM=1\nYDIM=1\nPIXSIZE=16 bits\nTYPE=unsigned fixed\nCPU={cpu}\n");
            let name = format!("inr_cpu_endian_{cpu}.inr");
            let path = write_inr(&tmp, &name, &body, &[0, 1]);

            let mut r = InrReader::new();
            r.set_id(&path).unwrap();
            assert_eq!(
                r.metadata().is_little_endian,
                expected_le,
                "CPU={cpu} should set is_little_endian={expected_le}"
            );
            let _ = std::fs::remove_file(&path);
        }
    }

    /// Pixel type derives from TYPE + byte width: float 32-bit -> Float32,
    /// float/8-byte -> Float64, fixed -> signed/unsigned Int/Uint.
    #[test]
    fn pixel_type_by_byte_width() {
        let tmp = std::env::temp_dir();
        let cases: &[(&str, u32, PixelType)] = &[
            ("signed fixed", 8, PixelType::Int8),
            ("unsigned fixed", 8, PixelType::Uint8),
            ("signed fixed", 16, PixelType::Int16),
            ("unsigned fixed", 16, PixelType::Uint16),
            ("signed fixed", 32, PixelType::Int32),
            ("unsigned fixed", 32, PixelType::Uint32),
            ("float", 64, PixelType::Float64),
            ("float", 32, PixelType::Float32),
        ];
        for (i, (ty, bits, expected)) in cases.iter().enumerate() {
            let body = format!("XDIM=1\nYDIM=1\nPIXSIZE={} bits\nTYPE={}\n", bits, ty);
            let payload = vec![0u8; (bits / 8) as usize];
            let name = format!("inr_pt_{}.inr", i);
            let path = write_inr(&tmp, &name, &body, &payload);
            let mut r = InrReader::new();
            r.set_id(&path).unwrap();
            assert_eq!(
                r.metadata().pixel_type,
                *expected,
                "case {}: {} {}",
                i,
                ty,
                bits
            );
            let _ = std::fs::remove_file(&path);
        }
    }

    fn tiff_entry(tag: u16, field_type: u16, count: u32, value_or_offset: u32) -> [u8; 12] {
        let mut entry = [0u8; 12];
        entry[0..2].copy_from_slice(&tag.to_le_bytes());
        entry[2..4].copy_from_slice(&field_type.to_le_bytes());
        entry[4..8].copy_from_slice(&count.to_le_bytes());
        entry[8..12].copy_from_slice(&value_or_offset.to_le_bytes());
        entry
    }

    fn write_minimal_tiff_with_optional_ascii_tag(
        path: &std::path::Path,
        tag: Option<(u16, &str)>,
    ) {
        let mut tag_bytes = tag.map(|(_, value)| {
            let mut bytes = value.as_bytes().to_vec();
            bytes.push(0);
            bytes
        });

        let entry_count = if tag.is_some() { 11u32 } else { 10u32 };
        let ifd_start = 8u32;
        let tag_start = ifd_start + 2 + entry_count * 12 + 4;
        let pixel_start = tag_start + tag_bytes.as_ref().map(|b| b.len()).unwrap_or(0) as u32;

        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"II");
        bytes.extend_from_slice(&42u16.to_le_bytes());
        bytes.extend_from_slice(&ifd_start.to_le_bytes());
        bytes.extend_from_slice(&(entry_count as u16).to_le_bytes());

        let mut entries = vec![
            tiff_entry(256, 4, 1, 1),
            tiff_entry(257, 4, 1, 1),
            tiff_entry(258, 3, 1, 8),
            tiff_entry(259, 3, 1, 1),
            tiff_entry(262, 3, 1, 1),
            tiff_entry(273, 4, 1, pixel_start),
            tiff_entry(277, 3, 1, 1),
            tiff_entry(278, 4, 1, 1),
            tiff_entry(279, 4, 1, 1),
            tiff_entry(284, 3, 1, 1),
        ];
        if let (Some((ascii_tag, _)), Some(tag_bytes)) = (tag, tag_bytes.as_ref()) {
            entries.push(tiff_entry(ascii_tag, 2, tag_bytes.len() as u32, tag_start));
            entries.sort_by_key(|entry| u16::from_le_bytes([entry[0], entry[1]]));
        }
        for entry in entries {
            bytes.extend_from_slice(&entry);
        }
        bytes.extend_from_slice(&0u32.to_le_bytes());
        if let Some(tag_bytes) = tag_bytes.take() {
            bytes.extend_from_slice(&tag_bytes);
        }
        bytes.push(7);
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn zeiss_tiff_requires_axiovision_companion_xml() {
        let path =
            std::env::temp_dir().join(format!("bioformats_zeiss_plain_{}.tif", std::process::id()));
        write_minimal_tiff_with_optional_ascii_tag(&path, None);

        let mut reader = ZeissTiffReader::new();
        assert!(reader.is_this_type_by_name(std::path::Path::new("sample.tif")));
        assert!(reader.is_this_type_by_name(std::path::Path::new("sample.xml")));
        assert!(reader.is_this_type_by_name(std::path::Path::new("sample_meta.xml")));
        assert!(!reader.is_this_type_by_name(std::path::Path::new("sample.tiff")));
        let err = reader.set_id(&path).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("XML metadata not found")),
            "unexpected error: {err:?}"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn leo_metadata_parsing_matches_java_line_offset_and_separators() {
        let path = std::env::temp_dir().join(format!("bioformats_leo_{}.tif", std::process::id()));
        let mut lines: Vec<String> = (0..36).map(|_| "ignored".to_string()).collect();
        lines[0] = "AP_WD".to_string();
        lines[1] = "WD = 9 mm".to_string();
        lines.push("AP_IMAGE_PIXEL_SIZE".to_string());
        lines.push("Pixel Size = 0.5 um".to_string());
        lines.push("AP_TIME".to_string());
        lines.push("Time :12:34".to_string());
        lines.push("DP_GAIN".to_string());
        lines.push("Gain=12".to_string());
        let tag_text = lines.join("\n");
        write_minimal_tiff_with_optional_ascii_tag(&path, Some((LeoReader::LEO_TAG, &tag_text)));

        let mut reader = LeoReader::new();
        reader.set_id(&path).unwrap();
        let metadata = &reader.metadata().series_metadata;
        assert_eq!(
            metadata.get("Pixel Size").map(ToString::to_string),
            Some("0.5 um".to_string())
        );
        assert_eq!(
            metadata.get("Time").map(ToString::to_string),
            Some("12:34".to_string())
        );
        assert!(
            !metadata.contains_key("WD"),
            "LEO metadata before Java line 36 must be ignored"
        );
        assert!(
            !metadata.contains_key("Gain"),
            "LEO metadata without Java whitespace separator must be ignored"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn imod_rasterizes_contours_and_meshes_into_rgb_slices() {
        let mut out = Vec::new();
        let put_i32 = |buf: &mut Vec<u8>, v: i32| buf.extend_from_slice(&v.to_be_bytes());
        let put_f32 = |buf: &mut Vec<u8>, v: f32| buf.extend_from_slice(&v.to_be_bytes());

        out.extend_from_slice(b"IMODV1.2");
        out.extend_from_slice(&[0u8; 128]); // filename
        put_i32(&mut out, 6); // sizeX
        put_i32(&mut out, 6); // sizeY
        put_i32(&mut out, 3); // sizeZ
        put_i32(&mut out, 1); // nObjects
        for _ in 0..5 {
            put_i32(&mut out, 0);
        }
        for _ in 0..3 {
            put_f32(&mut out, 0.0);
        }
        for _ in 0..3 {
            put_f32(&mut out, 1.0);
        }
        for _ in 0..6 {
            put_i32(&mut out, 0);
        }
        put_f32(&mut out, 1.0);
        put_i32(&mut out, -6);
        for _ in 0..4 {
            put_i32(&mut out, 0);
        }

        // OBJT
        out.extend_from_slice(b"OBJT");
        out.extend_from_slice(&[0u8; 128]);
        put_i32(&mut out, 1); // nContours
        put_i32(&mut out, 0); // objFlags (closed)
        put_i32(&mut out, 0);
        put_i32(&mut out, 0);
        put_f32(&mut out, 1.0);
        put_f32(&mut out, 0.5);
        put_f32(&mut out, 0.0);
        put_i32(&mut out, 0);
        out.extend_from_slice(&[0u8; 8]);
        put_i32(&mut out, 1); // nMeshes
        put_i32(&mut out, 0);

        out.extend_from_slice(b"CONT");
        put_i32(&mut out, 3);
        put_i32(&mut out, 0);
        put_i32(&mut out, 0);
        put_i32(&mut out, 0);
        for (x, y, z) in [(1.0f32, 1.0f32, 0.0f32), (4.0, 1.0, 0.0), (1.0, 4.0, 0.0)] {
            put_f32(&mut out, x);
            put_f32(&mut out, y);
            put_f32(&mut out, z);
        }

        out.extend_from_slice(b"MESH");
        put_i32(&mut out, 3);
        put_i32(&mut out, 5);
        put_i32(&mut out, 0);
        out.extend_from_slice(&0i32.to_be_bytes());
        for (x, y, z) in [(2.0f32, 3.0f32, 0.5f32), (4.0, 3.0, 0.5), (3.0, 3.0, 1.5)] {
            put_f32(&mut out, x);
            put_f32(&mut out, y);
            put_f32(&mut out, z);
        }
        for idx in [-25i32, 0, 1, 2, -22] {
            put_i32(&mut out, idx);
        }

        let path = std::env::temp_dir().join(format!(
            "bioformats_imod_raster_{}.mod",
            std::process::id()
        ));
        std::fs::write(&path, out).unwrap();

        let mut reader = ImodReader::new();
        reader.set_id(&path).unwrap();
        let px = |buf: &[u8], x: usize, y: usize| -> [u8; 3] {
            let o = (y * 6 + x) * 3;
            [buf[o], buf[o + 1], buf[o + 2]]
        };
        let z0 = reader.open_bytes(0).unwrap();
        assert_eq!(px(&z0, 1, 1), [255, 128, 0]);
        assert_eq!(px(&z0, 3, 1), [255, 128, 0]);
        assert_eq!(px(&z0, 1, 3), [255, 128, 0]);
        assert_eq!(px(&z0, 0, 0), [0, 0, 0]);

        let z1 = reader.open_bytes(1).unwrap();
        assert_eq!(px(&z1, 3, 3), [255, 128, 0]);

        let z2 = reader.open_bytes(2).unwrap();
        assert!(z2.iter().all(|&b| b == 0));

        let _ = std::fs::remove_file(&path);
    }
}
