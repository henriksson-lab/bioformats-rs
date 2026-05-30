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
// 7. PDS — Perkin Elmer Densitometer format
// ---------------------------------------------------------------------------
/// PDS (Perkin Elmer Densitometer) format reader.
///
/// Faithful port of Bio-Formats `loci.formats.in.PDSReader`. PDS is NOT the
/// NASA Planetary-Data-System format: it is a Perkin Elmer densitometer dataset
/// consisting of a text header (`.hdr`/`.pds`, magic ` IDENTIFICATION`) holding
/// `KEY = value / comment` lines, plus a companion binary pixel file
/// (`.IMG`/`.img`). Pixels are always UINT16 little-endian. Each on-disk row is
/// `recordWidth`-aligned: there are `pad = recordWidth - (sizeX % recordWidth)`
/// extra samples of padding after each row of `sizeX` samples. `SIGNX`/`SIGNY`
/// values of `-` request horizontal/vertical mirroring of the plane.
///
/// Relevant Java fields (PDSReader.java):
///   - magic `" IDENTIFICATION"` (15 bytes), `isThisType` (lines 76-92).
///   - `NXP`→sizeX, `NYP`→sizeY (lines 214-219).
///   - `SIGNX`/`SIGNY` (`-` ⇒ reverseX/reverseY, lines 230-235).
///   - `COLOR`: 4 ⇒ RGB sizeC=3; else sizeC=1, lutIndex=color-1 (lines 242-254).
///   - `FILE REC LEN` ⇒ recordWidth = value / 2 (lines 255-257).
///   - pixelType UINT16, littleEndian, dimensionOrder XYCZT (lines 262-267).
///   - companion `base + ".IMG"` then `base + ".img"` (lines 269-273).
///   - `openBytes` pad = recordWidth - (sizeX % recordWidth), realX/realY for
///     reverse, readPlane, then byte-swap mirroring (lines 120-162).
pub struct PdsReader {
    /// Path passed to `set_id` (the header file once resolved).
    header_path: Option<PathBuf>,
    /// Companion pixel file (`.IMG`/`.img`).
    pixels_file: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    record_width: u32,
    reverse_x: bool,
    reverse_y: bool,
}

impl PdsReader {
    pub fn new() -> Self {
        PdsReader {
            header_path: None,
            pixels_file: None,
            meta: None,
            record_width: 0,
            reverse_x: false,
            reverse_y: false,
        }
    }

    /// True if `data` begins with the PDS magic `" IDENTIFICATION"`.
    fn header_has_magic(data: &[u8]) -> bool {
        const MAGIC: &[u8] = b" IDENTIFICATION";
        data.len() >= MAGIC.len() && &data[..MAGIC.len()] == MAGIC
    }

    /// Replace the extension of `path` with `ext` (case as given). Mirrors the
    /// Java `name.substring(0, name.lastIndexOf(".")) + ext` logic.
    fn with_extension(path: &Path, ext: &str) -> Option<PathBuf> {
        let s = path.to_str()?;
        let dot = s.rfind('.')?;
        Some(PathBuf::from(format!("{}.{}", &s[..dot], ext)))
    }

    /// Resolve `path` to its header file (the `.hdr`/`.HDR` sibling when `path`
    /// is a `.img`/companion) and read its raw bytes.
    fn resolve_header(path: &Path) -> Result<(PathBuf, Vec<u8>)> {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        // Java initFile: if the id is not a .hdr, look for sibling .hdr then .HDR.
        if ext.as_deref() != Some("hdr") && ext.as_deref() != Some("pds") {
            for hdr_ext in ["hdr", "HDR"] {
                if let Some(hdr) = Self::with_extension(path, hdr_ext) {
                    if hdr.exists() {
                        let data = std::fs::read(&hdr).map_err(BioFormatsError::Io)?;
                        return Ok((hdr, data));
                    }
                }
            }
            return Err(BioFormatsError::Format(
                "Could not find matching .hdr file.".to_string(),
            ));
        }
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        Ok((path.to_path_buf(), data))
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
        match ext.as_deref() {
            // Java accepts "hdr" (and we also accept "pds") directly when the
            // file content matches; isThisType(name, open=true) defers to the
            // byte check, which we approximate by reading the header here.
            Some("hdr") | Some("pds") => std::fs::read(path)
                .map(|d| Self::header_has_magic(&d))
                .unwrap_or(false),
            // Java: for ".img", look up the sibling ".hdr" and check its magic.
            Some("img") => {
                if let Some(hdr) = Self::with_extension(path, "hdr") {
                    if hdr.exists() {
                        return std::fs::read(&hdr)
                            .map(|d| Self::header_has_magic(&d))
                            .unwrap_or(false);
                    }
                }
                if let Some(hdr) = Self::with_extension(path, "HDR") {
                    if hdr.exists() {
                        return std::fs::read(&hdr)
                            .map(|d| Self::header_has_magic(&d))
                            .unwrap_or(false);
                    }
                }
                false
            }
            _ => false,
        }
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // Java isThisType(stream): the first 15 bytes equal " IDENTIFICATION".
        Self::header_has_magic(header)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.header_path = None;
        self.pixels_file = None;
        self.meta = None;
        self.record_width = 0;
        self.reverse_x = false;
        self.reverse_y = false;

        let (header_path, header_data) = Self::resolve_header(path)?;

        // Java splits on "\r\n"; if that yields one element, it re-splits on
        // "\r". We normalize all line endings and iterate lines.
        let header_text = String::from_utf8_lossy(&header_data);

        let mut size_x: Option<u32> = None;
        let mut size_y: Option<u32> = None;
        let mut size_c: u32 = 1;
        let mut is_rgb = false;
        let mut is_indexed = false;
        let mut record_width: u32 = 0;
        let mut reverse_x = false;
        let mut reverse_y = false;

        for raw_line in header_text.split(['\n', '\r']) {
            let line = raw_line;
            // Java: int eq = line.indexOf('='); if (eq < 0) continue;
            let Some(eq) = line.find('=') else { continue };
            // Java: int end = line.indexOf('/'); if (end < 0) end = line.length();
            let value_end = line.find('/').unwrap_or(line.len());
            if value_end < eq + 1 {
                // A '/' before '=' would make the slice invalid; skip such lines.
                continue;
            }
            let key = line[..eq].trim();
            let value = line[eq + 1..value_end].trim();

            match key {
                "NXP" => {
                    size_x = Some(value.parse::<u32>().map_err(|_| {
                        BioFormatsError::Format("PDS NXP is not a valid integer".to_string())
                    })?);
                }
                "NYP" => {
                    size_y = Some(value.parse::<u32>().map_err(|_| {
                        BioFormatsError::Format("PDS NYP is not a valid integer".to_string())
                    })?);
                }
                "SIGNX" => {
                    reverse_x = value.replace('\'', "").trim() == "-";
                }
                "SIGNY" => {
                    reverse_y = value.replace('\'', "").trim() == "-";
                }
                "COLOR" => {
                    let color = value.parse::<i32>().map_err(|_| {
                        BioFormatsError::Format("PDS COLOR is not a valid integer".to_string())
                    })?;
                    if color == 4 {
                        size_c = 3;
                        is_rgb = true;
                    } else {
                        size_c = 1;
                        is_rgb = false;
                        let lut_index = color - 1;
                        is_indexed = lut_index >= 0;
                    }
                }
                "FILE REC LEN" => {
                    record_width = value
                        .parse::<u32>()
                        .map_err(|_| {
                            BioFormatsError::Format(
                                "PDS FILE REC LEN is not a valid integer".to_string(),
                            )
                        })?
                        / 2;
                }
                _ => {}
            }
        }

        let size_x = size_x.ok_or_else(|| {
            BioFormatsError::Format("PDS header missing NXP keyword".to_string())
        })?;
        let size_y = size_y.ok_or_else(|| {
            BioFormatsError::Format("PDS header missing NYP keyword".to_string())
        })?;
        if size_x == 0 || size_y == 0 {
            return Err(BioFormatsError::Format(
                "PDS NXP/NYP must be non-zero".to_string(),
            ));
        }
        // pad = recordWidth - (sizeX % recordWidth) requires recordWidth > 0.
        if record_width == 0 {
            return Err(BioFormatsError::Format(
                "PDS header missing FILE REC LEN keyword".to_string(),
            ));
        }

        // Resolve companion pixel file: base + ".IMG" then base + ".img".
        let base = Self::with_extension(&header_path, "IMG").ok_or_else(|| {
            BioFormatsError::Format("PDS header path has no extension".to_string())
        })?;
        let pixels_file = if base.exists() {
            base
        } else {
            Self::with_extension(&header_path, "img").ok_or_else(|| {
                BioFormatsError::Format("PDS header path has no extension".to_string())
            })?
        };
        if !pixels_file.exists() {
            return Err(BioFormatsError::Format(
                "PDS companion .IMG/.img pixel file not found".to_string(),
            ));
        }

        // Validate the companion file is large enough for the declared plane,
        // so truncated datasets fail in set_id like the reference would on read.
        let pad = record_width - (size_x % record_width);
        let scanline = (size_x as u64) + (pad as u64);
        let required = scanline
            .checked_mul(size_y as u64)
            .and_then(|rows| rows.checked_mul(size_c as u64))
            .and_then(|samples| samples.checked_mul(2)) // UINT16
            .ok_or_else(|| BioFormatsError::Format("PDS plane is too large".to_string()))?;
        let available = std::fs::metadata(&pixels_file)
            .map_err(BioFormatsError::Io)?
            .len();
        if available < required {
            return Err(BioFormatsError::UnsupportedFormat(
                "PDS companion file is shorter than declared image dimensions".to_string(),
            ));
        }

        self.record_width = record_width;
        self.reverse_x = reverse_x;
        self.reverse_y = reverse_y;
        self.header_path = Some(header_path);
        self.pixels_file = Some(pixels_file);
        self.meta = Some(ImageMetadata {
            size_x,
            size_y,
            size_z: 1,
            size_c,
            size_t: 1,
            pixel_type: PixelType::Uint16,
            bits_per_pixel: 16,
            image_count: 1,
            // Java: dimensionOrder = "XYCZT".
            dimension_order: DimensionOrder::XYCZT,
            is_rgb,
            // Java leaves interleaved at its default (false) -> planar RGB.
            is_interleaved: false,
            is_indexed,
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
        self.header_path = None;
        self.pixels_file = None;
        self.meta = None;
        self.record_width = 0;
        self.reverse_x = false;
        self.reverse_y = false;
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
        let (size_x, size_y) = (meta.size_x, meta.size_y);
        self.open_bytes_region(plane_index, 0, 0, size_x, size_y)
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
        let size_x = meta.size_x;
        let size_y = meta.size_y;
        let size_c = meta.size_c;
        if x.checked_add(w).is_none_or(|end| end > size_x)
            || y.checked_add(h).is_none_or(|end| end > size_y)
        {
            return Err(BioFormatsError::Format(
                "requested region is outside the image bounds".to_string(),
            ));
        }

        let pixels_file = self
            .pixels_file
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?;
        let data = std::fs::read(pixels_file).map_err(BioFormatsError::Io)?;

        let bpp = 2usize; // UINT16
        // Java: pad = recordWidth - (sizeX % recordWidth)
        let pad = self.record_width - (size_x % self.record_width);
        let scanline = (size_x + pad) as usize; // samples per on-disk row
        // On-disk size (in samples) of one full padded channel plane.
        let channel_plane = scanline
            .checked_mul(size_y as usize)
            .ok_or_else(|| BioFormatsError::Format("PDS plane is too large".to_string()))?;

        // Java: realX/realY flip the read origin when mirroring is requested.
        let real_x = if self.reverse_x {
            size_x - w - x
        } else {
            x
        };
        let real_y = if self.reverse_y {
            size_y - h - y
        } else {
            y
        };

        // Output buffer: planar, tightly packed (w*h per channel), w*bpp stride.
        let out_channel_bytes = (w as usize)
            .checked_mul(h as usize)
            .and_then(|px| px.checked_mul(bpp))
            .ok_or_else(|| BioFormatsError::Format("PDS region is too large".to_string()))?;
        let total = out_channel_bytes
            .checked_mul(size_c as usize)
            .ok_or_else(|| BioFormatsError::Format("PDS region is too large".to_string()))?;
        let mut buf = vec![0u8; total];

        // readPlane (non-interleaved): for each channel, for each row, copy a
        // contiguous run of w samples starting at realX within that on-disk row.
        for channel in 0..size_c as usize {
            let channel_base_samples = channel * channel_plane;
            for row in 0..h as usize {
                let src_sample =
                    channel_base_samples + (real_y as usize + row) * scanline + real_x as usize;
                let src_byte = src_sample * bpp;
                let run = w as usize * bpp;
                let src_end = src_byte + run;
                if src_end > data.len() {
                    return Err(BioFormatsError::Format(
                        "PDS companion file is shorter than expected".to_string(),
                    ));
                }
                let dst = channel * out_channel_bytes + row * (w as usize) * bpp;
                buf[dst..dst + run].copy_from_slice(&data[src_byte..src_end]);
            }
        }

        // Java reverseX: swap UINT16 samples within each row (per channel).
        if self.reverse_x {
            for channel in 0..size_c as usize {
                let cbase = channel * out_channel_bytes;
                for row in 0..h as usize {
                    let rbase = cbase + row * (w as usize) * bpp;
                    for col in 0..(w as usize) / 2 {
                        let begin = rbase + 2 * col;
                        let end = rbase + 2 * (w as usize - col - 1);
                        buf.swap(begin, end);
                        buf.swap(begin + 1, end + 1);
                    }
                }
            }
        }

        // Java reverseY: swap whole rows top-to-bottom (per channel).
        if self.reverse_y {
            let row_bytes = (w as usize) * bpp;
            for channel in 0..size_c as usize {
                let cbase = channel * out_channel_bytes;
                for row in 0..(h as usize) / 2 {
                    let start = cbase + row * row_bytes;
                    let end = cbase + (h as usize - row - 1) * row_bytes;
                    for k in 0..row_bytes {
                        buf.swap(start + k, end + k);
                    }
                }
            }
        }

        Ok(buf)
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

        let mut metas: Vec<ImageMetadata> = Vec::with_capacity(series_count);
        let mut pixel_offsets: Vec<u64> = Vec::with_capacity(series_count);
        let mut packed_12_bit: Vec<bool> = Vec::with_capacity(series_count);
        let mut offset = 0usize;
        // Java HISReader.initFile (lines 129, 138-148): a series after the first
        // that does not start with the "IM" magic indicates that the previous
        // 12-bit plane was actually stored padded to 16 bits. When that happens
        // we retroactively promote the previous series to 16-bit, recompute its
        // (padded) plane size so the current series begins at the correct
        // offset, and latch `adjusted_bit_depth` so the 12-bit data types (6 and
        // 14) are treated as 16-bit for the remainder of the file.
        let mut adjusted_bit_depth = false;
        for series in 0..series_count {
            if offset.checked_add(64).is_none_or(|end| end > data.len()) {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "HIS series {series} header is truncated"
                )));
            }
            if &data[offset..offset + 2] != b"IM" {
                // Mirror Java: only the previous series being 12-bit allows us to
                // recover; otherwise the magic really is missing/corrupt.
                if series > 0 && metas[series - 1].bits_per_pixel == 12 {
                    let prev = &mut metas[series - 1];
                    prev.bits_per_pixel = 16;
                    // prevSkip = sizeX*sizeY*sizeC*12/8 (already-consumed packed
                    // plane); totalBytes = sizeX*sizeY*sizeC*2 (16-bit padded).
                    let prev_samples = (prev.size_x as u64)
                        .checked_mul(prev.size_y as u64)
                        .and_then(|px| px.checked_mul(prev.size_c as u64))
                        .ok_or_else(|| {
                            BioFormatsError::Format("HIS image plane is too large".to_string())
                        })?;
                    let prev_pixel_offset = pixel_offsets[series - 1];
                    let total_bytes = prev_samples.checked_mul(2).ok_or_else(|| {
                        BioFormatsError::Format("HIS image plane is too large".to_string())
                    })?;
                    // The previous (12-bit packed) plane is no longer valid; this
                    // series really starts after the 16-bit padded plane.
                    packed_12_bit[series - 1] = false;
                    offset = (prev_pixel_offset + total_bytes) as usize;
                    adjusted_bit_depth = true;

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
                } else {
                    return Err(BioFormatsError::UnsupportedFormat(format!(
                        "HIS series {series} missing IM magic"
                    )));
                }
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

            // Java: data types 6 and 14 are nominally 12-bit, but once a prior
            // series has been promoted (`adjusted_bit_depth`) they are stored as
            // unpacked 16-bit samples.
            let (pixel_type, bits_per_pixel, size_c, bytes_per_sample, is_packed_12) =
                match data_type {
                    1 => (PixelType::Uint8, 8u8, 1u32, 1u64, false),
                    2 => (PixelType::Uint16, 16u8, 1u32, 2u64, false),
                    6 if adjusted_bit_depth => (PixelType::Uint16, 16u8, 1u32, 2u64, false),
                    6 => (PixelType::Uint16, 12u8, 1u32, 2u64, true),
                    11 => (PixelType::Uint8, 8u8, 3u32, 1u64, false),
                    12 => (PixelType::Uint16, 16u8, 3u32, 2u64, false),
                    14 if adjusted_bit_depth => (PixelType::Uint16, 16u8, 3u32, 2u64, false),
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


#[cfg(test)]
mod pds_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Build a unique base path (no intermediate dots) in the temp directory.
    fn unique_base(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("bioformats_pds_{tag}_{nanos}_{n}"))
    }

    /// Encode `samples` as little-endian UINT16 bytes.
    fn le_u16(samples: &[u16]) -> Vec<u8> {
        let mut v = Vec::with_capacity(samples.len() * 2);
        for s in samples {
            v.extend_from_slice(&s.to_le_bytes());
        }
        v
    }

    /// Write a minimal grayscale PDS fixture: `.hdr` + companion `.IMG`.
    ///
    /// Header carries the ` IDENTIFICATION` magic, `NXP`/`NYP`, `COLOR = 1`
    /// (grayscale, lutIndex 0), `FILE REC LEN` (= 2 * record_width), and the
    /// requested SIGNX/SIGNY. The companion holds `(size_x + pad)` UINT16
    /// samples per row, where `pad = record_width - (size_x % record_width)`.
    /// `pixels` must be a row-major `size_x * size_y` grid of sample values.
    fn write_gray_fixture(
        tag: &str,
        size_x: u32,
        size_y: u32,
        record_width: u32,
        signx: &str,
        signy: &str,
        pixels: &[u16],
    ) -> (PathBuf, PathBuf) {
        assert_eq!(pixels.len(), (size_x * size_y) as usize);
        let base = unique_base(tag);
        let hdr = base.with_extension("hdr");
        let img = base.with_extension("IMG");

        let header = format!(
            " IDENTIFICATION\r\n\
             NXP = {size_x} / x samples\r\n\
             NYP = {size_y} / y samples\r\n\
             SIGNX = '{signx}' / x sign\r\n\
             SIGNY = '{signy}' / y sign\r\n\
             COLOR = 1 / grayscale\r\n\
             FILE REC LEN = {rec_len} / record length in bytes\r\n\
             END\r\n",
            rec_len = record_width * 2,
        );
        std::fs::write(&hdr, header.as_bytes()).unwrap();

        // Companion: one padded row at a time. Padding samples are sentinel
        // 0xFFFF so a bug that reads padding instead of real data is visible.
        let pad = record_width - (size_x % record_width);
        let mut img_samples: Vec<u16> = Vec::new();
        for row in 0..size_y as usize {
            let start = row * size_x as usize;
            img_samples.extend_from_slice(&pixels[start..start + size_x as usize]);
            img_samples.extend(std::iter::repeat(0xFFFFu16).take(pad as usize));
        }
        std::fs::write(&img, le_u16(&img_samples)).unwrap();

        (hdr, img)
    }

    fn cleanup(paths: &[&PathBuf]) {
        for p in paths {
            let _ = std::fs::remove_file(p);
        }
    }

    #[test]
    fn pds_grayscale_full_and_region() {
        // 3x2 image; record_width = 4 => pad = 4 - (3 % 4) = 1 sample per row.
        let size_x = 3u32;
        let size_y = 2u32;
        let record_width = 4u32;
        let pixels: Vec<u16> = vec![
            10, 20, 30, // row 0
            40, 50, 60, // row 1
        ];
        let (hdr, img) =
            write_gray_fixture("gray", size_x, size_y, record_width, "+", "+", &pixels);

        let mut r = PdsReader::new();
        // Detection by name (header magic present).
        assert!(r.is_this_type_by_name(&hdr));
        // Detection of the companion via sibling header.
        assert!(r.is_this_type_by_name(&img));
        // Magic byte detection.
        assert!(r.is_this_type_by_bytes(b" IDENTIFICATION extra"));
        assert!(!r.is_this_type_by_bytes(b"NOT A PDS FILE."));

        r.set_id(&hdr).unwrap();
        let meta = r.metadata();
        assert_eq!(meta.size_x, size_x);
        assert_eq!(meta.size_y, size_y);
        assert_eq!(meta.size_c, 1);
        assert_eq!(meta.pixel_type, PixelType::Uint16);
        assert!(meta.is_little_endian);
        assert!(!meta.is_rgb);

        // Full plane: padding must be stripped, row-major order preserved.
        let full = r.open_bytes(0).unwrap();
        assert_eq!(full, le_u16(&pixels));

        // Region crop: 2x2 starting at (1,0) => columns 1,2 of both rows.
        let region = r.open_bytes_region(0, 1, 0, 2, 2).unwrap();
        assert_eq!(region, le_u16(&[20, 30, 50, 60]));

        // Single-pixel crop bottom-right.
        let one = r.open_bytes_region(0, 2, 1, 1, 1).unwrap();
        assert_eq!(one, le_u16(&[60]));

        // Out-of-bounds region rejected.
        assert!(r.open_bytes_region(0, 2, 0, 2, 1).is_err());

        cleanup(&[&hdr, &img]);
    }

    #[test]
    fn pds_grayscale_reverse_xy() {
        // SIGNX = '-' and SIGNY = '-' mirror horizontally and vertically.
        let size_x = 3u32;
        let size_y = 2u32;
        let record_width = 4u32;
        let pixels: Vec<u16> = vec![
            10, 20, 30, // row 0
            40, 50, 60, // row 1
        ];
        let (hdr, img) =
            write_gray_fixture("rev", size_x, size_y, record_width, "-", "-", &pixels);

        let mut r = PdsReader::new();
        r.set_id(&hdr).unwrap();
        assert!(r.reverse_x);
        assert!(r.reverse_y);

        // Full plane mirrored in both axes:
        // Original rows: [10,20,30],[40,50,60]
        // reverseX per row: [30,20,10],[60,50,40]
        // reverseY swaps rows: [60,50,40],[30,20,10]
        let full = r.open_bytes(0).unwrap();
        assert_eq!(full, le_u16(&[60, 50, 40, 30, 20, 10]));

        cleanup(&[&hdr, &img]);
    }

    #[test]
    fn pds_reject_missing_companion() {
        // Header present and valid, but no .IMG/.img companion exists.
        let base = unique_base("nocomp");
        let hdr = base.with_extension("hdr");
        let header = " IDENTIFICATION\r\n\
             NXP = 4 / x\r\n\
             NYP = 4 / y\r\n\
             COLOR = 1 /\r\n\
             FILE REC LEN = 8 /\r\n\
             END\r\n";
        std::fs::write(&hdr, header).unwrap();

        let mut r = PdsReader::new();
        assert!(r.set_id(&hdr).is_err());
        // State stays uninitialized after the failure.
        assert_eq!(r.series_count(), 0);

        cleanup(&[&hdr]);
    }

    #[test]
    fn pds_reject_truncated_companion() {
        // Companion exists but is shorter than the declared (padded) plane.
        let base = unique_base("trunc");
        let hdr = base.with_extension("hdr");
        let img = base.with_extension("IMG");
        let header = " IDENTIFICATION\r\n\
             NXP = 8 / x\r\n\
             NYP = 8 / y\r\n\
             COLOR = 1 /\r\n\
             FILE REC LEN = 16 /\r\n\
             END\r\n";
        std::fs::write(&hdr, header).unwrap();
        // Only a handful of bytes, far short of 8 rows of (8 + pad) UINT16.
        std::fs::write(&img, [0u8; 16]).unwrap();

        let mut r = PdsReader::new();
        assert!(r.set_id(&hdr).is_err());

        cleanup(&[&hdr, &img]);
    }
}
