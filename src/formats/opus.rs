//! Bruker OPUS FTIR spectroscopy and ISS Vista FLIM format readers.

use std::fs::File;
use std::io::{ErrorKind, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;

const OPUS_UNSUPPORTED: &str =
    "Bruker OPUS native spectral image decoding is unsupported; expected explicit strict blind raw data";
const ISS_UNSUPPORTED: &str =
    "ISS Vista FLIM native decoding is unsupported; expected explicit strict blind raw data";
const BLIND_HEADER_LEN: usize = 32;
const OPUS_BLIND_MAGIC: &[u8; 8] = b"BFOPUS1\0";
const ISS_BLIND_MAGIC: &[u8; 8] = b"BFISSFL1";

#[derive(Clone, Copy)]
struct BlindLayout {
    data_offset: u64,
    plane_bytes: usize,
}

fn blind_unsupported(format_name: &str) -> BioFormatsError {
    match format_name {
        "Bruker OPUS" => BioFormatsError::UnsupportedFormat(OPUS_UNSUPPORTED.to_string()),
        "ISS Vista FLIM" => BioFormatsError::UnsupportedFormat(ISS_UNSUPPORTED.to_string()),
        _ => BioFormatsError::UnsupportedFormat(format!(
            "{format_name} native decoding is unsupported"
        )),
    }
}

fn read_u16_le(buf: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([buf[offset], buf[offset + 1]])
}

fn read_u32_le(buf: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        buf[offset],
        buf[offset + 1],
        buf[offset + 2],
        buf[offset + 3],
    ])
}

fn read_u64_le(buf: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes([
        buf[offset],
        buf[offset + 1],
        buf[offset + 2],
        buf[offset + 3],
        buf[offset + 4],
        buf[offset + 5],
        buf[offset + 6],
        buf[offset + 7],
    ])
}

fn blind_pixel_type(code: u16, format_name: &str) -> Result<PixelType> {
    match code {
        1 => Ok(PixelType::Uint8),
        2 => Ok(PixelType::Uint16),
        3 => Ok(PixelType::Float32),
        _ => Err(BioFormatsError::Format(format!(
            "{format_name} blind subset has unsupported pixel type code {code}"
        ))),
    }
}

fn parse_blind_raw_layout(
    path: &Path,
    magic: &[u8; 8],
    format_name: &str,
) -> Result<(ImageMetadata, BlindLayout)> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == ErrorKind::NotFound => {
            return Err(blind_unsupported(format_name));
        }
        Err(err) => return Err(BioFormatsError::Io(err)),
    };
    let file_len = file.metadata().map_err(BioFormatsError::Io)?.len();
    if file_len < magic.len() as u64 {
        return Err(BioFormatsError::Format(format!(
            "{format_name} file is too short for blind subset magic"
        )));
    }

    let mut prefix = [0u8; 8];
    file.read_exact(&mut prefix).map_err(BioFormatsError::Io)?;
    if &prefix != magic {
        return Err(blind_unsupported(format_name));
    }
    if file_len < BLIND_HEADER_LEN as u64 {
        return Err(BioFormatsError::Format(format!(
            "{format_name} blind subset header is truncated"
        )));
    }

    let mut header = [0u8; BLIND_HEADER_LEN];
    header[..8].copy_from_slice(&prefix);
    file.read_exact(&mut header[8..])
        .map_err(BioFormatsError::Io)?;

    let size_x = read_u32_le(&header, 8);
    let size_y = read_u32_le(&header, 12);
    let image_count = read_u32_le(&header, 16);
    let pixel_type_code = read_u16_le(&header, 20);
    let reserved = read_u16_le(&header, 22);
    let data_offset = read_u64_le(&header, 24);
    if size_x == 0 || size_y == 0 || image_count == 0 {
        return Err(BioFormatsError::Format(format!(
            "{format_name} blind subset dimensions must be non-zero"
        )));
    }
    if reserved != 0 {
        return Err(BioFormatsError::Format(format!(
            "{format_name} blind subset reserved header bytes must be zero"
        )));
    }
    if data_offset < BLIND_HEADER_LEN as u64 {
        return Err(BioFormatsError::Format(format!(
            "{format_name} blind subset data offset points into header"
        )));
    }

    let pixel_type = blind_pixel_type(pixel_type_code, format_name)?;
    let plane_bytes = (size_x as usize)
        .checked_mul(size_y as usize)
        .and_then(|px| px.checked_mul(pixel_type.bytes_per_sample()))
        .ok_or_else(|| {
            BioFormatsError::Format(format!("{format_name} blind subset plane size overflows"))
        })?;
    let payload_bytes = (plane_bytes as u64)
        .checked_mul(image_count as u64)
        .ok_or_else(|| {
            BioFormatsError::Format(format!("{format_name} blind subset payload size overflows"))
        })?;
    let required_len = data_offset.checked_add(payload_bytes).ok_or_else(|| {
        BioFormatsError::Format(format!("{format_name} blind subset file size overflows"))
    })?;
    if file_len < required_len {
        return Err(BioFormatsError::Format(format!(
            "{format_name} blind subset payload is truncated: got {file_len} bytes, expected at least {required_len}"
        )));
    }

    let meta = ImageMetadata {
        size_x,
        size_y,
        size_z: 1,
        size_c: 1,
        size_t: image_count,
        pixel_type,
        bits_per_pixel: (pixel_type.bytes_per_sample() * 8) as u8,
        image_count,
        dimension_order: DimensionOrder::XYCZT,
        is_little_endian: true,
        ..Default::default()
    };
    Ok((
        meta,
        BlindLayout {
            data_offset,
            plane_bytes,
        },
    ))
}

fn read_blind_plane(path: &Path, layout: BlindLayout, plane_index: u32) -> Result<Vec<u8>> {
    let offset = layout
        .data_offset
        .checked_add(
            (layout.plane_bytes as u64)
                .checked_mul(plane_index as u64)
                .ok_or_else(|| {
                    BioFormatsError::Format("blind subset plane offset overflows".into())
                })?,
        )
        .ok_or_else(|| BioFormatsError::Format("blind subset plane offset overflows".into()))?;
    let mut file = File::open(path).map_err(BioFormatsError::Io)?;
    file.seek(SeekFrom::Start(offset))
        .map_err(BioFormatsError::Io)?;
    let mut plane = vec![0u8; layout.plane_bytes];
    file.read_exact(&mut plane).map_err(BioFormatsError::Io)?;
    Ok(plane)
}

// ─── Bruker OPUS ──────────────────────────────────────────────────────────────
//
// Bruker OPUS is a binary format for FTIR/Raman spectroscopy data.
// The file starts with a block directory. The magic is version-dependent:
//   byte[0] == 0x0A and byte[1] in {0x00, 0x01, 0x02} for versions 5-7.
// Spectral images are stored as 2D or 3D arrays (x, y, wavenumber).

pub struct BrukerOpusReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    layout: Option<BlindLayout>,
}

impl BrukerOpusReader {
    pub fn new() -> Self {
        BrukerOpusReader {
            path: None,
            meta: None,
            layout: None,
        }
    }
}
impl Default for BrukerOpusReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for BrukerOpusReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        // OPUS files have numeric extensions (.0, .1, ...) or .abs, .dpt
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        match ext.as_deref() {
            Some("abs") | Some("dpt") | Some("spa") => true,
            Some(e) => e.chars().all(|c| c.is_ascii_digit()),
            None => false,
        }
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.starts_with(OPUS_BLIND_MAGIC)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.meta = None;
        self.path = None;
        self.layout = None;
        let (meta, layout) = parse_blind_raw_layout(path, OPUS_BLIND_MAGIC, "Bruker OPUS")?;
        self.path = Some(path.to_path_buf());
        self.meta = Some(meta);
        self.layout = Some(layout);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.layout = None;
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
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let layout = self.layout.ok_or(BioFormatsError::NotInitialized)?;
        read_blind_plane(path, layout, plane_index)
    }

    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let full = self.open_bytes(p)?;
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        crop_full_plane("Bruker OPUS", &full, meta, 1, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = ((meta.size_x - tw) / 2, (meta.size_y - th) / 2);
        self.open_bytes_region(p, tx, ty, tw, th)
    }
}

// ─── ISS Vista FLIM ───────────────────────────────────────────────────────────
//
// ISS (formerly ISS Inc.) FLIM data files (.iss).
// Binary format with a header encoding image dimensions.

pub struct IssFlimReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    layout: Option<BlindLayout>,
}

impl IssFlimReader {
    pub fn new() -> Self {
        IssFlimReader {
            path: None,
            meta: None,
            layout: None,
        }
    }
}
impl Default for IssFlimReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for IssFlimReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("iss"))
            .unwrap_or(false)
    }
    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.starts_with(ISS_BLIND_MAGIC)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.meta = None;
        self.path = None;
        self.layout = None;
        let (meta, layout) = parse_blind_raw_layout(path, ISS_BLIND_MAGIC, "ISS Vista FLIM")?;
        self.path = Some(path.to_path_buf());
        self.meta = Some(meta);
        self.layout = Some(layout);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.layout = None;
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
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let layout = self.layout.ok_or(BioFormatsError::NotInitialized)?;
        read_blind_plane(path, layout, plane_index)
    }

    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let full = self.open_bytes(p)?;
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        crop_full_plane("ISS Vista FLIM", &full, meta, 1, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = ((meta.size_x - tw) / 2, (meta.size_y - th) / 2);
        self.open_bytes_region(p, tx, ty, tw, th)
    }
}
