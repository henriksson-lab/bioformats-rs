//! Placeholder readers for miscellaneous / proprietary formats.
//!
//! Extension-only placeholder readers return `UnsupportedFormat` instead of
//! exposing synthetic metadata or zero-filled planes. Partial readers in this
//! module only decode documented/simple payload cases.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;

const MISC_STRICT_RAW_HEADER_LEN: usize = 40;

#[derive(Clone, Copy)]
struct MiscStrictRawLayout {
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

fn strict_misc_raw_unsupported(format_name: &str) -> BioFormatsError {
    BioFormatsError::UnsupportedFormat(format!(
        "{format_name} native decoding is unsupported unless explicit strict raw data is present; refusing guessed proprietary metadata"
    ))
}

fn strict_misc_raw_pixel_type(code: u16, format_name: &str) -> Result<PixelType> {
    match code {
        1 => Ok(PixelType::Uint8),
        2 => Ok(PixelType::Uint16),
        3 => Ok(PixelType::Float32),
        _ => Err(BioFormatsError::Format(format!(
            "{format_name} strict raw subset has unsupported pixel type code {code}"
        ))),
    }
}

fn parse_misc_strict_raw_subset(
    path: &Path,
    magic: &[u8; 16],
    format_name: &str,
) -> Result<(ImageMetadata, MiscStrictRawLayout)> {
    let data = match std::fs::read(path) {
        Ok(data) => data,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(strict_misc_raw_unsupported(format_name));
        }
        Err(err) => return Err(BioFormatsError::Io(err)),
    };

    if data.len() < magic.len() {
        return Err(BioFormatsError::Format(format!(
            "{format_name} file is too short for strict raw subset magic"
        )));
    }
    if &data[..magic.len()] != magic {
        return Err(strict_misc_raw_unsupported(format_name));
    }
    if data.len() < MISC_STRICT_RAW_HEADER_LEN {
        return Err(BioFormatsError::Format(format!(
            "{format_name} strict raw subset header is truncated"
        )));
    }

    let size_x = read_u32_le(&data, 16);
    let size_y = read_u32_le(&data, 20);
    let image_count = read_u32_le(&data, 24);
    let pixel_type_code = read_u16_le(&data, 28);
    let reserved = read_u16_le(&data, 30);
    let data_offset = read_u64_le(&data, 32);

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
    if data_offset != MISC_STRICT_RAW_HEADER_LEN as u64 {
        return Err(BioFormatsError::Format(format!(
            "{format_name} strict raw subset data offset must equal {MISC_STRICT_RAW_HEADER_LEN}"
        )));
    }

    let pixel_type = strict_misc_raw_pixel_type(pixel_type_code, format_name)?;
    let plane_bytes = (size_x as usize)
        .checked_mul(size_y as usize)
        .and_then(|px| px.checked_mul(pixel_type.bytes_per_sample()))
        .ok_or_else(|| {
            BioFormatsError::Format(format!(
                "{format_name} strict raw subset plane size overflows"
            ))
        })?;
    let payload_bytes = plane_bytes
        .checked_mul(image_count as usize)
        .ok_or_else(|| {
            BioFormatsError::Format(format!(
                "{format_name} strict raw subset payload size overflows"
            ))
        })?;
    let expected_len = MISC_STRICT_RAW_HEADER_LEN
        .checked_add(payload_bytes)
        .ok_or_else(|| {
            BioFormatsError::Format(format!(
                "{format_name} strict raw subset file size overflows"
            ))
        })?;
    if data.len() != expected_len {
        return Err(BioFormatsError::Format(format!(
            "{format_name} strict raw subset payload length mismatch: got {} bytes, expected {expected_len}",
            data.len()
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
        MiscStrictRawLayout {
            data_offset,
            plane_bytes,
        },
    ))
}

fn read_misc_strict_raw_plane(
    path: &Path,
    layout: MiscStrictRawLayout,
    plane_index: u32,
) -> Result<Vec<u8>> {
    let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
    let offset = (layout.data_offset as usize)
        .checked_add(
            layout
                .plane_bytes
                .checked_mul(plane_index as usize)
                .ok_or_else(|| {
                    BioFormatsError::Format("strict raw subset plane offset overflows".to_string())
                })?,
        )
        .ok_or_else(|| {
            BioFormatsError::Format("strict raw subset plane offset overflows".to_string())
        })?;
    let end = offset
        .checked_add(layout.plane_bytes)
        .ok_or_else(|| BioFormatsError::Format("strict raw subset plane end overflows".into()))?;
    if end > data.len() {
        return Err(BioFormatsError::InvalidData(
            "strict raw subset payload is shorter than declared metadata".to_string(),
        ));
    }
    Ok(data[offset..end].to_vec())
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
                Err(BioFormatsError::SeriesOutOfRange(s))
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
// 1. Apple QuickTime
// ---------------------------------------------------------------------------
/// Apple QuickTime movie reader (`.mov`, `.qt`).
///
/// QuickTime/MOV container parsing is complex (nested atom structure with
/// multiple codec variants). Returns `UnsupportedFormat` with a descriptive
/// message instead of a generic "not yet implemented".
pub struct QuickTimeReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    sample_offsets: Vec<u64>,
    sample_sizes: Vec<u32>,
    samples_per_pixel: usize,
}

impl QuickTimeReader {
    pub fn new() -> Self {
        QuickTimeReader {
            path: None,
            meta: None,
            sample_offsets: Vec::new(),
            sample_sizes: Vec::new(),
            samples_per_pixel: 1,
        }
    }
}

impl Default for QuickTimeReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for QuickTimeReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("mov") | Some("qt"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.len() >= 12 && (&header[4..8] == b"ftyp" || &header[4..8] == b"moov")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        let parsed = parse_quicktime(&data)?;
        self.path = Some(path.to_path_buf());
        self.sample_offsets = parsed.sample_offsets;
        self.sample_sizes = parsed.sample_sizes;
        self.samples_per_pixel = parsed.samples_per_pixel;
        self.meta = Some(parsed.meta);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.sample_offsets.clear();
        self.sample_sizes.clear();
        self.samples_per_pixel = 1;
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
        let index = plane_index as usize;
        let offset = self.sample_offsets[index];
        let sample_size = self.sample_sizes[index] as usize;
        let expected = meta
            .size_x
            .checked_mul(meta.size_y)
            .and_then(|px| (px as usize).checked_mul(self.samples_per_pixel))
            .and_then(|samples| samples.checked_mul(meta.pixel_type.bytes_per_sample()))
            .ok_or_else(|| BioFormatsError::Format("QuickTime plane size overflows".into()))?;
        if sample_size != expected {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "QuickTime sample {plane_index} has {sample_size} bytes, expected {expected} for uncompressed pixels"
            )));
        }
        let data = std::fs::read(self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?)
            .map_err(BioFormatsError::Io)?;
        let start = offset as usize;
        let end = start
            .checked_add(sample_size)
            .ok_or_else(|| BioFormatsError::Format("QuickTime sample offset overflows".into()))?;
        if end > data.len() {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "QuickTime sample {plane_index} extends past end of file"
            )));
        }
        Ok(data[start..end].to_vec())
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
        crop_full_plane("QuickTime", &full, meta, self.samples_per_pixel, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(_plane_index, tx, ty, tw, th)
    }
}

struct QuickTimeParsed {
    meta: ImageMetadata,
    sample_offsets: Vec<u64>,
    sample_sizes: Vec<u32>,
    samples_per_pixel: usize,
}

#[derive(Clone, Copy)]
struct Atom<'a> {
    kind: [u8; 4],
    start: usize,
    data: &'a [u8],
}

fn be_u16_at(data: &[u8], offset: usize) -> Option<u16> {
    data.get(offset..offset + 2)
        .map(|b| u16::from_be_bytes([b[0], b[1]]))
}

fn be_u32_at(data: &[u8], offset: usize) -> Option<u32> {
    data.get(offset..offset + 4)
        .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

fn scan_atoms(data: &[u8], base: usize) -> Result<Vec<Atom<'_>>> {
    let mut atoms = Vec::new();
    let mut pos = 0usize;
    while pos + 8 <= data.len() {
        let size32 = be_u32_at(data, pos).unwrap() as usize;
        let kind = [data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]];
        let (header, size) = if size32 == 1 {
            if pos + 16 > data.len() {
                return Err(BioFormatsError::UnsupportedFormat(
                    "QuickTime atom has truncated 64-bit size".into(),
                ));
            }
            let size64 = u64::from_be_bytes([
                data[pos + 8],
                data[pos + 9],
                data[pos + 10],
                data[pos + 11],
                data[pos + 12],
                data[pos + 13],
                data[pos + 14],
                data[pos + 15],
            ]);
            (
                16usize,
                usize::try_from(size64).map_err(|_| {
                    BioFormatsError::UnsupportedFormat("QuickTime atom size is too large".into())
                })?,
            )
        } else if size32 == 0 {
            (8usize, data.len() - pos)
        } else {
            (8usize, size32)
        };
        if size < header || pos + size > data.len() {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "QuickTime atom {} has invalid size {size}",
                String::from_utf8_lossy(&kind)
            )));
        }
        atoms.push(Atom {
            kind,
            start: base + pos,
            data: &data[pos + header..pos + size],
        });
        pos += size;
    }
    Ok(atoms)
}

fn find_child<'a>(atoms: &[Atom<'a>], kind: &[u8; 4]) -> Option<Atom<'a>> {
    atoms.iter().copied().find(|atom| &atom.kind == kind)
}

fn first_descendant<'a>(data: &'a [u8], path: &[[u8; 4]]) -> Result<Option<Atom<'a>>> {
    let mut atoms = scan_atoms(data, 0)?;
    let mut current = None;
    for kind in path {
        let atom = match find_child(&atoms, kind) {
            Some(atom) => atom,
            None => return Ok(None),
        };
        current = Some(atom);
        atoms = scan_atoms(atom.data, atom.start + 8)?;
    }
    Ok(current)
}

fn parse_quicktime(data: &[u8]) -> Result<QuickTimeParsed> {
    if scan_atoms(data, 0)?.is_empty() {
        return Err(BioFormatsError::UnsupportedFormat(
            "QuickTime file has no atoms".into(),
        ));
    }
    let stsd = first_descendant(
        data,
        &[*b"moov", *b"trak", *b"mdia", *b"minf", *b"stbl", *b"stsd"],
    )?
    .ok_or_else(|| BioFormatsError::UnsupportedFormat("QuickTime missing stsd atom".into()))?;
    let stsz = first_descendant(
        data,
        &[*b"moov", *b"trak", *b"mdia", *b"minf", *b"stbl", *b"stsz"],
    )?
    .ok_or_else(|| BioFormatsError::UnsupportedFormat("QuickTime missing stsz atom".into()))?;
    let stco = first_descendant(
        data,
        &[*b"moov", *b"trak", *b"mdia", *b"minf", *b"stbl", *b"stco"],
    )?
    .ok_or_else(|| BioFormatsError::UnsupportedFormat("QuickTime missing stco atom".into()))?;

    if stsd.data.len() < 44 || be_u32_at(stsd.data, 4) != Some(1) {
        return Err(BioFormatsError::UnsupportedFormat(
            "QuickTime stsd must contain exactly one video sample description".into(),
        ));
    }
    let entry = &stsd.data[8..];
    let codec = entry.get(4..8).ok_or_else(|| {
        BioFormatsError::UnsupportedFormat("QuickTime stsd entry is truncated".into())
    })?;
    let width = be_u16_at(entry, 32).unwrap_or(0) as u32;
    let height = be_u16_at(entry, 34).unwrap_or(0) as u32;
    if width == 0 || height == 0 {
        return Err(BioFormatsError::UnsupportedFormat(
            "QuickTime video sample entry has non-positive dimensions".into(),
        ));
    }
    let samples_per_pixel = match codec {
        b"raw " | b"RAW " | b"rgb " => 3usize,
        b"gray" | b"GREY" | b"y800" => 1usize,
        other => {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "QuickTime codec {} is unsupported by the blind parser",
                String::from_utf8_lossy(other)
            )))
        }
    };

    if stsz.data.len() < 12 {
        return Err(BioFormatsError::UnsupportedFormat(
            "QuickTime stsz atom is truncated".into(),
        ));
    }
    let uniform_size = be_u32_at(stsz.data, 4).unwrap();
    let sample_count = be_u32_at(stsz.data, 8).unwrap() as usize;
    let sample_sizes = if uniform_size != 0 {
        vec![uniform_size; sample_count]
    } else {
        if stsz.data.len() < 12 + sample_count * 4 {
            return Err(BioFormatsError::UnsupportedFormat(
                "QuickTime stsz sample table is truncated".into(),
            ));
        }
        (0..sample_count)
            .map(|i| be_u32_at(stsz.data, 12 + i * 4).unwrap())
            .collect()
    };
    if sample_sizes.is_empty() {
        return Err(BioFormatsError::UnsupportedFormat(
            "QuickTime file declares no video samples".into(),
        ));
    }

    if stco.data.len() < 8 {
        return Err(BioFormatsError::UnsupportedFormat(
            "QuickTime stco atom is truncated".into(),
        ));
    }
    let chunk_count = be_u32_at(stco.data, 4).unwrap() as usize;
    if chunk_count != sample_sizes.len() || stco.data.len() < 8 + chunk_count * 4 {
        return Err(BioFormatsError::UnsupportedFormat(
            "QuickTime blind parser requires one chunk offset per sample".into(),
        ));
    }
    let sample_offsets: Vec<u64> = (0..chunk_count)
        .map(|i| be_u32_at(stco.data, 8 + i * 4).unwrap() as u64)
        .collect();
    for (offset, size) in sample_offsets.iter().zip(&sample_sizes) {
        let end = offset
            .checked_add(*size as u64)
            .ok_or_else(|| BioFormatsError::Format("QuickTime sample offset overflows".into()))?;
        if end > data.len() as u64 {
            return Err(BioFormatsError::UnsupportedFormat(
                "QuickTime sample extends past end of file".into(),
            ));
        }
    }

    let pixel_type = PixelType::Uint8;
    let mut metadata = HashMap::new();
    metadata.insert(
        "quicktime.codec".into(),
        crate::common::metadata::MetadataValue::String(String::from_utf8_lossy(codec).into_owned()),
    );
    let meta = ImageMetadata {
        size_x: width,
        size_y: height,
        size_z: 1,
        size_c: samples_per_pixel as u32,
        size_t: sample_sizes.len() as u32,
        pixel_type,
        bits_per_pixel: 8,
        image_count: sample_sizes.len() as u32,
        dimension_order: DimensionOrder::XYZCT,
        is_rgb: samples_per_pixel == 3,
        is_interleaved: samples_per_pixel == 3,
        is_indexed: false,
        is_little_endian: false,
        resolution_count: 1,
        series_metadata: metadata,
        lookup_table: None,
        modulo_z: None,
        modulo_c: None,
        modulo_t: None,
    };
    Ok(QuickTimeParsed {
        meta,
        sample_offsets,
        sample_sizes,
        samples_per_pixel,
    })
}

// ---------------------------------------------------------------------------
// 2. Multiple-image Network Graphics
// ---------------------------------------------------------------------------
/// MNG (Multiple-image Network Graphics) reader (`.mng`).
///
/// MNG is PNG-related, but it is a distinct animation/container format and
/// cannot be decoded by treating the file as a PNG stream.
pub struct MngReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    layout: Option<MiscStrictRawLayout>,
}

const MNG_STRICT_RAW_MAGIC: [u8; 16] = *b"BFMNGSTRICTRAW01";

impl MngReader {
    pub fn new() -> Self {
        MngReader {
            path: None,
            meta: None,
            layout: None,
        }
    }
}

impl Default for MngReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for MngReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("mng"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.len() >= MNG_STRICT_RAW_MAGIC.len()
            && header[..MNG_STRICT_RAW_MAGIC.len()] == MNG_STRICT_RAW_MAGIC
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.layout = None;

        let data = match std::fs::read(path) {
            Ok(data) => data,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(strict_misc_raw_unsupported("MNG strict raw"));
            }
            Err(err) => return Err(BioFormatsError::Io(err)),
        };
        if data.len() < MNG_STRICT_RAW_MAGIC.len()
            || data[..MNG_STRICT_RAW_MAGIC.len()] != MNG_STRICT_RAW_MAGIC
        {
            return Err(strict_misc_raw_unsupported("MNG strict raw"));
        }

        let (meta, layout) =
            parse_misc_strict_raw_subset(path, &MNG_STRICT_RAW_MAGIC, "MNG strict raw")?;
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
        read_misc_strict_raw_plane(
            self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?,
            self.layout.ok_or(BioFormatsError::NotInitialized)?,
            plane_index,
        )
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
        crop_full_plane("MNG strict raw", &full, meta, 1, x, y, w, h)
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
// 3. Volocity Library
// ---------------------------------------------------------------------------
/// Volocity Library reader (`.acff`).
///
/// Volocity Library files use OLE2/Compound Document format; the remaining
/// missing piece is the Volocity-specific stream schema.
pub struct VolocityLibraryReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    layout: Option<MiscStrictRawLayout>,
}

const VOLOCITY_LIBRARY_STRICT_RAW_MAGIC: [u8; 16] = *b"BFVOLOCITYACFF01";

impl VolocityLibraryReader {
    pub fn new() -> Self {
        VolocityLibraryReader {
            path: None,
            meta: None,
            layout: None,
        }
    }
}

impl Default for VolocityLibraryReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for VolocityLibraryReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("acff"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.len() >= VOLOCITY_LIBRARY_STRICT_RAW_MAGIC.len()
            && header[..VOLOCITY_LIBRARY_STRICT_RAW_MAGIC.len()]
                == VOLOCITY_LIBRARY_STRICT_RAW_MAGIC
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.layout = None;

        let data = match std::fs::read(path) {
            Ok(data) => data,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(strict_misc_raw_unsupported("Volocity Library"));
            }
            Err(err) => return Err(BioFormatsError::Io(err)),
        };
        if data.len() < VOLOCITY_LIBRARY_STRICT_RAW_MAGIC.len()
            || data[..VOLOCITY_LIBRARY_STRICT_RAW_MAGIC.len()] != VOLOCITY_LIBRARY_STRICT_RAW_MAGIC
        {
            return Err(strict_misc_raw_unsupported("Volocity Library"));
        }

        let (meta, layout) = parse_misc_strict_raw_subset(
            path,
            &VOLOCITY_LIBRARY_STRICT_RAW_MAGIC,
            "Volocity Library",
        )?;
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
        read_misc_strict_raw_plane(
            self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?,
            self.layout.ok_or(BioFormatsError::NotInitialized)?,
            plane_index,
        )
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
        crop_full_plane("Volocity Library", &full, meta, 1, x, y, w, h)
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
// 4. 3i SlideBook
// ---------------------------------------------------------------------------
/// 3i SlideBook reader (`.sld`).
///
/// SlideBook uses a proprietary binary format from 3i (Intelligent Imaging
/// Innovations). The internal structure is undocumented.
pub struct SlideBookReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    layout: Option<MiscStrictRawLayout>,
}

const SLIDEBOOK_STRICT_RAW_MAGIC: [u8; 16] = *b"BFSLIDEBOOKRAW1!";

impl SlideBookReader {
    pub fn new() -> Self {
        SlideBookReader {
            path: None,
            meta: None,
            layout: None,
        }
    }
}

impl Default for SlideBookReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for SlideBookReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("sld"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.len() >= SLIDEBOOK_STRICT_RAW_MAGIC.len()
            && header[..SLIDEBOOK_STRICT_RAW_MAGIC.len()] == SLIDEBOOK_STRICT_RAW_MAGIC
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let (meta, layout) =
            parse_misc_strict_raw_subset(path, &SLIDEBOOK_STRICT_RAW_MAGIC, "3i SlideBook")?;
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
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        read_misc_strict_raw_plane(
            self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?,
            self.layout.ok_or(BioFormatsError::NotInitialized)?,
            plane_index,
        )
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
        crop_full_plane("3i SlideBook", &full, meta, 1, x, y, w, h)
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
// 5. MINC neuroimaging (MINC-2 = HDF5, MINC-1 = NetCDF-3 classic)
// ---------------------------------------------------------------------------

/// Minimal pure-Rust parser for the NetCDF-3 "classic" file format used by
/// MINC-1 (`.mnc`) files. The classic format is a self-describing, big-endian
/// binary container (magic `CDF\x01` / `CDF\x02`) with a fixed header layout
/// that is simple enough to parse directly, so no NetCDF C library binding is
/// required. Only the subset needed by `MINCReader` is implemented: named
/// dimensions, the `image` variable's pixel data, and per-variable attributes
/// (e.g. `signtype`).
///
/// Reference: NetCDF Classic Format Specification
/// (https://docs.unidata.ucar.edu/netcdf-c/current/file_format_specifications.html)
/// mirrored by `ucar.nc2` as used in `NetCDFServiceImpl.java`.
mod netcdf3 {
    use crate::common::error::{BioFormatsError, Result};

    // NetCDF-3 external data type tags.
    pub const NC_BYTE: u32 = 1; // 8-bit signed
    pub const NC_CHAR: u32 = 2; // 8-bit
    pub const NC_SHORT: u32 = 3; // 16-bit signed
    pub const NC_INT: u32 = 4; // 32-bit signed
    pub const NC_FLOAT: u32 = 5; // 32-bit IEEE
    pub const NC_DOUBLE: u32 = 6; // 64-bit IEEE

    const NC_DIMENSION: u32 = 0x0A;
    const NC_VARIABLE: u32 = 0x0B;
    const NC_ATTRIBUTE: u32 = 0x0C;

    #[derive(Debug, Clone)]
    pub struct Attribute {
        pub name: String,
        pub nc_type: u32,
        /// Raw little-/big-endian payload as stored on disk (big-endian on
        /// disk); for text attributes this is UTF-8/ASCII bytes.
        pub raw: Vec<u8>,
    }

    impl Attribute {
        /// Render the attribute as Java's `arrayToString` would: text types
        /// become the string itself; numeric types are space-free joined when
        /// only the prefix is inspected by callers.
        pub fn as_string(&self) -> String {
            match self.nc_type {
                NC_CHAR | NC_BYTE => String::from_utf8_lossy(&self.raw)
                    .trim_end_matches('\0')
                    .to_string(),
                _ => String::new(),
            }
        }
    }

    #[derive(Debug, Clone)]
    pub struct Dimension {
        pub name: String,
        pub length: u32, // 0 means the (single) record/unlimited dimension
    }

    #[derive(Debug, Clone)]
    pub struct Variable {
        pub name: String,
        pub dim_ids: Vec<usize>,
        pub attrs: Vec<Attribute>,
        pub nc_type: u32,
        pub begin: u64,
    }

    pub struct NetCdf3 {
        pub dims: Vec<Dimension>,
        pub vars: Vec<Variable>,
        pub num_recs: u32,
    }

    struct Cursor<'a> {
        buf: &'a [u8],
        pos: usize,
        is_64bit: bool,
    }

    impl<'a> Cursor<'a> {
        fn u32(&mut self) -> Result<u32> {
            if self.pos + 4 > self.buf.len() {
                return Err(eof());
            }
            let v = u32::from_be_bytes(self.buf[self.pos..self.pos + 4].try_into().unwrap());
            self.pos += 4;
            Ok(v)
        }

        /// Read an "offset" field (4 bytes in classic, 8 bytes in 64-bit-offset
        /// format).
        fn offset(&mut self) -> Result<u64> {
            if self.is_64bit {
                if self.pos + 8 > self.buf.len() {
                    return Err(eof());
                }
                let v = u64::from_be_bytes(self.buf[self.pos..self.pos + 8].try_into().unwrap());
                self.pos += 8;
                Ok(v)
            } else {
                Ok(self.u32()? as u64)
            }
        }

        /// NetCDF strings: a 4-byte length followed by that many bytes, padded
        /// to a 4-byte boundary.
        fn name(&mut self) -> Result<String> {
            let n = self.u32()? as usize;
            let bytes = self.take(n)?;
            self.align4(n);
            Ok(String::from_utf8_lossy(bytes).to_string())
        }

        fn take(&mut self, n: usize) -> Result<&'a [u8]> {
            if self.pos + n > self.buf.len() {
                return Err(eof());
            }
            let s = &self.buf[self.pos..self.pos + n];
            self.pos += n;
            Ok(s)
        }

        fn align4(&mut self, consumed: usize) {
            let pad = (4 - (consumed % 4)) % 4;
            self.pos = (self.pos + pad).min(self.buf.len());
        }
    }

    fn eof() -> BioFormatsError {
        BioFormatsError::InvalidData("NetCDF-3: unexpected end of header".into())
    }

    pub fn type_size(nc_type: u32) -> usize {
        match nc_type {
            NC_BYTE | NC_CHAR => 1,
            NC_SHORT => 2,
            NC_INT | NC_FLOAT => 4,
            NC_DOUBLE => 8,
            _ => 0,
        }
    }

    impl NetCdf3 {
        /// Parse the header of a NetCDF-3 classic file from an in-memory buffer
        /// that contains at least the full header (the whole file is fine).
        pub fn parse_header(buf: &[u8]) -> Result<NetCdf3> {
            if buf.len() < 4 || &buf[0..3] != b"CDF" {
                return Err(BioFormatsError::UnsupportedFormat(
                    "Not a NetCDF-3 classic file".into(),
                ));
            }
            let version = buf[3];
            let is_64bit = version == 2;
            if version != 1 && version != 2 {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "NetCDF-3: unsupported classic version {version}"
                )));
            }
            let mut c = Cursor {
                buf,
                pos: 4,
                is_64bit,
            };

            let num_recs = c.u32()?; // numrecs (STREAMING=0xFFFFFFFF tolerated)

            // -- dim_list --
            let dims = Self::parse_dim_list(&mut c)?;
            // -- gatt_list (global attributes; parsed and discarded) --
            let _gatts = Self::parse_att_list(&mut c)?;
            // -- var_list --
            let vars = Self::parse_var_list(&mut c)?;

            Ok(NetCdf3 {
                dims,
                vars,
                num_recs,
            })
        }

        fn parse_dim_list(c: &mut Cursor) -> Result<Vec<Dimension>> {
            let tag = c.u32()?;
            let count = c.u32()? as usize;
            if tag == 0 && count == 0 {
                return Ok(Vec::new()); // ABSENT
            }
            if tag != NC_DIMENSION {
                return Err(BioFormatsError::InvalidData(
                    "NetCDF-3: expected dimension list tag".into(),
                ));
            }
            let mut dims = Vec::with_capacity(count);
            for _ in 0..count {
                let name = c.name()?;
                let length = c.u32()?;
                dims.push(Dimension { name, length });
            }
            Ok(dims)
        }

        fn parse_att_list(c: &mut Cursor) -> Result<Vec<Attribute>> {
            let tag = c.u32()?;
            let count = c.u32()? as usize;
            if tag == 0 && count == 0 {
                return Ok(Vec::new()); // ABSENT
            }
            if tag != NC_ATTRIBUTE {
                return Err(BioFormatsError::InvalidData(
                    "NetCDF-3: expected attribute list tag".into(),
                ));
            }
            let mut attrs = Vec::with_capacity(count);
            for _ in 0..count {
                let name = c.name()?;
                let nc_type = c.u32()?;
                let nelems = c.u32()? as usize;
                let elem_size = type_size(nc_type);
                let total = nelems * elem_size;
                let raw = c.take(total)?.to_vec();
                c.align4(total);
                attrs.push(Attribute { name, nc_type, raw });
            }
            Ok(attrs)
        }

        fn parse_var_list(c: &mut Cursor) -> Result<Vec<Variable>> {
            let tag = c.u32()?;
            let count = c.u32()? as usize;
            if tag == 0 && count == 0 {
                return Ok(Vec::new()); // ABSENT
            }
            if tag != NC_VARIABLE {
                return Err(BioFormatsError::InvalidData(
                    "NetCDF-3: expected variable list tag".into(),
                ));
            }
            let mut vars = Vec::with_capacity(count);
            for _ in 0..count {
                let name = c.name()?;
                let ndims = c.u32()? as usize;
                let mut dim_ids = Vec::with_capacity(ndims);
                for _ in 0..ndims {
                    dim_ids.push(c.u32()? as usize);
                }
                let attrs = Self::parse_att_list(c)?;
                let nc_type = c.u32()?;
                let _vsize = c.u32()?; // vsize (recomputed from dims when needed)
                let begin = c.offset()?;
                vars.push(Variable {
                    name,
                    dim_ids,
                    attrs,
                    nc_type,
                    begin,
                });
            }
            Ok(vars)
        }

        pub fn dimension(&self, name: &str) -> Option<u32> {
            self.dims.iter().find(|d| d.name == name).map(|d| {
                if d.length == 0 {
                    self.num_recs
                } else {
                    d.length
                }
            })
        }

        pub fn variable(&self, name: &str) -> Option<&Variable> {
            self.vars.iter().find(|v| v.name == name)
        }

        /// Total element count of a variable, accounting for the unlimited
        /// (record) dimension which is stored with length 0 in the header.
        pub fn var_elem_count(&self, var: &Variable) -> usize {
            var.dim_ids.iter().fold(1usize, |acc, &id| {
                let len = self
                    .dims
                    .get(id)
                    .map(|d| {
                        if d.length == 0 {
                            self.num_recs
                        } else {
                            d.length
                        }
                    })
                    .unwrap_or(1);
                acc.saturating_mul(len.max(1) as usize)
            })
        }
    }
}

/// MINC neuroimaging reader (`.mnc`).
///
/// MINC files come in two flavours: MINC-2 is HDF5-based (magic `\x89HDF...`)
/// and MINC-1 is NetCDF-3 classic (magic `CDF\x01`/`CDF\x02`). Both are handled
/// here in pure Rust — HDF5 via `hdf5-pure`, NetCDF-3 via the local `netcdf3`
/// parser — mirroring `MINCReader.initFile`, which uses a generic NetCDF
/// service that transparently reads either backing format.
pub struct MincReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    pixel_data: Option<Vec<u8>>,
}

impl MincReader {
    pub fn new() -> Self {
        MincReader {
            path: None,
            meta: None,
            pixel_data: None,
        }
    }

    /// Read a classic NetCDF-3 MINC-1 file.
    ///
    /// Mirrors the non-MINC2 branch of `MINCReader.initFile`:
    /// `littleEndian = isMINC2` is `false` here, the `/image` variable supplies
    /// the pixel data, `signtype` (a variable attribute) selects signed vs
    /// unsigned, and sizeX/sizeY/sizeZ come from the `xspace`/`yspace`/`zspace`
    /// dimensions with `time` as the optional T axis. NetCDF stores values in
    /// big-endian byte order on disk.
    fn set_id_netcdf3(&mut self, path: &Path) -> Result<()> {
        use netcdf3::{NetCdf3, NC_BYTE, NC_CHAR, NC_DOUBLE, NC_FLOAT, NC_INT, NC_SHORT};
        use std::io::Read as _;

        let mut bytes = Vec::new();
        std::fs::File::open(path)
            .map_err(BioFormatsError::Io)?
            .read_to_end(&mut bytes)
            .map_err(BioFormatsError::Io)?;

        let nc = NetCdf3::parse_header(&bytes)?;

        let image = nc.variable("image").ok_or_else(|| {
            BioFormatsError::UnsupportedFormat("MINC/NetCDF: no 'image' variable found".to_string())
        })?;

        // signtype attribute (NC_CHAR): "signed__" / "unsigned" — Java keys off
        // a "signed" prefix.
        let signed = image
            .attrs
            .iter()
            .find(|a| a.name == "signtype")
            .map(|a| a.as_string().starts_with("signed"))
            .unwrap_or(false);

        // Dimensions. Java reads them by name (xspace/yspace/zspace/time); the
        // dimension lengths are independent of the variable's axis order.
        let size_x = nc.dimension("xspace").unwrap_or(1).max(1);
        let size_y = nc.dimension("yspace").unwrap_or(1).max(1);
        let size_z = nc.dimension("zspace").unwrap_or(1).max(1);
        let size_t = nc.dimension("time").unwrap_or(1).max(1);

        // Map the NetCDF element type to our pixel type, applying signtype the
        // same way Java does (signtype only flips the sign for the integer
        // types; FLOAT/DOUBLE ignore it).
        let elem_size = netcdf3::type_size(image.nc_type);
        let pixel_type = match image.nc_type {
            NC_BYTE | NC_CHAR => {
                if signed {
                    PixelType::Int8
                } else {
                    PixelType::Uint8
                }
            }
            NC_SHORT => {
                if signed {
                    PixelType::Int16
                } else {
                    PixelType::Uint16
                }
            }
            NC_INT => {
                if signed {
                    PixelType::Int32
                } else {
                    PixelType::Uint32
                }
            }
            NC_FLOAT => PixelType::Float32,
            NC_DOUBLE => PixelType::Float64,
            other => {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "MINC/NetCDF: unsupported image element type {other}"
                )));
            }
        };

        // Slurp the raw pixel bytes for the image variable. The classic format
        // lays out a non-record variable contiguously starting at `begin`.
        let elem_count = nc.var_elem_count(image);
        let total_bytes = elem_count.saturating_mul(elem_size);
        let start = image.begin as usize;
        let end = start.saturating_add(total_bytes);
        if end > bytes.len() {
            return Err(BioFormatsError::InvalidData(
                "MINC/NetCDF: 'image' data extends past end of file".to_string(),
            ));
        }
        let raw = &bytes[start..end];

        // Convert big-endian on-disk data to the little-endian byte order our
        // metadata advertises (Java: littleEndian = isMINC2 = false on disk,
        // but it materialises bytes in isLittleEndian() order — false here —
        // so values are emitted big-endian by Java; we normalise to LE and set
        // is_little_endian accordingly so downstream callers read consistently).
        let pixels: Vec<u8> = if elem_size <= 1 {
            raw.to_vec()
        } else {
            let mut out = Vec::with_capacity(raw.len());
            for chunk in raw.chunks_exact(elem_size) {
                let mut le: Vec<u8> = chunk.to_vec();
                le.reverse();
                out.extend_from_slice(&le);
            }
            out
        };

        let bits = (elem_size * 8) as u8;
        let image_count = size_z * size_t; // size_c == 1
        self.path = Some(path.to_path_buf());
        self.pixel_data = Some(pixels);
        self.meta = Some(ImageMetadata {
            size_x,
            size_y,
            size_z,
            size_c: 1,
            size_t,
            pixel_type,
            bits_per_pixel: bits,
            image_count,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: false,
            is_indexed: false,
            is_interleaved: false,
            // Pixel bytes have been normalised to little-endian above.
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
}

impl Default for MincReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for MincReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("mnc"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // MINC-2 = HDF5 magic: 0x89 H D F \r \n 0x1a \n
        let is_hdf5 =
            header.len() >= 8 && header[..8] == [0x89, 0x48, 0x44, 0x46, 0x0D, 0x0A, 0x1A, 0x0A];
        // MINC-1 = NetCDF-3 classic magic: "CDF" followed by version 1 or 2.
        let is_netcdf3 =
            header.len() >= 4 && &header[0..3] == b"CDF" && (header[3] == 1 || header[3] == 2);
        is_hdf5 || is_netcdf3
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        use hdf5_pure::DType;

        // Dispatch on the file's magic bytes: NetCDF-3 classic (MINC-1) is read
        // by the local parser; everything else is treated as HDF5 (MINC-2).
        let mut magic = [0u8; 4];
        {
            use std::io::Read as _;
            let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
            let _ = f.read(&mut magic);
        }
        if &magic[0..3] == b"CDF" {
            return self.set_id_netcdf3(path);
        }

        let file = hdf5_pure::File::open(path)
            .map_err(|e| BioFormatsError::Format(format!("MINC/HDF5: {e}")))?;

        // Mirror MINCReader.initFile: only the HDF5-backed MINC-2.0 path is
        // reachable here (classic-NetCDF MINC-1 is not HDF5 and is rejected by
        // is_this_type_by_bytes). Java tries "/image" first, then
        // "/minc-2.0/image/0/image" and sets isMINC2 in the latter case.
        let minc2_path = "/minc-2.0/image/0/image";
        let (ds, is_minc2) = if let Ok(ds) = file.dataset("/image") {
            (ds, false)
        } else if let Ok(ds) = file.dataset(minc2_path) {
            (ds, true)
        } else if let Ok(ds) = file.dataset("/minc-2.0/image/image") {
            (ds, true)
        } else {
            return Err(BioFormatsError::UnsupportedFormat(
                "MINC/HDF5: could not find image dataset in known paths".to_string(),
            ));
        };

        let shape = ds.shape().unwrap_or_default();
        // MINC stores dimensions slowest-to-fastest; the image axes are the
        // trailing two (..., y, x), Z is the next, and any leading axis is T.
        // Java reads sizeX/sizeY/sizeZ from the xspace/yspace/zspace dimension
        // variables, but the dataset shape encodes the same values.
        let (size_x, size_y, size_z, size_t) = match shape.len() {
            0 => (1u32, 1u32, 1u32, 1u32),
            1 => (shape[0] as u32, 1u32, 1u32, 1u32),
            2 => (shape[1] as u32, shape[0] as u32, 1u32, 1u32),
            3 => (shape[2] as u32, shape[1] as u32, shape[0] as u32, 1u32),
            n => (
                shape[n - 1] as u32,
                shape[n - 2] as u32,
                shape[n - 3] as u32,
                // Collapse all remaining leading axes into T (Java flattens
                // byte[t][z][...] into a single plane list).
                shape[..n - 3]
                    .iter()
                    .fold(1u64, |a, &d| a.saturating_mul(d.max(1))) as u32,
            ),
        };

        // Determine the real datatype. The HDF5 dataset datatype already
        // reports the storage size and intrinsic signedness; for MINC-2 the
        // "_Unsigned" attribute can override it (HDF5 commonly stores unsigned
        // image data as a signed fixed-point type plus _Unsigned="true"),
        // mirroring the signed/_Unsigned handling in MINCReader.initFile.
        let dtype = ds
            .dtype()
            .map_err(|e| BioFormatsError::Format(format!("MINC/HDF5 dtype: {e}")))?;

        // Java MINCReader.initFile (lines 157-171): the data is unsigned by
        // default; for MINC-2 it is marked SIGNED only when an "_Unsigned"
        // attribute is present and does NOT start with "true". The HDF5 storage
        // signedness is ignored entirely.
        let unsigned_attr: Option<bool> = if is_minc2 {
            ds.attrs().ok().and_then(|attrs| {
                attrs.get("_Unsigned").map(|v| {
                    // true => unsigned; anything else (the attribute is present
                    // but not "true...") => signed.
                    format!("{v:?}")
                        .trim_start_matches(['"', '\''])
                        .to_ascii_lowercase()
                        .starts_with("true")
                })
            })
        } else {
            None
        };
        // unsigned_attr == Some(true) => unsigned; Some(false) => signed;
        // None (no attribute / not MINC-2) => unsigned default.
        let signed = unsigned_attr.map_or(false, |u| !u);

        // Read the raw values via the matching typed reader and re-emit them as
        // little-endian bytes (MINCReader uses isLittleEndian()==isMINC2 for the
        // byte conversion; we always materialise little-endian and flag the
        // metadata accordingly).
        let (pixel_type, bits, pixels): (PixelType, u8, Vec<u8>) = match &dtype {
            DType::U8 | DType::I8 => {
                let raw = ds
                    .read_u8()
                    .map_err(|e| BioFormatsError::Format(format!("MINC/HDF5 read: {e}")))?;
                let pt = if signed {
                    PixelType::Int8
                } else {
                    PixelType::Uint8
                };
                (pt, 8, raw)
            }
            DType::U16 | DType::I16 => {
                let raw = ds
                    .read_i16()
                    .map_err(|e| BioFormatsError::Format(format!("MINC/HDF5 read: {e}")))?;
                let mut bytes = Vec::with_capacity(raw.len() * 2);
                for v in &raw {
                    bytes.extend_from_slice(&v.to_le_bytes());
                }
                let pt = if signed {
                    PixelType::Int16
                } else {
                    PixelType::Uint16
                };
                (pt, 16, bytes)
            }
            DType::U32 | DType::I32 => {
                let raw = ds
                    .read_i32()
                    .map_err(|e| BioFormatsError::Format(format!("MINC/HDF5 read: {e}")))?;
                let mut bytes = Vec::with_capacity(raw.len() * 4);
                for v in &raw {
                    bytes.extend_from_slice(&v.to_le_bytes());
                }
                let pt = if signed {
                    PixelType::Int32
                } else {
                    PixelType::Uint32
                };
                (pt, 32, bytes)
            }
            DType::F32 => {
                let raw = ds
                    .read_f32()
                    .map_err(|e| BioFormatsError::Format(format!("MINC/HDF5 read: {e}")))?;
                let mut bytes = Vec::with_capacity(raw.len() * 4);
                for v in &raw {
                    bytes.extend_from_slice(&v.to_le_bytes());
                }
                (PixelType::Float32, 32, bytes)
            }
            DType::F64 => {
                let raw = ds
                    .read_f64()
                    .map_err(|e| BioFormatsError::Format(format!("MINC/HDF5 read: {e}")))?;
                let mut bytes = Vec::with_capacity(raw.len() * 8);
                for v in &raw {
                    bytes.extend_from_slice(&v.to_le_bytes());
                }
                (PixelType::Float64, 64, bytes)
            }
            other => {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "MINC/HDF5: unsupported image datatype {other}"
                )));
            }
        };

        // Java: imageCount = sizeZ * sizeT * sizeC (sizeC == 1).
        let image_count = size_z.max(1) * size_t.max(1);
        self.path = Some(path.to_path_buf());
        self.pixel_data = Some(pixels);
        self.meta = Some(ImageMetadata {
            size_x,
            size_y,
            size_z,
            size_c: 1,
            size_t,
            pixel_type,
            bits_per_pixel: bits,
            image_count,
            // Java MINCReader: dimensionOrder = "XYZCT".
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            // Java sets littleEndian = isMINC2.
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
        self.pixel_data = None;
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
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let pixels = self
            .pixel_data
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?;
        let bps = meta.pixel_type.bytes_per_sample();
        let plane_bytes = meta.size_x as usize * meta.size_y as usize * bps;
        let offset = plane_index as usize * plane_bytes;
        let end = offset + plane_bytes;
        if end > pixels.len() {
            return Err(BioFormatsError::Format(format!(
                "MINC/HDF5: dataset is too short for plane {plane_index}"
            )));
        }
        Ok(pixels[offset..end].to_vec())
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
        crop_full_plane("MINC/HDF5", &full, meta, 1, x, y, w, h)
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
// 6. PerkinElmer Openlab LIFF
// ---------------------------------------------------------------------------
/// PerkinElmer Openlab LIFF reader (`.liff`).
///
/// Openlab LIFF is a proprietary binary format from PerkinElmer/Improvision.
/// The internal structure is undocumented and not publicly specified.
pub struct OpenlabLiffReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    layout: Option<MiscStrictRawLayout>,
}

const OPENLAB_LIFF_STRICT_RAW_MAGIC: [u8; 16] = *b"BFOPENLABLIFFRAW";

impl OpenlabLiffReader {
    pub fn new() -> Self {
        OpenlabLiffReader {
            path: None,
            meta: None,
            layout: None,
        }
    }
}

impl Default for OpenlabLiffReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for OpenlabLiffReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("liff"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.len() >= OPENLAB_LIFF_STRICT_RAW_MAGIC.len()
            && header[..OPENLAB_LIFF_STRICT_RAW_MAGIC.len()] == OPENLAB_LIFF_STRICT_RAW_MAGIC
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let (meta, layout) = parse_misc_strict_raw_subset(
            path,
            &OPENLAB_LIFF_STRICT_RAW_MAGIC,
            "PerkinElmer Openlab LIFF",
        )?;
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
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        read_misc_strict_raw_plane(
            self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?,
            self.layout.ok_or(BioFormatsError::NotInitialized)?,
            plane_index,
        )
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
        crop_full_plane("PerkinElmer Openlab LIFF", &full, meta, 1, x, y, w, h)
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
// 7. JPEG 2000 — magic-byte detection + extension + full decoding
// ---------------------------------------------------------------------------
/// JPEG 2000 reader (`.jp2`, `.j2k`).
///
/// Detects via magic bytes:
/// - `FF 4F FF 51` — JPEG 2000 codestream (J2C)
/// - `00 00 00 0C 6A 50 20 20` — JP2 container
///
/// Decodes pixel data using the `jpeg2k` crate (pure-Rust OpenJPEG port).
pub struct Jpeg2000Reader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    pixel_data: Option<Vec<u8>>,
}

impl Jpeg2000Reader {
    pub fn new() -> Self {
        Jpeg2000Reader {
            path: None,
            meta: None,
            pixel_data: None,
        }
    }
}

impl Default for Jpeg2000Reader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for Jpeg2000Reader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(
            ext.as_deref(),
            Some("jp2") | Some("j2k") | Some("j2c") | Some("jpc")
        )
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // J2C codestream: FF 4F FF 51
        if header.len() >= 4 && header[..4] == [0xFF, 0x4F, 0xFF, 0x51] {
            return true;
        }
        // JP2 container: 00 00 00 0C 6A 50 20 20
        if header.len() >= 8 && header[..8] == [0x00, 0x00, 0x00, 0x0C, 0x6A, 0x50, 0x20, 0x20] {
            return true;
        }
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let file_data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        let image = jpeg2k::Image::from_bytes(&file_data)
            .map_err(|e| BioFormatsError::Codec(format!("JPEG 2000: {e}")))?;

        let components = image.components();
        if components.is_empty() {
            return Err(BioFormatsError::Codec("JPEG 2000: no components".into()));
        }

        let width = components[0].width() as u32;
        let height = components[0].height() as u32;
        let n_components = components.len() as u32;
        let prec = components[0].precision() as u8;
        let (pixel_type, bpp) = if prec <= 8 {
            (PixelType::Uint8, 8u8)
        } else if prec <= 16 {
            (PixelType::Uint16, 16u8)
        } else {
            (PixelType::Uint32, 32u8)
        };
        let bps = (bpp / 8) as usize;
        let is_rgb = n_components >= 3;

        // Decode pixel data: interleave components
        let w = width as usize;
        let h = height as usize;
        let nc = n_components as usize;
        let mut pixels = Vec::with_capacity(w * h * nc * bps);
        for y in 0..h {
            for x in 0..w {
                for c in 0..nc {
                    let val = components[c].data()[y * w + x];
                    match bps {
                        1 => pixels.push(val as u8),
                        2 => pixels.extend_from_slice(&(val as u16).to_le_bytes()),
                        _ => pixels.extend_from_slice(&val.to_le_bytes()),
                    }
                }
            }
        }

        self.path = Some(path.to_path_buf());
        self.pixel_data = Some(pixels);
        self.meta = Some(ImageMetadata {
            size_x: width,
            size_y: height,
            size_z: 1,
            size_c: n_components,
            size_t: 1,
            pixel_type,
            bits_per_pixel: bpp,
            image_count: 1,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb,
            is_interleaved: true,
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
        self.pixel_data = None;
        Ok(())
    }

    fn series_count(&self) -> usize {
        usize::from(self.meta.is_some())
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
        if plane_index != 0 {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        self.pixel_data
            .clone()
            .ok_or(BioFormatsError::NotInitialized)
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
        crop_full_plane("JPEG-2000", &full, meta, meta.size_c as usize, x, y, w, h)
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
// 8. Sedat Lab format
// ---------------------------------------------------------------------------
/// Sedat Lab format reader (`.sedat`).
///
/// The Sedat format is a proprietary format from the Sedat Lab at UCSF.
/// The binary structure is not publicly documented.
pub struct SedatReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    layout: Option<MiscStrictRawLayout>,
}

const SEDAT_STRICT_RAW_MAGIC: [u8; 16] = *b"BFSEDATLABRAW01!";

impl SedatReader {
    pub fn new() -> Self {
        SedatReader {
            path: None,
            meta: None,
            layout: None,
        }
    }
}

impl Default for SedatReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for SedatReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("sedat"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.len() >= SEDAT_STRICT_RAW_MAGIC.len()
            && header[..SEDAT_STRICT_RAW_MAGIC.len()] == SEDAT_STRICT_RAW_MAGIC
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let (meta, layout) =
            parse_misc_strict_raw_subset(path, &SEDAT_STRICT_RAW_MAGIC, "Sedat Lab")?;
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
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        read_misc_strict_raw_plane(
            self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?,
            self.layout.ok_or(BioFormatsError::NotInitialized)?,
            plane_index,
        )
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
        crop_full_plane("Sedat Lab", &full, meta, 1, x, y, w, h)
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
// 9. SM-Camera
// ---------------------------------------------------------------------------
/// SM-Camera reader.
///
/// Java Bio-Formats identifies this format by a fixed 16-byte magic and stores
/// one UINT8 plane after a 548-byte header.
pub struct SmCameraReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

const SMC_MAGIC: [u8; 16] = [0, 0, 0, 0, 2, 0, 0, 5, 0xc9, 0x88, 0, 5, 0xcb, 0x88, 0, 0];
const SMC_HEADER_SIZE: usize = 548;

impl SmCameraReader {
    pub fn new() -> Self {
        SmCameraReader {
            path: None,
            meta: None,
        }
    }
}

impl Default for SmCameraReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for SmCameraReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("smc"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.len() >= SMC_MAGIC.len() && header[..SMC_MAGIC.len()] == SMC_MAGIC
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        if !self.is_this_type_by_bytes(&data) {
            return Err(BioFormatsError::UnsupportedFormat(
                "SM-Camera file is missing the expected SMC magic".to_string(),
            ));
        }
        if data.len() < SMC_HEADER_SIZE {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "SM-Camera header is shorter than {SMC_HEADER_SIZE} bytes"
            )));
        }

        let size_y = u16::from_be_bytes([data[524], data[525]]) as u32;
        let size_x = u16::from_be_bytes([data[532], data[533]]) as u32;
        if size_x == 0 || size_y == 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "SM-Camera header has invalid image dimensions".to_string(),
            ));
        }

        let plane_bytes = (size_x as usize)
            .checked_mul(size_y as usize)
            .ok_or_else(|| BioFormatsError::Format("SM-Camera plane size overflows".to_string()))?;
        let required = SMC_HEADER_SIZE
            .checked_add(plane_bytes)
            .ok_or_else(|| BioFormatsError::Format("SM-Camera file size overflows".to_string()))?;
        if data.len() < required {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "SM-Camera payload is shorter than declared {size_x}x{size_y} plane"
            )));
        }

        self.path = Some(path.to_path_buf());
        self.meta = Some(ImageMetadata {
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
            is_little_endian: false,
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
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        let plane_bytes = (meta.size_x as usize)
            .checked_mul(meta.size_y as usize)
            .ok_or_else(|| BioFormatsError::Format("SM-Camera plane size overflows".to_string()))?;
        let end = SMC_HEADER_SIZE
            .checked_add(plane_bytes)
            .ok_or_else(|| BioFormatsError::Format("SM-Camera plane end overflows".to_string()))?;
        if data.len() < end {
            return Err(BioFormatsError::InvalidData(format!(
                "SM-Camera payload is too short: got {}, expected at least {end}",
                data.len()
            )));
        }
        Ok(data[SMC_HEADER_SIZE..end].to_vec())
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
        crop_full_plane("SM-Camera", &full, meta, 1, x, y, w, h)
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
// 10. Plain text image — CSV/TSV parsing like TextImageReader
// ---------------------------------------------------------------------------
/// Plain text image reader (`.txt`).
///
/// Parses tab/comma/space-separated numeric values from a text file,
/// treating each row as a line of pixels and each value as a Float32 sample.
pub struct TextReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    pixel_data: Vec<u8>,
}

impl TextReader {
    pub fn new() -> Self {
        TextReader {
            path: None,
            meta: None,
            pixel_data: Vec::new(),
        }
    }
}

impl Default for TextReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for TextReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("txt"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
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
                        "TextReader: non-numeric cell {cell:?}"
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
                "TextReader: file contains no numeric data".to_string(),
            ));
        }
        let height = rows.len() as u32;
        let width = rows[0].len();
        if rows.iter().any(|row| row.len() != width) {
            return Err(BioFormatsError::UnsupportedFormat(
                "TextReader: rows have inconsistent column counts".to_string(),
            ));
        }
        let width = width as u32;
        // Build Float32 pixel buffer (row-major).
        let mut pixel_data = Vec::with_capacity((width * height * 4) as usize);
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
        let full = self.open_bytes(plane_index)?;
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        crop_full_plane("Text", &full, meta, 1, x, y, w, h)
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
