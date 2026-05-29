//! Readers and explicit unsupported detectors for obscure and proprietary formats.
//!
//! Partial readers decode only simple documented/raw payload cases. Formats
//! without enough structure to read pixels fail with `UnsupportedFormat` instead
//! of exposing placeholder metadata or synthetic planes.

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;

fn checked_plane_len(meta: &ImageMetadata) -> Result<usize> {
    let bytes_per_pixel = (meta.bits_per_pixel as usize)
        .checked_div(8)
        .filter(|bps| *bps > 0)
        .ok_or_else(|| BioFormatsError::Format("invalid bits per pixel".to_string()))?;
    (meta.size_x as usize)
        .checked_mul(meta.size_y as usize)
        .and_then(|px| px.checked_mul(bytes_per_pixel))
        .ok_or_else(|| BioFormatsError::Format("image plane is too large".to_string()))
}

fn crop_plane(
    plane: &[u8],
    meta: &ImageMetadata,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
) -> Result<Vec<u8>> {
    if x.checked_add(w).is_none_or(|end| end > meta.size_x)
        || y.checked_add(h).is_none_or(|end| end > meta.size_y)
    {
        return Err(BioFormatsError::Format(
            "requested region is outside the image bounds".to_string(),
        ));
    }
    let bytes_per_pixel = (meta.bits_per_pixel / 8) as usize;
    let row_bytes = meta.size_x as usize * bytes_per_pixel;
    let crop_row_bytes = w as usize * bytes_per_pixel;
    let x_offset = x as usize * bytes_per_pixel;
    let mut out = Vec::with_capacity(crop_row_bytes * h as usize);
    for row in y as usize..(y + h) as usize {
        let start = row
            .checked_mul(row_bytes)
            .and_then(|base| base.checked_add(x_offset))
            .ok_or_else(|| BioFormatsError::Format("requested region is too large".to_string()))?;
        let end = start
            .checked_add(crop_row_bytes)
            .ok_or_else(|| BioFormatsError::Format("requested region is too large".to_string()))?;
        if end > plane.len() {
            return Err(BioFormatsError::Format(
                "decoded plane is shorter than expected".to_string(),
            ));
        }
        out.extend_from_slice(&plane[start..end]);
    }
    Ok(out)
}

const MISC4_STRICT_RAW_HEADER_LEN: usize = 32;

#[derive(Clone, Copy)]
struct StrictRawLayout {
    data_offset: u64,
    plane_bytes: usize,
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

fn strict_raw_unsupported(format_name: &str) -> BioFormatsError {
    BioFormatsError::UnsupportedFormat(format!(
        "{format_name} native decoding is unsupported unless explicit strict raw data is present; refusing guessed proprietary metadata"
    ))
}

fn strict_raw_pixel_type(code: u16, format_name: &str) -> Result<PixelType> {
    match code {
        1 => Ok(PixelType::Uint8),
        2 => Ok(PixelType::Uint16),
        3 => Ok(PixelType::Float32),
        _ => Err(BioFormatsError::Format(format!(
            "{format_name} strict raw subset has unsupported pixel type code {code}"
        ))),
    }
}

fn parse_strict_raw_subset(
    path: &Path,
    magic: &[u8; 8],
    format_name: &str,
) -> Result<(ImageMetadata, StrictRawLayout)> {
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == ErrorKind::NotFound => {
            return Err(strict_raw_unsupported(format_name));
        }
        Err(err) => return Err(BioFormatsError::Io(err)),
    };
    let file_len = file.metadata().map_err(BioFormatsError::Io)?.len();
    if file_len < magic.len() as u64 {
        return Err(BioFormatsError::Format(format!(
            "{format_name} file is too short for strict raw subset magic"
        )));
    }

    let mut prefix = [0u8; 8];
    file.read_exact(&mut prefix).map_err(BioFormatsError::Io)?;
    if &prefix != magic {
        return Err(strict_raw_unsupported(format_name));
    }
    if file_len < MISC4_STRICT_RAW_HEADER_LEN as u64 {
        return Err(BioFormatsError::Format(format!(
            "{format_name} strict raw subset header is truncated"
        )));
    }

    let mut header = [0u8; MISC4_STRICT_RAW_HEADER_LEN];
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
            "{format_name} strict raw subset dimensions must be non-zero"
        )));
    }
    if reserved != 0 {
        return Err(BioFormatsError::Format(format!(
            "{format_name} strict raw subset reserved header bytes must be zero"
        )));
    }
    if data_offset < MISC4_STRICT_RAW_HEADER_LEN as u64 {
        return Err(BioFormatsError::Format(format!(
            "{format_name} strict raw subset data offset points into header"
        )));
    }

    let pixel_type = strict_raw_pixel_type(pixel_type_code, format_name)?;
    let plane_bytes = (size_x as usize)
        .checked_mul(size_y as usize)
        .and_then(|px| px.checked_mul(pixel_type.bytes_per_sample()))
        .ok_or_else(|| {
            BioFormatsError::Format(format!(
                "{format_name} strict raw subset plane size overflows"
            ))
        })?;
    let payload_bytes = (plane_bytes as u64)
        .checked_mul(image_count as u64)
        .ok_or_else(|| {
            BioFormatsError::Format(format!(
                "{format_name} strict raw subset payload size overflows"
            ))
        })?;
    let required_len = data_offset.checked_add(payload_bytes).ok_or_else(|| {
        BioFormatsError::Format(format!(
            "{format_name} strict raw subset file size overflows"
        ))
    })?;
    if file_len < required_len {
        return Err(BioFormatsError::Format(format!(
            "{format_name} strict raw subset payload is truncated: got {file_len} bytes, expected at least {required_len}"
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
        ..ImageMetadata::default()
    };
    Ok((
        meta,
        StrictRawLayout {
            data_offset,
            plane_bytes,
        },
    ))
}

fn read_strict_raw_plane(
    path: &Path,
    layout: StrictRawLayout,
    plane_index: u32,
) -> Result<Vec<u8>> {
    let offset = layout
        .data_offset
        .checked_add(
            (layout.plane_bytes as u64)
                .checked_mul(plane_index as u64)
                .ok_or_else(|| {
                    BioFormatsError::Format("strict raw subset plane offset overflows".to_string())
                })?,
        )
        .ok_or_else(|| {
            BioFormatsError::Format("strict raw subset plane offset overflows".to_string())
        })?;
    let mut file = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
    file.seek(SeekFrom::Start(offset))
        .map_err(BioFormatsError::Io)?;
    let mut plane = vec![0u8; layout.plane_bytes];
    file.read_exact(&mut plane).map_err(BioFormatsError::Io)?;
    Ok(plane)
}

// ---------------------------------------------------------------------------
// Macro for extension-only placeholder readers
// ---------------------------------------------------------------------------
#[allow(unused_macros)]
macro_rules! placeholder_reader {
    (
        $(#[$attr:meta])*
        pub struct $name:ident;
        extensions: [$($ext:literal),+];
        magic_bytes: false;
    ) => {
        $(#[$attr])*
        pub struct $name {
            path: Option<PathBuf>,
            meta: Option<ImageMetadata>,
        }

        impl $name {
            pub fn new() -> Self {
                $name { path: None, meta: None }
            }
        }

        impl Default for $name {
            fn default() -> Self { Self::new() }
        }

        impl FormatReader for $name {
            fn is_this_type_by_name(&self, path: &Path) -> bool {
                let ext = path.extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.to_ascii_lowercase());
                matches!(ext.as_deref(), $(Some($ext))|+)
            }

            fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool { false }

            fn set_id(&mut self, _path: &Path) -> Result<()> {
                Err(BioFormatsError::UnsupportedFormat(format!(
                    "{} native decoding is unsupported; refusing guessed proprietary metadata",
                    stringify!($name)
                )))
            }

            fn close(&mut self) -> Result<()> {
                self.path = None;
                self.meta = None;
                Ok(())
            }

            fn series_count(&self) -> usize { 0 }

            fn set_series(&mut self, s: usize) -> Result<()> {
                let _ = s;
                Err(BioFormatsError::NotInitialized)
            }

            fn series(&self) -> usize { 0 }

            fn metadata(&self) -> &ImageMetadata {
                self.meta.as_ref().unwrap_or(crate::common::reader::uninitialized_metadata())
            }

            fn open_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
                Err(BioFormatsError::UnsupportedFormat(format!(
                    "{} native decoding is unsupported; refusing guessed proprietary metadata",
                    stringify!($name)
                )))
            }

            fn open_bytes_region(&mut self, _plane_index: u32, _x: u32, _y: u32, _w: u32, _h: u32) -> Result<Vec<u8>> {
                Err(BioFormatsError::UnsupportedFormat(format!(
                    "{} native decoding is unsupported; refusing guessed proprietary metadata",
                    stringify!($name)
                )))
            }

            fn open_thumb_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
                Err(BioFormatsError::UnsupportedFormat(format!(
                    "{} native decoding is unsupported; refusing guessed proprietary metadata",
                    stringify!($name)
                )))
            }
        }
    };
}

// ---------------------------------------------------------------------------
// 1. Applied Precision APL
// ---------------------------------------------------------------------------
/// Applied Precision format reader (`.apl`).
///
/// Applied Precision APL is a proprietary binary format used by DeltaVision
/// instruments. The internal structure requires vendor documentation.
pub struct AplReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    layout: Option<StrictRawLayout>,
}

impl AplReader {
    pub fn new() -> Self {
        AplReader {
            path: None,
            meta: None,
            layout: None,
        }
    }
}

impl Default for AplReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for AplReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("apl"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.starts_with(b"BFAPL\0\0\0")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.layout = None;
        let (meta, layout) =
            parse_strict_raw_subset(path, b"BFAPL\0\0\0", "Applied Precision APL")?;
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
        read_strict_raw_plane(path, layout, plane_index)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let meta = meta.clone();
        let plane = self.open_bytes(plane_index)?;
        crop_plane(&plane, &meta, x, y, w, h)
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

// ---------------------------------------------------------------------------
// 2. ARF format — raw uint16 heuristic
// ---------------------------------------------------------------------------
/// Axon Raw Format (ARF) reader (`.arf`).
///
/// Reads the real file header per the upstream Java ARFReader:
/// 2 endianness bytes, "AR" signature, then version/width/height/bitsPerPixel
/// as unsigned shorts. Pixel data begins at `PIXELS_OFFSET` (524).
pub struct ArfReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

const ARF_PIXELS_OFFSET: u64 = 524;

impl ArfReader {
    pub fn new() -> Self {
        ArfReader {
            path: None,
            meta: None,
        }
    }
}

impl Default for ArfReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for ArfReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("arf"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // 2 endianness bytes followed by the "AR" signature.
        if header.len() < 4 {
            return false;
        }
        let valid_endian = (header[0] == 1 && header[1] == 0) || (header[0] == 0 && header[1] == 1);
        valid_endian && &header[2..4] == b"AR"
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.path = None;
        self.meta = None;

        let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
        let mut hdr = [0u8; 12];
        f.read_exact(&mut hdr).map_err(BioFormatsError::Io)?;

        // Determine endianness from the first two bytes.
        let little = if hdr[0] == 1 && hdr[1] == 0 {
            true
        } else if hdr[0] == 0 && hdr[1] == 1 {
            false
        } else {
            return Err(BioFormatsError::InvalidData(
                "ARF: undefined endianness".to_string(),
            ));
        };

        if &hdr[2..4] != b"AR" {
            return Err(BioFormatsError::InvalidData(
                "ARF: missing 'AR' signature".to_string(),
            ));
        }

        let read_u16 = |b: &[u8]| -> u32 {
            if little {
                u16::from_le_bytes([b[0], b[1]]) as u32
            } else {
                u16::from_be_bytes([b[0], b[1]]) as u32
            }
        };

        let version = read_u16(&hdr[4..6]);
        let width = read_u16(&hdr[6..8]);
        let height = read_u16(&hdr[8..10]);
        let bits_per_pixel = read_u16(&hdr[10..12]);
        if width == 0 || height == 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "ARF header has zero image dimensions".to_string(),
            ));
        }

        // For version 2, the image count follows; otherwise a single image.
        let num_images = if version == 2 {
            let mut nb = [0u8; 2];
            f.read_exact(&mut nb).map_err(BioFormatsError::Io)?;
            let count = read_u16(&nb);
            if count == 0 {
                return Err(BioFormatsError::UnsupportedFormat(
                    "ARF header declares zero image count".to_string(),
                ));
            }
            count
        } else {
            1
        };

        // pixelTypeFromBytes(bpp, false, false): unsigned integer of bpp bytes.
        let mut bpp = bits_per_pixel / 8;
        if bits_per_pixel % 8 != 0 {
            bpp += 1;
        }
        let pixel_type = match bpp {
            1 => PixelType::Uint8,
            2 => PixelType::Uint16,
            4 => PixelType::Uint32,
            _ => {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "ARF: unsupported bits per pixel {}",
                    bits_per_pixel
                )))
            }
        };
        let plane_bytes = (width as u64)
            .checked_mul(height as u64)
            .and_then(|px| px.checked_mul(pixel_type.bytes_per_sample() as u64))
            .ok_or_else(|| BioFormatsError::Format("ARF image plane is too large".to_string()))?;
        let required_len = ARF_PIXELS_OFFSET
            .checked_add(plane_bytes.checked_mul(num_images as u64).ok_or_else(|| {
                BioFormatsError::Format("ARF image payload size overflows".to_string())
            })?)
            .ok_or_else(|| BioFormatsError::Format("ARF file size overflows".to_string()))?;
        let file_len = f.metadata().map_err(BioFormatsError::Io)?.len();
        if file_len < required_len {
            return Err(BioFormatsError::UnsupportedFormat(
                "ARF payload is shorter than declared image dimensions".to_string(),
            ));
        }

        self.path = Some(path.to_path_buf());
        self.meta = Some(ImageMetadata {
            size_x: width,
            size_y: height,
            size_z: 1,
            size_c: 1,
            size_t: num_images,
            pixel_type,
            bits_per_pixel: bits_per_pixel as u8,
            image_count: num_images,
            dimension_order: DimensionOrder::XYCZT,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: little,
            resolution_count: 1,
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
        let n_bytes = checked_plane_len(meta)?;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(
            ARF_PIXELS_OFFSET + plane_index as u64 * n_bytes as u64,
        ))
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
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let meta = meta.clone();
        let plane = self.open_bytes(plane_index)?;
        crop_plane(&plane, &meta, x, y, w, h)
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

// ---------------------------------------------------------------------------
// 3. I2I format
// ---------------------------------------------------------------------------
/// I2I format reader (`.i2i`).
///
/// I2I is a proprietary format with undocumented structure.
pub struct I2iReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    layout: Option<StrictRawLayout>,
}

impl I2iReader {
    pub fn new() -> Self {
        I2iReader {
            path: None,
            meta: None,
            layout: None,
        }
    }
}

impl Default for I2iReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for I2iReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("i2i"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.starts_with(b"BFI2I\0\0\0")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.layout = None;
        let (meta, layout) = parse_strict_raw_subset(path, b"BFI2I\0\0\0", "I2I")?;
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
        read_strict_raw_plane(path, layout, plane_index)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let meta = meta.clone();
        let plane = self.open_bytes(plane_index)?;
        crop_plane(&plane, &meta, x, y, w, h)
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

// ---------------------------------------------------------------------------
// 4. JDCE format
// ---------------------------------------------------------------------------
/// JDCE format reader (`.jdce`).
///
/// JDCE is a proprietary format with undocumented structure.
pub struct JdceReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    layout: Option<StrictRawLayout>,
}

impl JdceReader {
    pub fn new() -> Self {
        JdceReader {
            path: None,
            meta: None,
            layout: None,
        }
    }
}

impl Default for JdceReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for JdceReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("jdce"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.starts_with(b"BFJDCE\0\0")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.layout = None;
        let (meta, layout) = parse_strict_raw_subset(path, b"BFJDCE\0\0", "JDCE")?;
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
        read_strict_raw_plane(path, layout, plane_index)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let meta = meta.clone();
        let plane = self.open_bytes(plane_index)?;
        crop_plane(&plane, &meta, x, y, w, h)
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

// ---------------------------------------------------------------------------
// 5. JPX (JPEG 2000 Part 2)
// ---------------------------------------------------------------------------
/// JPX (JPEG 2000 Part 2) format reader (`.jpx`).
///
/// JPX files are JPEG 2000 Part 2; delegates to `Jpeg2000Reader`.
pub struct JpxReader {
    inner: crate::formats::misc::Jpeg2000Reader,
}

impl JpxReader {
    pub fn new() -> Self {
        JpxReader {
            inner: crate::formats::misc::Jpeg2000Reader::new(),
        }
    }
}

impl Default for JpxReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for JpxReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("jpx"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        self.inner.is_this_type_by_bytes(header)
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
}

// ---------------------------------------------------------------------------
// 6. Capture Pro Image (PCI)
// ---------------------------------------------------------------------------
/// Capture Pro Image format reader (`.pci`).
///
/// Capture Pro is a proprietary format from Media Cybernetics with
/// undocumented binary structure.
pub struct PciReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    layout: Option<StrictRawLayout>,
}

impl PciReader {
    pub fn new() -> Self {
        PciReader {
            path: None,
            meta: None,
            layout: None,
        }
    }
}

impl Default for PciReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for PciReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("pci"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.starts_with(b"BFPCI\0\0\0")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.layout = None;
        let (meta, layout) =
            parse_strict_raw_subset(path, b"BFPCI\0\0\0", "Capture Pro Image (PCI)")?;
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
        read_strict_raw_plane(path, layout, plane_index)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let meta = meta.clone();
        let plane = self.open_bytes(plane_index)?;
        crop_plane(&plane, &meta, x, y, w, h)
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

// ---------------------------------------------------------------------------
// 7. PDS planetary format — text header parsing
// ---------------------------------------------------------------------------
/// PDS (Planetary Data System) format reader (`.pds`).
///
/// PDS has a text header with keyword=value pairs (LINES, LINE_SAMPLES,
/// SAMPLE_BITS, etc.) followed by raw binary pixel data.
pub struct PdsReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    data_offset: u64,
}

impl PdsReader {
    pub fn new() -> Self {
        PdsReader {
            path: None,
            meta: None,
            data_offset: 0,
        }
    }
}

impl Default for PdsReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for PdsReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("pds"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.data_offset = 0;

        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        let text_end = data.len().min(8192);
        let header_text = String::from_utf8_lossy(&data[..text_end]);

        let mut lines = None;
        let mut line_samples = None;
        let mut sample_bits = None;
        let mut record_bytes = None;
        let mut label_records = None;
        let mut found_end = false;

        for line in header_text.lines() {
            let line = line.trim();
            if line == "END" {
                found_end = true;
                break;
            }
            if let Some((key, val)) = line.split_once('=') {
                let key = key.trim();
                let val = val.trim();
                match key {
                    "LINES" => {
                        lines = Some(val.parse::<u32>().map_err(|_| {
                            BioFormatsError::Format("PDS LINES is not a valid integer".to_string())
                        })?);
                    }
                    "LINE_SAMPLES" => {
                        line_samples = Some(val.parse::<u32>().map_err(|_| {
                            BioFormatsError::Format(
                                "PDS LINE_SAMPLES is not a valid integer".to_string(),
                            )
                        })?);
                    }
                    "SAMPLE_BITS" => {
                        sample_bits = Some(val.parse::<u32>().map_err(|_| {
                            BioFormatsError::Format(
                                "PDS SAMPLE_BITS is not a valid integer".to_string(),
                            )
                        })?);
                    }
                    "RECORD_BYTES" => {
                        record_bytes = Some(val.parse::<u64>().map_err(|_| {
                            BioFormatsError::Format(
                                "PDS RECORD_BYTES is not a valid integer".to_string(),
                            )
                        })?);
                    }
                    "LABEL_RECORDS" | "^IMAGE" => {
                        // ^IMAGE can be a record number
                        label_records = Some(val.parse::<u64>().map_err(|_| {
                            BioFormatsError::Format(
                                "PDS image record offset is not a valid integer".to_string(),
                            )
                        })?);
                    }
                    _ => {}
                }
            }
        }

        let lines = lines.unwrap_or(0);
        let line_samples = line_samples.unwrap_or(0);
        if lines == 0 || line_samples == 0 {
            return Err(BioFormatsError::Format(
                "PDS header missing LINES or LINE_SAMPLES keywords".to_string(),
            ));
        }
        let sample_bits = sample_bits.ok_or_else(|| {
            BioFormatsError::Format("PDS header missing SAMPLE_BITS keyword".to_string())
        })?;

        let (pixel_type, bpp) = match sample_bits {
            8 => (PixelType::Uint8, 8u8),
            16 => (PixelType::Uint16, 16u8),
            32 => (PixelType::Uint32, 32u8),
            _ => {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "PDS SAMPLE_BITS={sample_bits} is not supported"
                )));
            }
        };

        // Calculate data offset
        let offset =
            if let (Some(record_bytes), Some(label_records)) = (record_bytes, label_records) {
                if record_bytes == 0 || label_records == 0 {
                    return Err(BioFormatsError::Format(
                        "PDS record offset must be non-zero".to_string(),
                    ));
                }
                record_bytes * (label_records - 1) // PDS records are 1-based
            } else {
                // Find END keyword position and skip past it
                if !found_end {
                    return Err(BioFormatsError::Format(
                        "PDS header missing END marker".to_string(),
                    ));
                } else if let Some(end_pos) = header_text.find("\nEND\r") {
                    (end_pos + 5) as u64
                } else if let Some(end_pos) = header_text.find("\nEND\n") {
                    (end_pos + 5) as u64
                } else if let Some(end_pos) = header_text.find("\nEND") {
                    (end_pos + 4) as u64
                } else {
                    return Err(BioFormatsError::Format(
                        "PDS header END marker offset is invalid".to_string(),
                    ));
                }
            };

        let bytes_per_pixel = (bpp / 8) as u64;
        let expected = (line_samples as u64)
            .checked_mul(lines as u64)
            .and_then(|px| px.checked_mul(bytes_per_pixel))
            .ok_or_else(|| BioFormatsError::Format("PDS image plane is too large".to_string()))?;
        let available = data.len() as u64;
        if offset
            .checked_add(expected)
            .is_none_or(|end| end > available)
        {
            return Err(BioFormatsError::UnsupportedFormat(
                "PDS payload is shorter than declared image dimensions".to_string(),
            ));
        }

        self.data_offset = offset;
        self.path = Some(path.to_path_buf());
        self.meta = Some(ImageMetadata {
            size_x: line_samples,
            size_y: lines,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type,
            bits_per_pixel: bpp,
            image_count: 1,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: false, // PDS is typically big-endian
            resolution_count: 1,
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
        let n_bytes = checked_plane_len(meta)?;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
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
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let meta = meta.clone();
        let plane = self.open_bytes(plane_index)?;
        crop_plane(&plane, &meta, x, y, w, h)
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

// ---------------------------------------------------------------------------
// 8. Hiscan HIS format
// ---------------------------------------------------------------------------
/// Hamamatsu HIS format reader (`.his`).
///
/// Translated from Bio-Formats `HISReader`: each series starts with the `IM`
/// magic, a compact little-endian header, an optional semicolon-delimited
/// comment block, and then one image plane. Packed 12-bit variants are unpacked
/// to little-endian `u16` samples; byte-aligned UINT8/UINT16 grayscale and RGB
/// planes are decoded directly.
pub struct HisReader {
    path: Option<PathBuf>,
    metas: Vec<ImageMetadata>,
    pixel_offsets: Vec<u64>,
    packed_12_bit: Vec<bool>,
    current_series: usize,
}

impl HisReader {
    pub fn new() -> Self {
        HisReader {
            path: None,
            metas: Vec::new(),
            pixel_offsets: Vec::new(),
            packed_12_bit: Vec::new(),
            current_series: 0,
        }
    }

    fn current_meta(&self) -> Result<&ImageMetadata> {
        self.metas
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)
    }
}

fn unpack_his_packed_12(data: &[u8], samples: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples * 2);
    for sample in 0..samples {
        let mut value = 0u16;
        let bit_base = sample * 12;
        for bit_offset in 0..12 {
            let bit = bit_base + bit_offset;
            let byte = data.get(bit / 8).copied().unwrap_or(0);
            let bit_value = (byte >> (7 - (bit % 8))) & 1;
            value = (value << 1) | bit_value as u16;
        }
        out.extend_from_slice(&value.to_le_bytes());
    }
    out
}

impl Default for HisReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for HisReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("his"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.len() >= 2 && &header[..2] == b"IM"
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.path = None;
        self.metas.clear();
        self.pixel_offsets.clear();
        self.packed_12_bit.clear();
        self.current_series = 0;

        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        if data.len() < 16 || &data[..2] != b"IM" {
            return Err(BioFormatsError::UnsupportedFormat(
                "HIS header missing IM magic".to_string(),
            ));
        }

        let series_count = u16::from_le_bytes([data[14], data[15]]) as usize;
        if series_count == 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "HIS header declares zero image series".to_string(),
            ));
        }

        let mut metas = Vec::with_capacity(series_count);
        let mut pixel_offsets = Vec::with_capacity(series_count);
        let mut packed_12_bit = Vec::with_capacity(series_count);
        let mut offset = 0usize;
        for series in 0..series_count {
            if offset.checked_add(64).is_none_or(|end| end > data.len()) {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "HIS series {series} header is truncated"
                )));
            }
            if &data[offset..offset + 2] != b"IM" {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "HIS series {series} missing IM magic"
                )));
            }

            let comment_bytes = u16::from_le_bytes([data[offset + 2], data[offset + 3]]) as usize;
            let w = u16::from_le_bytes([data[offset + 4], data[offset + 5]]) as u32;
            let h = u16::from_le_bytes([data[offset + 6], data[offset + 7]]) as u32;
            let data_type = u16::from_le_bytes([data[offset + 12], data[offset + 13]]);
            if w == 0 || h == 0 {
                return Err(BioFormatsError::UnsupportedFormat(
                    "HIS header is missing image dimensions".to_string(),
                ));
            }

            let (pixel_type, bits_per_pixel, size_c, bytes_per_sample, is_packed_12) =
                match data_type {
                    1 => (PixelType::Uint8, 8u8, 1u32, 1u64, false),
                    2 => (PixelType::Uint16, 16u8, 1u32, 2u64, false),
                    6 => (PixelType::Uint16, 12u8, 1u32, 2u64, true),
                    11 => (PixelType::Uint8, 8u8, 3u32, 1u64, false),
                    12 => (PixelType::Uint16, 16u8, 3u32, 2u64, false),
                    14 => (PixelType::Uint16, 12u8, 3u32, 2u64, true),
                    other => {
                        return Err(BioFormatsError::UnsupportedFormat(format!(
                            "HIS data type {other} is not supported"
                        )));
                    }
                };

            let pixel_offset = offset
                .checked_add(64)
                .and_then(|base| base.checked_add(comment_bytes))
                .ok_or_else(|| BioFormatsError::Format("HIS header is too large".to_string()))?;
            let samples = (w as u64)
                .checked_mul(h as u64)
                .and_then(|px| px.checked_mul(size_c as u64))
                .ok_or_else(|| {
                    BioFormatsError::Format("HIS image plane is too large".to_string())
                })?;
            let plane_bytes = if is_packed_12 {
                samples
                    .checked_mul(12)
                    .and_then(|bits| bits.checked_add(7))
                    .map(|bits| bits / 8)
                    .ok_or_else(|| {
                        BioFormatsError::Format("HIS image plane is too large".to_string())
                    })?
            } else {
                samples.checked_mul(bytes_per_sample).ok_or_else(|| {
                    BioFormatsError::Format("HIS image plane is too large".to_string())
                })?
            };
            let next_offset = (pixel_offset as u64)
                .checked_add(plane_bytes)
                .ok_or_else(|| {
                    BioFormatsError::Format("HIS image plane is too large".to_string())
                })?;
            if next_offset > data.len() as u64 {
                return Err(BioFormatsError::UnsupportedFormat(
                    "HIS payload is shorter than declared image dimensions".to_string(),
                ));
            }

            let mut series_metadata = HashMap::new();
            if comment_bytes > 0 {
                let comment_end = pixel_offset;
                let comment_start = comment_end - comment_bytes;
                let comment = String::from_utf8_lossy(&data[comment_start..comment_end]);
                for token in comment.split(';') {
                    if let Some((key, value)) = token.split_once('=') {
                        series_metadata
                            .insert(key.to_string(), MetadataValue::String(value.to_string()));
                    }
                }
            }

            metas.push(ImageMetadata {
                size_x: w,
                size_y: h,
                size_z: 1,
                size_c,
                size_t: 1,
                pixel_type,
                bits_per_pixel,
                image_count: 1,
                dimension_order: DimensionOrder::XYCZT,
                is_rgb: size_c > 1,
                is_interleaved: size_c > 1,
                is_indexed: false,
                is_little_endian: true,
                resolution_count: 1,
                series_metadata,
                lookup_table: None,
                modulo_z: None,
                modulo_c: None,
                modulo_t: None,
            });
            pixel_offsets.push(pixel_offset as u64);
            packed_12_bit.push(is_packed_12);
            offset = next_offset as usize;
        }

        self.path = Some(path.to_path_buf());
        self.metas = metas;
        self.pixel_offsets = pixel_offsets;
        self.packed_12_bit = packed_12_bit;
        self.current_series = 0;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.metas.clear();
        self.pixel_offsets.clear();
        self.packed_12_bit.clear();
        self.current_series = 0;
        Ok(())
    }

    fn series_count(&self) -> usize {
        self.metas.len()
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.metas.is_empty() {
            Err(BioFormatsError::NotInitialized)
        } else if s >= self.metas.len() {
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
        self.metas
            .get(self.current_series)
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.current_meta()?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let sample_count = (meta.size_x as usize)
            .checked_mul(meta.size_y as usize)
            .and_then(|px| px.checked_mul(meta.size_c as usize))
            .ok_or_else(|| BioFormatsError::Format("HIS image plane is too large".to_string()))?;
        let is_packed_12 = *self
            .packed_12_bit
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        let n_bytes = if is_packed_12 {
            sample_count
                .checked_mul(12)
                .and_then(|bits| bits.checked_add(7))
                .map(|bits| bits / 8)
                .ok_or_else(|| {
                    BioFormatsError::Format("HIS image plane is too large".to_string())
                })?
        } else {
            sample_count
                .checked_mul(meta.pixel_type.bytes_per_sample())
                .ok_or_else(|| {
                    BioFormatsError::Format("HIS image plane is too large".to_string())
                })?
        };
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let pixel_offset = *self
            .pixel_offsets
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(pixel_offset))
            .map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; n_bytes];
        f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
        if is_packed_12 {
            Ok(unpack_his_packed_12(&buf, sample_count))
        } else {
            Ok(buf)
        }
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let meta = self.current_meta()?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        if x.checked_add(w).is_none_or(|end| end > meta.size_x)
            || y.checked_add(h).is_none_or(|end| end > meta.size_y)
        {
            return Err(BioFormatsError::Format(
                "requested region is outside the image bounds".to_string(),
            ));
        }
        let meta = meta.clone();
        let plane = self.open_bytes(plane_index)?;
        let bytes_per_pixel = meta.pixel_type.bytes_per_sample() * meta.size_c as usize;
        let row_bytes = meta.size_x as usize * bytes_per_pixel;
        let crop_row_bytes = w as usize * bytes_per_pixel;
        let x_offset = x as usize * bytes_per_pixel;
        let mut out = Vec::with_capacity(crop_row_bytes * h as usize);
        for row in y as usize..(y + h) as usize {
            let start = row
                .checked_mul(row_bytes)
                .and_then(|base| base.checked_add(x_offset))
                .ok_or_else(|| {
                    BioFormatsError::Format("requested region is too large".to_string())
                })?;
            let end = start.checked_add(crop_row_bytes).ok_or_else(|| {
                BioFormatsError::Format("requested region is too large".to_string())
            })?;
            out.extend_from_slice(&plane[start..end]);
        }
        Ok(out)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.current_meta()?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}

// ---------------------------------------------------------------------------
// 9. HRDC GDF format
// ---------------------------------------------------------------------------
/// HRDC GDF format reader (`.gdf`).
///
/// HRDC GDF is a proprietary format from the Health Research Data Council
/// with undocumented binary structure.
pub struct HrdgdfReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    layout: Option<StrictRawLayout>,
}

impl HrdgdfReader {
    pub fn new() -> Self {
        HrdgdfReader {
            path: None,
            meta: None,
            layout: None,
        }
    }
}

impl Default for HrdgdfReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for HrdgdfReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("gdf"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.starts_with(b"BFGDF\0\0\0")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.layout = None;
        let (meta, layout) = parse_strict_raw_subset(path, b"BFGDF\0\0\0", "HRDC GDF")?;
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
        read_strict_raw_plane(path, layout, plane_index)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let meta = meta.clone();
        let plane = self.open_bytes(plane_index)?;
        crop_plane(&plane, &meta, x, y, w, h)
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

// ---------------------------------------------------------------------------
// 10. Text/CSV image format
// ---------------------------------------------------------------------------
/// Text/CSV image reader (`.csv`).
///
/// Reads a CSV/TSV text file where each row is a line and columns are separated
/// by commas, tabs, or spaces. Each cell is parsed as f64, then stored as Float32
/// pixel data.
pub struct TextImageReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    pixel_data: Vec<u8>,
}

impl TextImageReader {
    pub fn new() -> Self {
        TextImageReader {
            path: None,
            meta: None,
            pixel_data: Vec::new(),
        }
    }
}

impl Default for TextImageReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for TextImageReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("csv"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.pixel_data.clear();

        let text = std::fs::read_to_string(path).map_err(BioFormatsError::Io)?;
        let mut rows: Vec<Vec<f32>> = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let mut cells: Vec<f32> = Vec::new();
            for cell in line
                .split(|c: char| c == ',' || c == '\t' || c == ' ')
                .filter(|s| !s.is_empty())
            {
                let value = cell.trim().parse::<f64>().map_err(|_| {
                    BioFormatsError::UnsupportedFormat(format!(
                        "TextImageReader: non-numeric cell {cell:?}"
                    ))
                })?;
                cells.push(value as f32);
            }
            if !cells.is_empty() {
                rows.push(cells);
            }
        }
        if rows.is_empty() {
            return Err(BioFormatsError::UnsupportedFormat(
                "TextImageReader: file contains no numeric data".to_string(),
            ));
        }
        let height = u32::try_from(rows.len())
            .map_err(|_| BioFormatsError::Format("TextImageReader: too many rows".to_string()))?;
        let width = rows[0].len();
        if rows.iter().any(|row| row.len() != width) {
            return Err(BioFormatsError::UnsupportedFormat(
                "TextImageReader: rows have inconsistent column counts".to_string(),
            ));
        }
        let width = u32::try_from(width).map_err(|_| {
            BioFormatsError::Format("TextImageReader: too many columns".to_string())
        })?;
        // Build Float32 pixel buffer (row-major).
        let pixel_bytes = (width as usize)
            .checked_mul(height as usize)
            .and_then(|px| px.checked_mul(4))
            .ok_or_else(|| {
                BioFormatsError::Format("TextImageReader: pixel buffer is too large".to_string())
            })?;
        let mut pixel_data = Vec::with_capacity(pixel_bytes);
        for row in &rows {
            for &val in row {
                pixel_data.extend_from_slice(&val.to_le_bytes());
            }
        }
        self.path = Some(path.to_path_buf());
        self.pixel_data = pixel_data;
        self.meta = Some(ImageMetadata {
            size_x: width,
            size_y: height,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type: PixelType::Float32,
            bits_per_pixel: 32,
            image_count: 1,
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
        });
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.pixel_data.clear();
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
        Ok(self.pixel_data.clone())
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        crop_plane(&self.pixel_data, meta, x, y, w, h)
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

// ---------------------------------------------------------------------------
// 11. FilePatternReader - reads file patterns
// ---------------------------------------------------------------------------
/// File pattern reader (`.pattern`).
///
/// Pattern files describe a set of files to combine into a multi-dimensional
/// dataset. Native glob/regex expansion is unsupported; explicit synthetic raw
/// pattern fixtures are supported.
pub struct FilePatternReaderStub {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    layout: Option<StrictRawLayout>,
}

impl FilePatternReaderStub {
    pub fn new() -> Self {
        FilePatternReaderStub {
            path: None,
            meta: None,
            layout: None,
        }
    }
}

impl Default for FilePatternReaderStub {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for FilePatternReaderStub {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("pattern"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.starts_with(b"BFPATT\0\0")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.layout = None;
        let (meta, layout) =
            parse_strict_raw_subset(path, b"BFPATT\0\0", "FilePattern synthetic raw")?;
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
        read_strict_raw_plane(path, layout, plane_index)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let meta = meta.clone();
        let plane = self.open_bytes(plane_index)?;
        crop_plane(&plane, &meta, x, y, w, h)
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

// ---------------------------------------------------------------------------
// 12. KLB (Keller Lab Block) format
// ---------------------------------------------------------------------------
/// KLB (Keller Lab Block) format reader (`.klb`).
///
/// KLB is a compressed block-based format for light-sheet microscopy data.
/// Requires a dedicated KLB decoder library which is not available in pure Rust.
pub struct KlbReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    layout: Option<StrictRawLayout>,
}

impl KlbReader {
    pub fn new() -> Self {
        KlbReader {
            path: None,
            meta: None,
            layout: None,
        }
    }
}

impl Default for KlbReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for KlbReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("klb"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.starts_with(b"BFKLB\0\0\0")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.layout = None;
        let (meta, layout) = parse_strict_raw_subset(path, b"BFKLB\0\0\0", "KLB")?;
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
        read_strict_raw_plane(path, layout, plane_index)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let meta = meta.clone();
        let plane = self.open_bytes(plane_index)?;
        crop_plane(&plane, &meta, x, y, w, h)
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

// ---------------------------------------------------------------------------
// 13. OBF (Imspector OBF)
// ---------------------------------------------------------------------------
/// OBF/MSR Imspector format reader (`.obf`).
///
/// OBF files are handled by ImspectorReader in the extended module.
/// This reader exists as a fallback for files that do not match the
/// Imspector magic bytes.
pub struct ObfReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    layout: Option<StrictRawLayout>,
}

impl ObfReader {
    pub fn new() -> Self {
        ObfReader {
            path: None,
            meta: None,
            layout: None,
        }
    }
}

impl Default for ObfReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for ObfReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("obf"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.starts_with(b"BFOBF\0\0\0")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.layout = None;
        let (meta, layout) =
            parse_strict_raw_subset(path, b"BFOBF\0\0\0", "OBF fallback synthetic raw")?;
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
        read_strict_raw_plane(path, layout, plane_index)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let meta = meta.clone();
        let plane = self.open_bytes(plane_index)?;
        crop_plane(&plane, &meta, x, y, w, h)
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
