//! Placeholder readers for miscellaneous / proprietary formats.
//!
//! Extension-only placeholder readers return `UnsupportedFormat` instead of
//! exposing synthetic metadata or zero-filled planes. Partial readers in this
//! module only decode documented/simple payload cases.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;

// ---------------------------------------------------------------------------
// Shared byte cursor (mirrors loci.common.RandomAccessInputStream)
// ---------------------------------------------------------------------------
/// In-memory, endian-aware byte cursor used by the faithful ports of the
/// Improvision Openlab LIFF, MNG and 3i SlideBook readers. It mirrors the
/// subset of `RandomAccessInputStream` those Java readers rely on: seeking,
/// signed/unsigned multi-byte reads, fixed-length and NUL-terminated strings,
/// and an endianness flag that can be toggled mid-stream (`in.order(...)`).
///
/// Reads past the end of the buffer return zero-padded values and advance the
/// position, matching the way the Java readers tolerate over-reads while their
/// `while (fp < length)` loops terminate.
struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
    little: bool,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8], little: bool) -> Self {
        Cursor {
            data,
            pos: 0,
            little,
        }
    }

    fn fp(&self) -> usize {
        self.pos
    }

    fn seek(&mut self, p: usize) {
        self.pos = p;
    }

    /// Set the byte order (`in.order(little)`).
    fn order(&mut self, little: bool) {
        self.little = little;
    }

    fn skip(&mut self, n: i64) {
        if n >= 0 {
            self.pos = self.pos.saturating_add(n as usize);
        } else {
            self.pos = self.pos.saturating_sub((-n) as usize);
        }
    }

    fn read_bytes(&mut self, n: usize) -> Vec<u8> {
        let start = self.pos.min(self.data.len());
        let end = self.pos.saturating_add(n).min(self.data.len());
        let mut out = vec![0u8; n];
        let avail = end.saturating_sub(start);
        out[..avail].copy_from_slice(&self.data[start..end]);
        self.pos = self.pos.saturating_add(n);
        out
    }

    /// Java `in.read()`: next byte 0-255, or -1 at end of stream.
    fn read(&mut self) -> i32 {
        if self.pos < self.data.len() {
            let v = self.data[self.pos] as i32;
            self.pos += 1;
            v
        } else {
            self.pos += 1;
            -1
        }
    }

    fn read_u16(&mut self) -> u16 {
        let b = self.read_bytes(2);
        let a = [b[0], b[1]];
        if self.little {
            u16::from_le_bytes(a)
        } else {
            u16::from_be_bytes(a)
        }
    }

    fn read_short(&mut self) -> i16 {
        self.read_u16() as i16
    }

    fn read_u32(&mut self) -> u32 {
        let b = self.read_bytes(4);
        let a = [b[0], b[1], b[2], b[3]];
        if self.little {
            u32::from_le_bytes(a)
        } else {
            u32::from_be_bytes(a)
        }
    }

    fn read_int(&mut self) -> i32 {
        self.read_u32() as i32
    }

    fn read_u64(&mut self) -> u64 {
        let b = self.read_bytes(8);
        let a = [b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]];
        if self.little {
            u64::from_le_bytes(a)
        } else {
            u64::from_be_bytes(a)
        }
    }

    fn read_long(&mut self) -> i64 {
        self.read_u64() as i64
    }

    fn read_float(&mut self) -> f32 {
        f32::from_bits(self.read_u32())
    }

    /// Read exactly `n` bytes and interpret them as a (lossy UTF-8) string.
    fn read_string(&mut self, n: usize) -> String {
        let b = self.read_bytes(n);
        String::from_utf8_lossy(&b).into_owned()
    }

    /// Read up to a NUL terminator, consuming the terminator (Java
    /// `findString(true, .., "\0")`). Stops at end of stream.
    fn read_cstring(&mut self) -> String {
        let start = self.pos.min(self.data.len());
        let mut end = start;
        while end < self.data.len() && self.data[end] != 0 {
            end += 1;
        }
        let s = String::from_utf8_lossy(&self.data[start..end]).into_owned();
        // Consume up to and including the terminator (or to EOF).
        self.pos = if end < self.data.len() {
            end + 1
        } else {
            self.data.len()
        };
        s
    }
}

/// Crop a region out of a channel-separated (planar) plane buffer. Used by the
/// MNG / Openlab ports, whose `ImageMetadata` advertises `is_interleaved =
/// false` so each channel is stored as a contiguous sub-plane.
fn crop_planar(
    format_name: &str,
    full: &[u8],
    meta: &ImageMetadata,
    channels: usize,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
) -> Result<Vec<u8>> {
    use crate::common::region::validate_region;
    validate_region(format_name, meta.size_x, meta.size_y, x, y, w, h)?;
    let bps = meta.pixel_type.bytes_per_sample();
    let row = (meta.size_x as usize) * bps;
    let plane = row * (meta.size_y as usize);
    let out_row = (w as usize) * bps;
    if full.len() < plane * channels {
        return Err(BioFormatsError::InvalidData(format!(
            "{format_name} plane buffer is too short: got {}, expected {}",
            full.len(),
            plane * channels
        )));
    }
    let mut out = Vec::with_capacity(out_row * (h as usize) * channels);
    for c in 0..channels {
        let base = c * plane;
        for r in 0..h as usize {
            let src = base + (y as usize + r) * row + (x as usize) * bps;
            out.extend_from_slice(&full[src..src + out_row]);
        }
    }
    Ok(out)
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
    series: Vec<QuickTimeParsed>,
    current_series: usize,
}

impl QuickTimeReader {
    pub fn new() -> Self {
        QuickTimeReader {
            path: None,
            series: Vec::new(),
            current_series: 0,
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
        self.series = parsed;
        self.current_series = 0;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.series.clear();
        self.current_series = 0;
        Ok(())
    }

    fn series_count(&self) -> usize {
        self.series.len()
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.series.is_empty() {
            Err(BioFormatsError::NotInitialized)
        } else if s < self.series.len() {
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
        self.series
            .get(self.current_series)
            .map(|series| &series.meta)
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let series = self
            .series
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        let meta = &series.meta;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let index = plane_index as usize;
        let offset = series.sample_offsets[index];
        let sample_size = series.sample_sizes[index] as usize;
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
        let sample = &data[start..end];
        match series.codec {
            QuickTimeCodec::UncompressedRgb | QuickTimeCodec::UncompressedGray => {
                let expected = meta
                    .size_x
                    .checked_mul(meta.size_y)
                    .and_then(|px| (px as usize).checked_mul(series.samples_per_pixel))
                    .and_then(|samples| samples.checked_mul(meta.pixel_type.bytes_per_sample()))
                    .ok_or_else(|| {
                        BioFormatsError::Format("QuickTime plane size overflows".into())
                    })?;
                if sample_size != expected {
                    return Err(BioFormatsError::UnsupportedFormat(format!(
                        "QuickTime sample {plane_index} has {sample_size} bytes, expected {expected} for uncompressed pixels"
                    )));
                }
                Ok(sample.to_vec())
            }
            QuickTimeCodec::Jpeg => decode_quicktime_jpeg_sample(sample, meta, plane_index),
            QuickTimeCodec::Png => decode_quicktime_png_sample(sample, meta, plane_index),
            QuickTimeCodec::Cinepak { depth } => {
                let mut previous = None;
                for current in 0..=index {
                    let offset = series.sample_offsets[current] as usize;
                    let end = offset
                        .checked_add(series.sample_sizes[current] as usize)
                        .ok_or_else(|| {
                            BioFormatsError::Format("QuickTime sample offset overflows".into())
                        })?;
                    if end > data.len() {
                        return Err(BioFormatsError::UnsupportedFormat(format!(
                            "QuickTime sample {current} extends past end of file"
                        )));
                    }
                    previous = Some(decode_quicktime_cinepak_sample(
                        &data[offset..end],
                        meta,
                        current as u32,
                        depth,
                        previous.as_deref(),
                    )?);
                }
                previous.ok_or(BioFormatsError::PlaneOutOfRange(plane_index))
            }
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
        let full = self.open_bytes(plane_index)?;
        let series = self
            .series
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        crop_full_plane(
            "QuickTime",
            &full,
            &series.meta,
            series.samples_per_pixel,
            x,
            y,
            w,
            h,
        )
    }

    fn open_thumb_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
        let meta = &self
            .series
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?
            .meta;
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
    codec: QuickTimeCodec,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum QuickTimeCodec {
    UncompressedRgb,
    UncompressedGray,
    Jpeg,
    Png,
    Cinepak { depth: u16 },
}

#[derive(Clone, Copy)]
struct QuickTimeSttsEntry {
    sample_count: u32,
    sample_delta: u32,
}

#[derive(Clone, Copy)]
struct QuickTimeEditEntry {
    segment_duration: u64,
    media_time: i64,
    media_rate: f64,
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

fn be_i32_at(data: &[u8], offset: usize) -> Option<i32> {
    data.get(offset..offset + 4)
        .map(|b| i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

fn quicktime_codec_from_fourcc(fourcc: &[u8], depth: u16) -> Result<QuickTimeCodec> {
    match fourcc {
        b"raw " | b"RAW " | b"rgb " => Ok(QuickTimeCodec::UncompressedRgb),
        b"gray" | b"GREY" | b"y800" => Ok(QuickTimeCodec::UncompressedGray),
        b"jpeg" | b"mjpa" | b"mjpb" | b"mjpg" | b"MJPG" => Ok(QuickTimeCodec::Jpeg),
        b"png " => Ok(QuickTimeCodec::Png),
        b"cvid" => Ok(QuickTimeCodec::Cinepak {
            depth: match depth {
                0 | 24 => 24,
                8 => 8,
                other => {
                    return Err(BioFormatsError::UnsupportedFormat(format!(
                        "QuickTime Cinepak depth {other} is unsupported"
                    )))
                }
            },
        }),
        other => Err(BioFormatsError::UnsupportedFormat(format!(
            "QuickTime codec {} is unsupported (family: {}); decoding this codec family requires an external video decoder",
            String::from_utf8_lossy(other),
            quicktime_codec_family(other)
        ))),
    }
}

fn quicktime_codec_family(fourcc: &[u8]) -> &'static str {
    match fourcc {
        b"raw " | b"RAW " | b"rgb " => "uncompressed RGB",
        b"gray" | b"GREY" | b"y800" => "uncompressed grayscale",
        b"jpeg" | b"mjpa" | b"mjpb" | b"mjpg" | b"MJPG" => "Motion JPEG",
        b"png " => "PNG",
        b"cvid" => "Cinepak",
        b"avc1" | b"avc2" | b"avc3" | b"avc4" | b"h264" | b"H264" | b"x264" | b"X264" => {
            "H.264/AVC"
        }
        b"hvc1" | b"hev1" => "H.265/HEVC",
        b"apch" | b"apcn" | b"apcs" | b"apco" | b"ap4h" | b"ap4x" => "Apple ProRes",
        b"mjp2" => "Motion JPEG 2000",
        b"dv  " | b"dvc " | b"dvcp" | b"dvhq" | b"dvcpro" => "DV",
        _ => "unknown codec family",
    }
}

fn decode_quicktime_jpeg_sample(
    sample: &[u8],
    meta: &ImageMetadata,
    plane_index: u32,
) -> Result<Vec<u8>> {
    let mut decoder = jpeg_decoder::Decoder::new(sample);
    let decoded = decoder.decode().map_err(|err| {
        BioFormatsError::UnsupportedFormat(format!(
            "QuickTime JPEG sample {plane_index} failed to decode: {err}"
        ))
    })?;
    let info = decoder.info().ok_or_else(|| {
        BioFormatsError::UnsupportedFormat(format!(
            "QuickTime JPEG sample {plane_index} has no image info"
        ))
    })?;
    if u32::from(info.width) != meta.size_x || u32::from(info.height) != meta.size_y {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "QuickTime JPEG sample {plane_index} is {}x{}, expected {}x{}",
            info.width, info.height, meta.size_x, meta.size_y
        )));
    }
    let samples_per_pixel = match info.pixel_format {
        jpeg_decoder::PixelFormat::L8 => 1,
        jpeg_decoder::PixelFormat::RGB24 => 3,
        other => {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "QuickTime JPEG sample {plane_index} pixel format {other:?} is unsupported"
            )))
        }
    };
    if samples_per_pixel as u32 != meta.size_c {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "QuickTime JPEG sample {plane_index} has {samples_per_pixel} channel(s), expected {}",
            meta.size_c
        )));
    }
    Ok(decoded)
}

fn decode_quicktime_png_sample(
    sample: &[u8],
    meta: &ImageMetadata,
    plane_index: u32,
) -> Result<Vec<u8>> {
    let image =
        image::load_from_memory_with_format(sample, image::ImageFormat::Png).map_err(|err| {
            BioFormatsError::UnsupportedFormat(format!(
                "QuickTime PNG sample {plane_index} failed to decode: {err}"
            ))
        })?;
    if image.width() != meta.size_x || image.height() != meta.size_y {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "QuickTime PNG sample {plane_index} is {}x{}, expected {}x{}",
            image.width(),
            image.height(),
            meta.size_x,
            meta.size_y
        )));
    }
    let (samples_per_pixel, decoded) = match image {
        image::DynamicImage::ImageLuma8(buffer) => (1usize, buffer.into_raw()),
        image::DynamicImage::ImageLumaA8(buffer) => (2usize, buffer.into_raw()),
        image::DynamicImage::ImageRgb8(buffer) => (3usize, buffer.into_raw()),
        image::DynamicImage::ImageRgba8(buffer) => (4usize, buffer.into_raw()),
        other => {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "QuickTime PNG sample {plane_index} pixel format {:?} is unsupported",
                other.color()
            )))
        }
    };
    if samples_per_pixel as u32 != meta.size_c {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "QuickTime PNG sample {plane_index} has {samples_per_pixel} channel(s), expected {}",
            meta.size_c
        )));
    }
    Ok(decoded)
}

fn decode_quicktime_cinepak_sample(
    sample: &[u8],
    meta: &ImageMetadata,
    plane_index: u32,
    depth: u16,
    previous: Option<&[u8]>,
) -> Result<Vec<u8>> {
    let expected_channels = match depth {
        8 => 1u32,
        24 => 3u32,
        other => {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "QuickTime Cinepak sample {plane_index} depth {other} is unsupported"
            )))
        }
    };
    if meta.size_c != expected_channels || meta.pixel_type != PixelType::Uint8 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "QuickTime Cinepak sample {plane_index} only supports {expected_channels}-channel Uint8 output"
        )));
    }
    if sample.len() < 10 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "QuickTime Cinepak sample {plane_index} is truncated"
        )));
    }
    let expected = meta
        .size_x
        .checked_mul(meta.size_y)
        .and_then(|px| (px as usize).checked_mul(expected_channels as usize))
        .ok_or_else(|| BioFormatsError::Format("QuickTime Cinepak plane size overflows".into()))?;
    let previous = match previous {
        Some(previous) if previous.len() == expected => previous,
        Some(previous) => {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "QuickTime Cinepak sample {plane_index} previous frame has {} bytes, expected {expected}",
                previous.len()
            )))
        }
        None if sample[0] == 0 => &[],
        None => {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "QuickTime Cinepak sample {plane_index} is a delta frame without a previous frame"
            )))
        }
    };
    let decoded = crate::common::codec::decompress_cinepak(
        sample,
        meta.size_x,
        meta.size_y,
        depth as u32,
        previous,
    )?;
    if decoded.len() != expected {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "QuickTime Cinepak sample {plane_index} decoded to {} bytes, expected {expected}",
            decoded.len()
        )));
    }
    Ok(decoded)
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

fn descendant<'a>(atom: Atom<'a>, path: &[[u8; 4]]) -> Result<Option<Atom<'a>>> {
    let mut atoms = scan_atoms(atom.data, atom.start + 8)?;
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

fn parse_quicktime_time_header(atom: Atom<'_>, atom_name: &str) -> Result<(u32, u64)> {
    if atom.data.is_empty() {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "QuickTime {atom_name} atom is truncated"
        )));
    }
    match atom.data[0] {
        0 => {
            if atom.data.len() < 24 {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "QuickTime {atom_name} atom is truncated"
                )));
            }
            Ok((
                be_u32_at(atom.data, 12).unwrap(),
                be_u32_at(atom.data, 16).unwrap() as u64,
            ))
        }
        version => Err(BioFormatsError::UnsupportedFormat(format!(
            "QuickTime {atom_name} version {version} is unsupported"
        ))),
    }
}

fn parse_quicktime_stts(
    stts: Atom<'_>,
    expected_samples: usize,
) -> Result<Vec<QuickTimeSttsEntry>> {
    if stts.data.len() < 8 {
        return Err(BioFormatsError::UnsupportedFormat(
            "QuickTime stts atom is truncated".into(),
        ));
    }
    let entry_count = be_u32_at(stts.data, 4).unwrap() as usize;
    if stts.data.len() < 8 + entry_count * 8 {
        return Err(BioFormatsError::UnsupportedFormat(
            "QuickTime stts table is truncated".into(),
        ));
    }
    let mut entries = Vec::with_capacity(entry_count);
    let mut total = 0usize;
    for i in 0..entry_count {
        let base = 8 + i * 8;
        let sample_count = be_u32_at(stts.data, base).unwrap();
        let sample_delta = be_u32_at(stts.data, base + 4).unwrap();
        if sample_count == 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "QuickTime stts contains a zero sample count".into(),
            ));
        }
        total += sample_count as usize;
        entries.push(QuickTimeSttsEntry {
            sample_count,
            sample_delta,
        });
    }
    if total != expected_samples {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "QuickTime stts sample count {total} does not match stsz sample count {expected_samples}"
        )));
    }
    Ok(entries)
}

fn parse_quicktime_elst(elst: Atom<'_>) -> Result<Vec<QuickTimeEditEntry>> {
    if elst.data.len() < 8 {
        return Err(BioFormatsError::UnsupportedFormat(
            "QuickTime elst atom is truncated".into(),
        ));
    }
    if elst.data[0] != 0 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "QuickTime elst version {} is unsupported",
            elst.data[0]
        )));
    }
    let entry_count = be_u32_at(elst.data, 4).unwrap() as usize;
    if elst.data.len() < 8 + entry_count * 12 {
        return Err(BioFormatsError::UnsupportedFormat(
            "QuickTime elst table is truncated".into(),
        ));
    }
    let mut entries = Vec::with_capacity(entry_count);
    for i in 0..entry_count {
        let base = 8 + i * 12;
        let rate = be_i32_at(elst.data, base + 8).unwrap();
        entries.push(QuickTimeEditEntry {
            segment_duration: be_u32_at(elst.data, base).unwrap() as u64,
            media_time: be_i32_at(elst.data, base + 4).unwrap() as i64,
            media_rate: f64::from(rate) / 65536.0,
        });
    }
    Ok(entries)
}

fn quicktime_stts_total_duration(entries: &[QuickTimeSttsEntry]) -> Option<u64> {
    entries.iter().try_fold(0u64, |total, entry| {
        total.checked_add(u64::from(entry.sample_count) * u64::from(entry.sample_delta))
    })
}

fn quicktime_sample_media_times(entries: &[QuickTimeSttsEntry]) -> Option<Vec<u64>> {
    let sample_count = entries.iter().try_fold(0usize, |total, entry| {
        total.checked_add(entry.sample_count as usize)
    })?;
    let mut times = Vec::with_capacity(sample_count);
    let mut time = 0u64;
    for entry in entries {
        for _ in 0..entry.sample_count {
            times.push(time);
            time = time.checked_add(u64::from(entry.sample_delta))?;
        }
    }
    Some(times)
}

fn quicktime_insert_u64_metadata(
    metadata: &mut HashMap<String, MetadataValue>,
    key: &str,
    value: u64,
) {
    metadata.insert(
        key.into(),
        i64::try_from(value)
            .map(MetadataValue::Int)
            .unwrap_or_else(|_| MetadataValue::String(value.to_string())),
    );
}

fn quicktime_insert_i64_list_metadata(
    metadata: &mut HashMap<String, MetadataValue>,
    key: &str,
    values: &[i64],
) {
    metadata.insert(
        key.into(),
        MetadataValue::String(
            values
                .iter()
                .map(i64::to_string)
                .collect::<Vec<_>>()
                .join(","),
        ),
    );
}

fn quicktime_insert_u64_list_metadata(
    metadata: &mut HashMap<String, MetadataValue>,
    key: &str,
    values: &[u64],
) {
    metadata.insert(
        key.into(),
        MetadataValue::String(
            values
                .iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join(","),
        ),
    );
}

fn quicktime_insert_seconds_list_metadata(
    metadata: &mut HashMap<String, MetadataValue>,
    key: &str,
    values: &[i64],
    timescale: u32,
) {
    metadata.insert(
        key.into(),
        MetadataValue::String(
            values
                .iter()
                .map(|value| format!("{}", *value as f64 / f64::from(timescale)))
                .collect::<Vec<_>>()
                .join(","),
        ),
    );
}

fn quicktime_movie_ticks_to_media_ticks(
    movie_ticks: u64,
    media_timescale: Option<u32>,
    movie_timescale: Option<u32>,
) -> Option<u64> {
    if movie_ticks == 0 {
        return Some(0);
    }
    let (Some(media_timescale), Some(movie_timescale)) = (media_timescale, movie_timescale) else {
        return None;
    };
    if movie_timescale == 0 {
        return None;
    }
    let numerator = u128::from(movie_ticks) * u128::from(media_timescale);
    let denominator = u128::from(movie_timescale);
    if numerator % denominator == 0 {
        u64::try_from(numerator / denominator).ok()
    } else {
        None
    }
}

fn quicktime_multi_segment_presentation_times(
    entries: &[QuickTimeEditEntry],
    empty_duration_media_ticks: u64,
    sample_media_times: &[u64],
    media_duration_ticks: u64,
    media_timescale: Option<u32>,
    movie_timescale: Option<u32>,
) -> std::result::Result<Vec<i64>, String> {
    let mut out = vec![None; sample_media_times.len()];
    let mut cursor = empty_duration_media_ticks;
    for entry in entries {
        let start =
            u64::try_from(entry.media_time).map_err(|_| "negative media_time".to_string())?;
        let duration = quicktime_movie_ticks_to_media_ticks(
            entry.segment_duration,
            media_timescale,
            movie_timescale,
        )
        .ok_or_else(|| {
            "media segment duration cannot be represented exactly in media ticks".to_string()
        })?;
        let end = start
            .checked_add(duration)
            .ok_or_else(|| "media segment duration overflows".to_string())?;
        if end > media_duration_ticks {
            return Err("media segment extends past media duration".into());
        }
        let start_index = sample_media_times
            .binary_search(&start)
            .map_err(|_| "media segment start is not sample-aligned".to_string())?;
        let end_index = if end == media_duration_ticks {
            sample_media_times.len()
        } else {
            sample_media_times
                .binary_search(&end)
                .map_err(|_| "media segment end is not sample-aligned".to_string())?
        };
        if start_index >= end_index {
            return Err("media segment contains no complete samples".into());
        }
        for sample_index in start_index..end_index {
            if out[sample_index].is_some() {
                return Err("media segments overlap in sample space".into());
            }
            let t = cursor
                .checked_add(sample_media_times[sample_index] - start)
                .ok_or_else(|| "presentation time overflows".to_string())?;
            out[sample_index] = Some(
                i64::try_from(t).map_err(|_| "presentation time exceeds i64 range".to_string())?,
            );
        }
        cursor = cursor
            .checked_add(duration)
            .ok_or_else(|| "presentation timeline duration overflows".to_string())?;
    }
    out.into_iter()
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| "edit list media segments do not cover every sample".into())
}

fn quicktime_edit_presentation_times(
    entries: &[QuickTimeEditEntry],
    media_timescale: Option<u32>,
    movie_timescale: Option<u32>,
    sample_media_times: &[u64],
    media_duration_ticks: u64,
    metadata: &mut HashMap<String, MetadataValue>,
) -> Option<Vec<i64>> {
    let mut empty_count = 0usize;
    let mut empty_movie_ticks = 0u64;
    let mut media_segments = Vec::new();
    for entry in entries {
        if (entry.media_rate - 1.0).abs() > f64::EPSILON {
            metadata.insert(
                "quicktime.edit_list.presentation_status".into(),
                MetadataValue::String("not_applied_non_unit_rate".into()),
            );
            metadata.insert(
                "quicktime.edit_list.presentation_diagnostic".into(),
                MetadataValue::String(format!(
                    "edit list contains media_rate {}",
                    entry.media_rate
                )),
            );
            metadata.insert(
                "quicktime.edit_list.media_rate".into(),
                MetadataValue::Float(entry.media_rate),
            );
            return None;
        }
        if entry.media_time < 0 {
            if !media_segments.is_empty() {
                metadata.insert(
                    "quicktime.edit_list.presentation_status".into(),
                    MetadataValue::String("not_applied_complex_edit_list".into()),
                );
                metadata.insert(
                    "quicktime.edit_list.presentation_diagnostic".into(),
                    MetadataValue::String("empty edit follows a media segment".into()),
                );
                return None;
            }
            empty_count += 1;
            empty_movie_ticks = empty_movie_ticks.checked_add(entry.segment_duration)?;
        } else {
            media_segments.push(*entry);
        }
    }
    metadata.insert(
        "quicktime.edit_list.empty_edit_count".into(),
        MetadataValue::Int(empty_count as i64),
    );
    quicktime_insert_u64_metadata(
        metadata,
        "quicktime.edit_list.empty_duration_movie_ticks",
        empty_movie_ticks,
    );
    let first = media_segments.first()?;
    metadata.insert(
        "quicktime.edit_list.media_time_ticks".into(),
        MetadataValue::Int(first.media_time),
    );
    metadata.insert(
        "quicktime.edit_list.media_rate".into(),
        MetadataValue::Float(first.media_rate),
    );
    let empty_media_ticks =
        quicktime_movie_ticks_to_media_ticks(empty_movie_ticks, media_timescale, movie_timescale)?;
    if media_segments.len() == 1 {
        let offset = i64::try_from(empty_media_ticks)
            .ok()?
            .checked_sub(first.media_time)?;
        metadata.insert(
            "quicktime.edit_list.presentation_status".into(),
            MetadataValue::String(if empty_count == 0 {
                "applied_single_normal_speed_media_segment".into()
            } else {
                "applied_leading_empty_edits_single_normal_speed_media_segment".into()
            }),
        );
        metadata.insert(
            "quicktime.edit_list.presentation_diagnostic".into(),
            MetadataValue::String(format!(
                "media_time {} applied at normal speed",
                first.media_time
            )),
        );
        metadata.insert(
            "quicktime.edit_list.presentation_offset_ticks".into(),
            MetadataValue::Int(offset),
        );
        return sample_media_times
            .iter()
            .map(|time| i64::try_from(*time).ok()?.checked_add(offset))
            .collect();
    }
    match quicktime_multi_segment_presentation_times(
        &media_segments,
        empty_media_ticks,
        sample_media_times,
        media_duration_ticks,
        media_timescale,
        movie_timescale,
    ) {
        Ok(times) => {
            metadata.insert(
                "quicktime.edit_list.presentation_status".into(),
                MetadataValue::String(if empty_count == 0 {
                    "applied_multiple_normal_speed_media_segments".into()
                } else {
                    "applied_leading_empty_edits_multiple_normal_speed_media_segments".into()
                }),
            );
            metadata.insert(
                "quicktime.edit_list.presentation_diagnostic".into(),
                MetadataValue::String(format!(
                    "{} media segments applied at normal speed with sample-aligned boundaries",
                    media_segments.len()
                )),
            );
            Some(times)
        }
        Err(diagnostic) => {
            metadata.insert(
                "quicktime.edit_list.presentation_status".into(),
                MetadataValue::String("not_applied_complex_edit_list".into()),
            );
            metadata.insert(
                "quicktime.edit_list.presentation_diagnostic".into(),
                MetadataValue::String(format!("multiple media segments not applied: {diagnostic}")),
            );
            None
        }
    }
}

fn parse_quicktime(data: &[u8]) -> Result<Vec<QuickTimeParsed>> {
    if scan_atoms(data, 0)?.is_empty() {
        return Err(BioFormatsError::UnsupportedFormat(
            "QuickTime file has no atoms".into(),
        ));
    }
    let moov = first_descendant(data, &[*b"moov"])?
        .ok_or_else(|| BioFormatsError::UnsupportedFormat("QuickTime missing moov atom".into()))?;
    let tracks = scan_atoms(moov.data, moov.start + 8)?;
    let video_tracks = tracks
        .iter()
        .copied()
        .filter(|atom| atom.kind == *b"trak")
        .filter_map(|trak| {
            let stsd = descendant(trak, &[*b"mdia", *b"minf", *b"stbl", *b"stsd"]).ok()??;
            let stsz = descendant(trak, &[*b"mdia", *b"minf", *b"stbl", *b"stsz"]).ok()??;
            Some((trak, stsd, stsz))
        })
        .collect::<Vec<_>>();
    if video_tracks.is_empty() {
        return Err(BioFormatsError::UnsupportedFormat(
            "QuickTime missing stsd atom".into(),
        ));
    }

    let movie_header = first_descendant(data, &[*b"moov", *b"mvhd"])?
        .map(|atom| parse_quicktime_time_header(atom, "mvhd"))
        .transpose()?;
    let mut parsed_tracks = Vec::with_capacity(video_tracks.len());
    for (track_index, (trak, stsd, stsz)) in video_tracks.iter().copied().enumerate() {
        parsed_tracks.push(parse_quicktime_track(
            data,
            trak,
            stsd,
            stsz,
            movie_header,
            video_tracks.len(),
            track_index,
        )?);
    }

    if parsed_tracks.len() > 1 && !quicktime_tracks_are_compatible(&parsed_tracks) {
        let diagnostics = parsed_tracks
            .iter()
            .enumerate()
            .map(|(index, parsed)| {
                format!(
                    "track {}: codec={} {}x{} samples={}",
                    index + 1,
                    parsed
                        .meta
                        .series_metadata
                        .get("quicktime.codec")
                        .map(ToString::to_string)
                        .unwrap_or_else(|| "unknown".into()),
                    parsed.meta.size_x,
                    parsed.meta.size_y,
                    parsed.meta.image_count
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "QuickTime files with multiple incompatible video tracks are unsupported ({diagnostics})"
        )));
    }

    Ok(parsed_tracks)
}

fn quicktime_tracks_are_compatible(tracks: &[QuickTimeParsed]) -> bool {
    let Some(first) = tracks.first() else {
        return true;
    };
    tracks.iter().skip(1).all(|track| {
        track.codec == first.codec
            && track.samples_per_pixel == first.samples_per_pixel
            && track.meta.size_x == first.meta.size_x
            && track.meta.size_y == first.meta.size_y
            && track.meta.size_c == first.meta.size_c
            && track.meta.pixel_type == first.meta.pixel_type
            && track.meta.bits_per_pixel == first.meta.bits_per_pixel
            && track.meta.is_rgb == first.meta.is_rgb
            && track.meta.is_interleaved == first.meta.is_interleaved
    })
}

fn parse_quicktime_track(
    data: &[u8],
    trak: Atom<'_>,
    stsd: Atom<'_>,
    stsz: Atom<'_>,
    movie_header: Option<(u32, u64)>,
    video_track_count: usize,
    video_track_index: usize,
) -> Result<QuickTimeParsed> {
    let stco = descendant(trak, &[*b"mdia", *b"minf", *b"stbl", *b"stco"])?
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
    let sample_depth = be_u16_at(entry, 82).unwrap_or(0);
    let qt_codec = quicktime_codec_from_fourcc(codec, sample_depth)?;
    let width = be_u16_at(entry, 32).unwrap_or(0) as u32;
    let height = be_u16_at(entry, 34).unwrap_or(0) as u32;
    if width == 0 || height == 0 {
        return Err(BioFormatsError::UnsupportedFormat(
            "QuickTime video sample entry has non-positive dimensions".into(),
        ));
    }
    let mut samples_per_pixel = match qt_codec {
        QuickTimeCodec::UncompressedRgb => 3usize,
        QuickTimeCodec::UncompressedGray => 1usize,
        QuickTimeCodec::Jpeg | QuickTimeCodec::Png => 3usize,
        QuickTimeCodec::Cinepak { depth: 8 } => 1usize,
        QuickTimeCodec::Cinepak { .. } => 3usize,
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
    let first_sample =
        &data[sample_offsets[0] as usize..sample_offsets[0] as usize + sample_sizes[0] as usize];
    match qt_codec {
        QuickTimeCodec::Jpeg => {
            let mut decoder = jpeg_decoder::Decoder::new(first_sample);
            decoder.decode().map_err(|err| {
                BioFormatsError::UnsupportedFormat(format!(
                    "QuickTime JPEG sample 0 failed to decode: {err}"
                ))
            })?;
            let info = decoder.info().ok_or_else(|| {
                BioFormatsError::UnsupportedFormat(
                    "QuickTime JPEG sample 0 has no image info".into(),
                )
            })?;
            samples_per_pixel = match info.pixel_format {
                jpeg_decoder::PixelFormat::L8 => 1,
                jpeg_decoder::PixelFormat::RGB24 => 3,
                other => {
                    return Err(BioFormatsError::UnsupportedFormat(format!(
                        "QuickTime JPEG sample 0 pixel format {other:?} is unsupported"
                    )))
                }
            };
        }
        QuickTimeCodec::Png => {
            let image = image::load_from_memory_with_format(first_sample, image::ImageFormat::Png)
                .map_err(|err| {
                    BioFormatsError::UnsupportedFormat(format!(
                        "QuickTime PNG sample 0 failed to decode: {err}"
                    ))
                })?;
            samples_per_pixel = match image.color() {
                image::ColorType::L8 => 1,
                image::ColorType::La8 => 2,
                image::ColorType::Rgb8 => 3,
                image::ColorType::Rgba8 => 4,
                other => {
                    return Err(BioFormatsError::UnsupportedFormat(format!(
                        "QuickTime PNG sample 0 pixel format {other:?} is unsupported"
                    )))
                }
            };
        }
        QuickTimeCodec::Cinepak { depth } => {
            let probe_meta = ImageMetadata {
                size_x: width,
                size_y: height,
                size_z: 1,
                size_c: samples_per_pixel as u32,
                size_t: sample_sizes.len() as u32,
                pixel_type: PixelType::Uint8,
                bits_per_pixel: 8,
                image_count: sample_sizes.len() as u32,
                dimension_order: DimensionOrder::XYZCT,
                is_rgb: samples_per_pixel == 3,
                is_interleaved: samples_per_pixel == 3,
                is_indexed: false,
                is_little_endian: false,
                resolution_count: 1,
                series_metadata: HashMap::new(),
                lookup_table: None,
                modulo_z: None,
                modulo_c: None,
                modulo_t: None,
            };
            decode_quicktime_cinepak_sample(first_sample, &probe_meta, 0, depth, None)?;
        }
        _ => {}
    }

    let pixel_type = PixelType::Uint8;
    let mut metadata = HashMap::new();
    metadata.insert(
        "quicktime.codec".into(),
        MetadataValue::String(String::from_utf8_lossy(codec).into_owned()),
    );
    metadata.insert(
        "quicktime.codec_family".into(),
        MetadataValue::String(quicktime_codec_family(codec).into()),
    );
    metadata.insert(
        "quicktime.video_track_count".into(),
        MetadataValue::Int(video_track_count as i64),
    );
    metadata.insert(
        "quicktime.video_track_index".into(),
        MetadataValue::Int(video_track_index as i64),
    );
    quicktime_insert_u64_metadata(
        &mut metadata,
        "quicktime.sample_count",
        sample_sizes.len() as u64,
    );
    if let QuickTimeCodec::Cinepak { depth } = qt_codec {
        metadata.insert(
            "quicktime.cinepak.depth".into(),
            MetadataValue::Int(depth as i64),
        );
    }
    let media_header = descendant(trak, &[*b"mdia", *b"mdhd"])?
        .map(|atom| parse_quicktime_time_header(atom, "mdhd"))
        .transpose()?;
    let stts_entries = descendant(trak, &[*b"mdia", *b"minf", *b"stbl", *b"stts"])?
        .map(|atom| parse_quicktime_stts(atom, sample_sizes.len()))
        .transpose()?;
    let edit_entries = descendant(trak, &[*b"edts", *b"elst"])?
        .map(parse_quicktime_elst)
        .transpose()?;
    if let Some((timescale, duration)) = media_header {
        metadata.insert(
            "quicktime.timescale".into(),
            MetadataValue::Int(timescale as i64),
        );
        quicktime_insert_u64_metadata(&mut metadata, "quicktime.duration_ticks", duration);
        metadata.insert(
            "quicktime.duration_seconds".into(),
            MetadataValue::Float(duration as f64 / f64::from(timescale)),
        );
    }
    if let Some((timescale, duration)) = movie_header {
        metadata.insert(
            "quicktime.movie_timescale".into(),
            MetadataValue::Int(timescale as i64),
        );
        quicktime_insert_u64_metadata(&mut metadata, "quicktime.movie_duration_ticks", duration);
    }
    if let Some(entries) = &stts_entries {
        if let Some(sample_media_times) = quicktime_sample_media_times(entries) {
            quicktime_insert_u64_list_metadata(
                &mut metadata,
                "quicktime.sample_media_time_ticks",
                &sample_media_times,
            );
            let sample_presentation_times = if let Some(edit_entries) = &edit_entries {
                quicktime_edit_presentation_times(
                    edit_entries,
                    media_header.map(|(timescale, _)| timescale),
                    movie_header.map(|(timescale, _)| timescale),
                    &sample_media_times,
                    quicktime_stts_total_duration(entries).unwrap_or(0),
                    &mut metadata,
                )
            } else {
                sample_media_times
                    .iter()
                    .map(|time| i64::try_from(*time).ok())
                    .collect::<Option<Vec<_>>>()
            };
            if let Some(sample_presentation_times) = sample_presentation_times {
                quicktime_insert_i64_list_metadata(
                    &mut metadata,
                    "quicktime.sample_presentation_time_ticks",
                    &sample_presentation_times,
                );
                if let Some((timescale, _)) = media_header {
                    quicktime_insert_seconds_list_metadata(
                        &mut metadata,
                        "quicktime.sample_presentation_time_seconds",
                        &sample_presentation_times,
                        timescale,
                    );
                }
            }
        }
    }
    if let Some(entries) = &edit_entries {
        metadata.insert(
            "quicktime.edit_list.count".into(),
            MetadataValue::Int(entries.len() as i64),
        );
    }
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
        is_rgb: samples_per_pixel >= 3,
        is_interleaved: samples_per_pixel > 1,
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
        codec: qt_codec,
    })
}

// ---------------------------------------------------------------------------
// 2. Multiple-image Network Graphics
// ---------------------------------------------------------------------------
/// MNG (Multiple-image Network Graphics) reader (`.mng`).
///
/// Faithful port of the Java `MNGReader` (formats-bsd). MNG is a container of
/// embedded PNG (and, for JNG, JPEG) datastreams. `set_id` walks the
/// `[len][code][data][crc]` chunk chain (big-endian), recording the byte range
/// of each embedded image between an `IHDR`/`JDAT` chunk and its terminating
/// `IEND`, honouring `LOOP`/`ENDL` iteration markers. Frames are then grouped
/// by `(width, height, bands, pixel type)` into series, exactly as the Java
/// reader does by decoding each frame once to read its dimensions.
///
/// Each embedded PNG frame is reconstructed by prepending the 8-byte PNG
/// signature to the recorded chunk bytes and decoded with the `image` crate.
/// Pixel data is returned channel-separated (`is_interleaved = false`,
/// big-endian, matching `MNGReader`'s `littleEndian = false`).
const MNG_MAGIC: [u8; 8] = [0x8a, 0x4d, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
const PNG_SIGNATURE: [u8; 8] = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];

#[derive(Clone)]
struct MngSeries {
    offsets: Vec<usize>,
    lengths: Vec<usize>,
    meta: ImageMetadata,
}

pub struct MngReader {
    path: Option<PathBuf>,
    is_jng: bool,
    series: Vec<MngSeries>,
    current: usize,
}

impl MngReader {
    pub fn new() -> Self {
        MngReader {
            path: None,
            is_jng: false,
            series: Vec::new(),
            current: 0,
        }
    }

    /// Decode the embedded image at `[offset, end)` into a `DynamicImage`.
    fn decode_frame(
        data: &[u8],
        offset: usize,
        end: usize,
        is_jng: bool,
    ) -> Result<image::DynamicImage> {
        if end <= offset || end > data.len() {
            return Err(BioFormatsError::InvalidData(
                "MNG frame range is outside the file".into(),
            ));
        }
        if is_jng {
            // JNG embeds a JPEG datastream; Java does not fully support JNG, so
            // this is a best-effort JPEG decode of the recorded bytes.
            image::load_from_memory_with_format(&data[offset..end], image::ImageFormat::Jpeg)
                .map_err(|e| BioFormatsError::Codec(format!("MNG/JNG decode: {e}")))
        } else {
            let mut png = Vec::with_capacity(8 + (end - offset));
            png.extend_from_slice(&PNG_SIGNATURE);
            png.extend_from_slice(&data[offset..end]);
            image::load_from_memory_with_format(&png, image::ImageFormat::Png)
                .map_err(|e| BioFormatsError::Codec(format!("MNG/PNG decode: {e}")))
        }
    }

    /// Per-frame geometry: width, height, band count, pixel type.
    fn frame_info(img: &image::DynamicImage) -> (u32, u32, u32, PixelType) {
        use image::DynamicImage::*;
        let (w, h) = (img.width(), img.height());
        let (bands, pt) = match img {
            ImageLuma8(_) => (1, PixelType::Uint8),
            ImageLumaA8(_) => (2, PixelType::Uint8),
            ImageRgb8(_) => (3, PixelType::Uint8),
            ImageRgba8(_) => (4, PixelType::Uint8),
            ImageLuma16(_) => (1, PixelType::Uint16),
            ImageLumaA16(_) => (2, PixelType::Uint16),
            ImageRgb16(_) => (3, PixelType::Uint16),
            ImageRgba16(_) => (4, PixelType::Uint16),
            ImageRgb32F(_) => (3, PixelType::Float32),
            ImageRgba32F(_) => (4, PixelType::Float32),
            _ => (3, PixelType::Uint8),
        };
        (w, h, bands, pt)
    }

    /// Convert a decoded frame into channel-separated (planar) bytes. Multi-byte
    /// samples are emitted big-endian to match `littleEndian = false`.
    fn frame_to_planar(img: &image::DynamicImage, bands: u32, pt: PixelType) -> Vec<u8> {
        let w = img.width() as usize;
        let h = img.height() as usize;
        let pixels = w * h;
        let bands = bands as usize;
        match pt {
            PixelType::Uint8 => {
                let interleaved = img.to_rgba8();
                let src = interleaved.as_raw();
                // image always gives 4-band RGBA8 from to_rgba8; remap to the
                // declared band count.
                let mut out = vec![0u8; pixels * bands];
                for p in 0..pixels {
                    for b in 0..bands {
                        out[b * pixels + p] = src[p * 4 + b.min(3)];
                    }
                }
                out
            }
            PixelType::Uint16 => {
                let interleaved = img.to_rgba16();
                let src = interleaved.as_raw();
                let mut out = vec![0u8; pixels * bands * 2];
                for p in 0..pixels {
                    for b in 0..bands {
                        let v = src[p * 4 + b.min(3)];
                        let be = v.to_be_bytes();
                        let dst = (b * pixels + p) * 2;
                        out[dst] = be[0];
                        out[dst + 1] = be[1];
                    }
                }
                out
            }
            _ => {
                // Float / other: fall back to interleaved RGBA8.
                img.to_rgba8().into_raw()
            }
        }
    }

    fn parse(path: &Path) -> Result<(bool, Vec<MngSeries>)> {
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        let mut c = Cursor::new(&data, false); // MNG is big-endian

        c.skip(12);
        if c.read_string(4) != "MHDR" {
            return Err(BioFormatsError::Format("Invalid MNG file.".into()));
        }
        c.skip(32);

        let mut offsets: Vec<usize> = Vec::new();
        let mut lengths: Vec<usize> = Vec::new();
        let mut stack: Vec<i64> = Vec::new();
        let mut max_iterations: i32 = 0;
        let mut current_iteration: i32 = 0;
        let mut is_jng = false;

        // Read the sequence of [len, code, value] chunks.
        while c.fp() + 8 <= data.len() {
            let len = c.read_int() as i64;
            let code = c.read_string(4);
            let fp = c.fp() as i64;

            match code.as_str() {
                "IHDR" => offsets.push((fp - 8).max(0) as usize),
                "JDAT" => {
                    is_jng = true;
                    offsets.push(fp as usize);
                }
                "IEND" => lengths.push((fp + len + 4).max(0) as usize),
                "LOOP" => {
                    stack.push(fp + len + 4);
                    c.skip(1);
                    max_iterations = c.read_int();
                }
                "ENDL" => {
                    if let Some(&seek) = stack.last() {
                        if current_iteration < max_iterations {
                            c.seek(seek.max(0) as usize);
                            current_iteration += 1;
                        } else {
                            stack.pop();
                            max_iterations = 0;
                            current_iteration = 0;
                        }
                    }
                }
                _ => {}
            }
            // Skip to the start of the next chunk (data + 4-byte CRC).
            let next = fp + len + 4;
            if next < 0 {
                break;
            }
            c.seek(next as usize);
        }

        // Group frames by (width-height-bands-pixeltype), preserving order.
        let mut keys: Vec<(u32, u32, u32, PixelType)> = Vec::new();
        let mut grouped_offsets: Vec<Vec<usize>> = Vec::new();
        let mut grouped_lengths: Vec<Vec<usize>> = Vec::new();

        for i in 0..offsets.len() {
            let offset = offsets[i];
            let end = match lengths.get(i) {
                Some(&e) => e,
                None => continue,
            };
            if end < offset {
                continue;
            }
            let img = Self::decode_frame(&data, offset, end, is_jng)?;
            let (w, h, bands, pt) = Self::frame_info(&img);
            let key = (w, h, bands, pt);
            let idx = match keys.iter().position(|k| *k == key) {
                Some(idx) => idx,
                None => {
                    keys.push(key);
                    grouped_offsets.push(Vec::new());
                    grouped_lengths.push(Vec::new());
                    keys.len() - 1
                }
            };
            grouped_offsets[idx].push(offset);
            grouped_lengths[idx].push(end);
        }

        if keys.is_empty() {
            return Err(BioFormatsError::Format("Pixel data not found.".into()));
        }

        let mut series = Vec::with_capacity(keys.len());
        for (i, &(w, h, bands, pt)) in keys.iter().enumerate() {
            let count = grouped_offsets[i].len() as u32;
            let meta = ImageMetadata {
                size_x: w,
                size_y: h,
                size_z: 1,
                size_c: bands,
                size_t: count,
                pixel_type: pt,
                bits_per_pixel: (pt.bytes_per_sample() * 8) as u8,
                image_count: count,
                dimension_order: DimensionOrder::XYCZT,
                is_rgb: bands > 1,
                is_interleaved: false,
                is_indexed: false,
                is_little_endian: false,
                ..ImageMetadata::default()
            };
            series.push(MngSeries {
                offsets: grouped_offsets[i].clone(),
                lengths: grouped_lengths[i].clone(),
                meta,
            });
        }
        Ok((is_jng, series))
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
        header.len() >= 8 && header[..8] == MNG_MAGIC
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let (is_jng, series) = Self::parse(path)?;
        self.path = Some(path.to_path_buf());
        self.is_jng = is_jng;
        self.series = series;
        self.current = 0;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.is_jng = false;
        self.series.clear();
        self.current = 0;
        Ok(())
    }
    fn series_count(&self) -> usize {
        self.series.len()
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if s < self.series.len() {
            self.current = s;
            Ok(())
        } else {
            Err(BioFormatsError::SeriesOutOfRange(s))
        }
    }
    fn series(&self) -> usize {
        self.current
    }
    fn metadata(&self) -> &ImageMetadata {
        self.series
            .get(self.current)
            .map(|s| &s.meta)
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }
    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let path = self.path.clone().ok_or(BioFormatsError::NotInitialized)?;
        let s = self
            .series
            .get(self.current)
            .ok_or(BioFormatsError::NotInitialized)?;
        let no = plane_index as usize;
        if no >= s.offsets.len() {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let (offset, end) = (s.offsets[no], s.lengths[no]);
        let (bands, pt) = (s.meta.size_c, s.meta.pixel_type);
        let data = std::fs::read(&path).map_err(BioFormatsError::Io)?;
        let img = Self::decode_frame(&data, offset, end, self.is_jng)?;
        Ok(Self::frame_to_planar(&img, bands, pt))
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
        let s = self
            .series
            .get(self.current)
            .ok_or(BioFormatsError::NotInitialized)?;
        crop_planar("MNG", &full, &s.meta, s.meta.size_c as usize, x, y, w, h)
    }
    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let (sx, sy) = {
            let m = self.metadata();
            (m.size_x, m.size_y)
        };
        let tw = sx.min(256);
        let th = sy.min(256);
        let tx = (sx - tw) / 2;
        let ty = (sy - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}

// ---------------------------------------------------------------------------
// 4. 3i SlideBook
// ---------------------------------------------------------------------------
/// 3i SlideBook reader (`.sld`).
///
/// BEST-EFFORT port of the Java `SlidebookReader`. SlideBook files are a
/// loosely documented sequence of variable-length pixel-data blocks and
/// fixed/variable-length metadata blocks. This port faithfully reproduces:
///
/// - detection (`isThisType`): little/big-endian flag at offset 4, then the
///   two magic shorts;
/// - the block-offset scan (`initFile` lines 242-396) that records every
///   metadata block and confirms each candidate pixel block by scanning forward
///   for the `h`/`i`/`j`/`k`/`n` identifier that follows it;
/// - reading the `'i'` and `'u'` metadata blocks to recover sizeX/sizeY (with
///   the divisor fix-up), sizeC (`iCount`) and sizeZ (`uCount`).
///
/// Each surviving pixel block becomes one series of 16-bit planes. Planes are
/// uncompressed and read directly by byte offset.
///
/// NOT PORTED (Java lines ~758-1207): the extensive heuristic dimension
/// disambiguation, montage/spool handling, image-name based series flattening,
/// and physical-size/channel-name metadata. When the recovered geometry cannot
/// be factored cleanly into the available planes, this reader returns an honest
/// `UnsupportedFormat`/`Format` error rather than fabricating a layout.
struct SlideBookSeries {
    meta: ImageMetadata,
    plane_offsets: Vec<usize>,
    plane_bytes: usize,
}

pub struct SlideBookReader {
    path: Option<PathBuf>,
    series: Vec<SlideBookSeries>,
    current: usize,
}

impl SlideBookReader {
    pub fn new() -> Self {
        SlideBookReader {
            path: None,
            series: Vec::new(),
            current: 0,
        }
    }

    fn within_pixels(offset: usize, pixel_offsets: &[usize], pixel_lengths: &[usize]) -> bool {
        for i in 0..pixel_offsets.len() {
            let po = pixel_offsets[i];
            let pl = pixel_lengths.get(i).copied().unwrap_or(0);
            if offset >= po && offset < po + pl {
                return true;
            }
        }
        false
    }

    /// Scan the file for metadata and pixel block offsets (Java initFile
    /// 228-396). Returns (little_endian, metadata_offsets, pixel_offsets,
    /// pixel_lengths).
    fn scan_offsets(data: &[u8]) -> Result<(bool, Vec<usize>, Vec<usize>, Vec<usize>)> {
        let mut c = Cursor::new(data, true);
        c.skip(4);
        let little = c.read() == 0x49; // 'I'
        c.order(little);

        let mut metadata_offsets: Vec<usize> = Vec::new();
        let mut pixel_offsets: Vec<usize> = Vec::new();
        let mut pixel_lengths: Vec<usize> = Vec::new();

        c.seek(0);
        let total = data.len();
        let is_marker = |b0: u8, b1: u8| (b0 == b'I' && b1 == b'I') || (b0 == b'M' && b1 == b'M');

        while c.fp() + 8 < total {
            c.skip(4);
            let check_one = c.read();
            let check_two = c.read();

            if (check_one == b'I' as i32 && check_two == b'I' as i32)
                || (check_one == b'M' as i32 && check_two == b'M' as i32)
            {
                metadata_offsets.push(c.fp() - 6);
                let s = c.read_short() as i64;
                c.skip(s - 8);
            } else if (check_one == 0xff || check_one == -1)
                && (check_two == 0xff || check_two == -1)
            {
                // Variable-length metadata block: find the next II/MM marker.
                let mut m: Option<usize> = None;
                let mut p = c.fp();
                while p + 1 < total {
                    if is_marker(data[p], data[p + 1]) {
                        m = Some(p);
                        break;
                    }
                    p += 1;
                }
                match m {
                    Some(m) if m >= 2 => {
                        c.seek(m - 2);
                        metadata_offsets.push(m.saturating_sub(4));
                        let s = c.read_short() as i64;
                        c.skip(s - 5);
                    }
                    _ => break, // no further markers: stop scanning
                }
            } else {
                // Candidate pixel block.
                let mut fp = c.fp() as i64 - 6;
                if fp < 0 {
                    break;
                }
                c.seek(fp as usize);
                let blen = c.read();
                let s = if blen > 0 && blen <= 32 {
                    Some(c.read_string(blen as usize))
                } else {
                    None
                };

                if let Some(s) = s.as_deref().filter(|s| s.contains("Annotation")) {
                    match s {
                        "CTimelapseAnnotation" => {
                            c.skip(41);
                            if c.read() == 0 {
                                c.skip(10);
                            } else {
                                c.skip(-1);
                            }
                        }
                        "CIntensityBarAnnotation" => {
                            c.skip(56);
                            let mut n = c.read();
                            while n == 0 || n < 6 || n > 0x80 {
                                if n == -1 {
                                    break;
                                }
                                n = c.read();
                            }
                            c.skip(-1);
                        }
                        "CCubeAnnotation" => {
                            c.skip(66);
                            let n = c.read();
                            if n != 0 {
                                c.skip(-1);
                            }
                        }
                        "CScaleBarAnnotation" => {
                            c.skip(38);
                            let extra = c.read();
                            if extra <= 16 {
                                c.skip(3 + extra as i64);
                            } else {
                                c.skip(2);
                            }
                        }
                        _ => {}
                    }
                } else if s.as_deref().map(|s| s.contains("Decon")).unwrap_or(false) {
                    c.seek(fp as usize);
                    loop {
                        let b = c.read();
                        if b == b']' as i32 || b == -1 {
                            break;
                        }
                    }
                } else {
                    if fp % 2 == 1 {
                        fp -= 2;
                    }
                    c.seek(fp as usize);
                    let check_string = c.read_string(64);
                    let idx = check_string.find("II").or_else(|| check_string.find("MM"));
                    if let Some(index) = idx {
                        c.seek((fp + index as i64 - 4).max(0) as usize);
                        continue;
                    } else {
                        c.seek(fp as usize);
                    }

                    pixel_offsets.push(fp as usize);

                    // Confirm the block by scanning forward for the identifier
                    // (h/i/j/k/n/on) followed by an II/MM marker.
                    let start = fp as usize;
                    let mut found = false;
                    let mut a = start;
                    while a + 6 <= total {
                        let m4 = data[a + 4];
                        let m5 = data[a + 5];
                        if is_marker(m4, m5) {
                            let b0 = data[a];
                            let b1 = data[a + 1];
                            if ((b0 == b'h' || b0 == b'i') && b1 == 0)
                                || (b0 == 0 && (b1 == b'h' || b1 == b'i'))
                            {
                                found = true;
                                if b0 == b'i' || b1 == b'i' {
                                    pixel_offsets.pop();
                                }
                                c.seek(a.saturating_sub(20));
                                break;
                            } else if ((b0 == b'j' || b0 == b'k' || b0 == b'n') && b1 == 0)
                                || (b0 == 0 && (b1 == b'j' || b1 == b'k' || b1 == b'n'))
                                || (b0 == b'o' && b1 == b'n')
                            {
                                found = true;
                                pixel_offsets.pop();
                                c.seek(a.saturating_sub(20));
                                break;
                            }
                        }
                        a += 1;
                    }
                    if !found {
                        c.seek(total);
                    }

                    // Compute and validate the pixel block length.
                    if pixel_offsets.len() > pixel_lengths.len() {
                        let mut length = c.fp() as i64 - fp;
                        if (length / 2) % 2 == 1 {
                            if let Some(last) = pixel_offsets.last_mut() {
                                *last = (fp + 2) as usize;
                            }
                            length -= 2;
                        }
                        if length >= 1024 {
                            pixel_lengths.push(length.max(0) as usize);
                        } else {
                            pixel_offsets.pop();
                        }
                    }
                }
            }
        }

        Ok((little, metadata_offsets, pixel_offsets, pixel_lengths))
    }

    fn parse(path: &Path) -> Result<Vec<SlideBookSeries>> {
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        let (little, metadata_offsets, mut pixel_offsets, mut pixel_lengths) =
            Self::scan_offsets(&data)?;

        // Drop pixel blocks that run off the end of the file (padding = 7 for
        // non-spool .sld files).
        let mut i = 0;
        while i < pixel_offsets.len() {
            let length = pixel_lengths.get(i).copied().unwrap_or(0);
            let offset = pixel_offsets[i];
            if length + offset + 7 > data.len() {
                pixel_offsets.remove(i);
                if i < pixel_lengths.len() {
                    pixel_lengths.remove(i);
                }
            } else {
                i += 1;
            }
        }

        let n_pix = pixel_offsets.len();
        if n_pix == 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "3i SlideBook: no pixel data blocks found".into(),
            ));
        }

        // Recover per-block dimensions from 'i' and 'u' metadata blocks.
        let mut size_x = vec![0i32; n_pix];
        let mut size_y = vec![0i32; n_pix];
        let mut size_z = vec![0i32; n_pix];
        let mut size_c = vec![0i32; n_pix];
        let mut div_values = vec![0i32; n_pix];

        let mut c = Cursor::new(&data, little);
        let mut i_count = 0i32;
        let mut u_count = 0i32;
        let mut prev_series = -1i32;
        let mut prev_series_u = -1i32;
        let total = data.len();

        for mi in 0..metadata_offsets.len() {
            let off = metadata_offsets[mi];
            let next = if mi == metadata_offsets.len() - 1 {
                total
            } else {
                metadata_offsets[mi + 1]
            };
            if next <= off {
                continue;
            }
            let total_blocks = (next - off) / 128;
            for q in 0..total_blocks {
                let blk = off + q * 128;
                if Self::within_pixels(blk, &pixel_offsets, &pixel_lengths) {
                    continue;
                }
                c.seek(blk);
                let mut n = c.read_short() as u16;
                while n == 0 && c.fp() < off + (q + 1) * 128 {
                    n = c.read_short() as u16;
                }
                if c.fp() >= total.saturating_sub(2) {
                    break;
                }
                let n = n as u8 as char;
                if n == 'i' {
                    i_count += 1;
                    c.skip(70);
                    let _exp = c.read_int();
                    c.skip(20);
                    let _size = c.read_float();
                    c.skip(-20);
                    for j in 0..n_pix {
                        let end = if j == n_pix - 1 {
                            total
                        } else {
                            pixel_offsets[j + 1]
                        };
                        if c.fp() < end {
                            if size_x[j] == 0 {
                                let x = c.read_short() as i32;
                                let y = c.read_short() as i32;
                                if x != 0 && y != 0 {
                                    size_x[j] = x;
                                    size_y[j] = y;
                                    let check_x = c.read_short() as i32;
                                    let check_y = c.read_short() as i32;
                                    let mut div = c.read_short() as i32;
                                    if check_x == check_y {
                                        div_values[j] = div;
                                        size_x[j] /= if div == 0 { 1 } else { div };
                                        div = c.read_short() as i32;
                                        size_y[j] /= if div == 0 { 1 } else { div };
                                    }
                                } else {
                                    c.skip(8);
                                }
                            }
                            if prev_series != j as i32 {
                                i_count = 1;
                            }
                            prev_series = j as i32;
                            size_c[j] = i_count;
                            break;
                        }
                    }
                } else if n == 'u' {
                    u_count += 1;
                    for j in 0..n_pix {
                        let end = if j == n_pix - 1 {
                            total
                        } else {
                            pixel_offsets[j + 1]
                        };
                        if c.fp() < end {
                            if prev_series_u != j as i32 {
                                u_count = 1;
                            }
                            prev_series_u = j as i32;
                            size_z[j] = u_count;
                            break;
                        }
                    }
                }
            }
        }

        // Build one series per pixel block with a clean dimension factoring.
        let mut series = Vec::with_capacity(n_pix);
        for idx in 0..n_pix {
            let sx = size_x[idx];
            let sy = size_y[idx];
            if sx <= 0 || sy <= 0 {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "3i SlideBook: could not recover dimensions for series {idx}"
                )));
            }
            let _ = div_values[idx];
            let sx = sx as u32;
            let sy = sy as u32;
            let plane_bytes = (sx as usize) * (sy as usize) * 2;
            if plane_bytes == 0 {
                return Err(BioFormatsError::Format(
                    "3i SlideBook: zero-sized plane".into(),
                ));
            }
            let length = pixel_lengths.get(idx).copied().unwrap_or(0);
            let plane_count = length / plane_bytes;
            if plane_count == 0 {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "3i SlideBook: series {idx} pixel block holds no full planes"
                )));
            }

            let mut sc = size_c[idx].max(1) as u32;
            let mut sz = size_z[idx].max(1) as u32;
            let product = (sc as usize) * (sz as usize);
            if product == 0 || plane_count % product != 0 {
                // Cannot factor cleanly into C*Z; fall back to a plain Z-stack.
                sc = 1;
                sz = 1;
            }
            let nplanes = (sc * sz).max(1);
            let size_t = (plane_count as u32 / nplanes).max(1);
            let image_count = (nplanes * size_t).min(plane_count as u32).max(1);

            let start = pixel_offsets[idx];
            let mut plane_offsets = Vec::with_capacity(image_count as usize);
            for p in 0..image_count as usize {
                plane_offsets.push(start + p * plane_bytes);
            }

            let meta = ImageMetadata {
                size_x: sx,
                size_y: sy,
                size_z: sz,
                size_c: sc,
                size_t,
                pixel_type: PixelType::Uint16,
                bits_per_pixel: 16,
                image_count,
                dimension_order: DimensionOrder::XYZTC,
                is_rgb: false,
                is_interleaved: false,
                is_indexed: false,
                is_little_endian: little,
                ..ImageMetadata::default()
            };
            series.push(SlideBookSeries {
                meta,
                plane_offsets,
                plane_bytes,
            });
        }
        Ok(series)
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
        if header.len() < 10 {
            return false;
        }
        let little = &header[4..6] == b"II";
        let rd = |o: usize| -> i32 {
            let a = [header[o], header[o + 1]];
            (if little {
                u16::from_le_bytes(a)
            } else {
                u16::from_be_bytes(a)
            }) as i32
        };
        let magic1 = rd(6);
        let magic2 = rd(8);
        ((magic2 & 0xff00) == 0x0100 || (magic2 & 0xff00) == 0x0200)
            && (magic1 == 0x006c || magic1 == 0x01f5)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let series = Self::parse(path)?;
        self.path = Some(path.to_path_buf());
        self.series = series;
        self.current = 0;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.series.clear();
        self.current = 0;
        Ok(())
    }

    fn series_count(&self) -> usize {
        self.series.len()
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if s < self.series.len() {
            self.current = s;
            Ok(())
        } else {
            Err(BioFormatsError::SeriesOutOfRange(s))
        }
    }

    fn series(&self) -> usize {
        self.current
    }

    fn metadata(&self) -> &ImageMetadata {
        self.series
            .get(self.current)
            .map(|s| &s.meta)
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let path = self.path.clone().ok_or(BioFormatsError::NotInitialized)?;
        let s = self
            .series
            .get(self.current)
            .ok_or(BioFormatsError::NotInitialized)?;
        let no = plane_index as usize;
        if no >= s.plane_offsets.len() {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let (offset, plane_bytes) = (s.plane_offsets[no], s.plane_bytes);
        let data = std::fs::read(&path).map_err(BioFormatsError::Io)?;
        let end = offset
            .checked_add(plane_bytes)
            .ok_or_else(|| BioFormatsError::Format("3i SlideBook plane offset overflows".into()))?;
        if end > data.len() {
            return Err(BioFormatsError::InvalidData(
                "3i SlideBook plane extends past end of file".into(),
            ));
        }
        Ok(data[offset..end].to_vec())
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
        let meta = self.metadata().clone();
        crop_full_plane("3i SlideBook", &full, &meta, 1, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let (sx, sy) = {
            let m = self.metadata();
            (m.size_x, m.size_y)
        };
        let tw = sx.min(256);
        let th = sy.min(256);
        let tx = (sx - tw) / 2;
        let ty = (sy - th) / 2;
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
        use hdf5_pure_rust::format::messages::datatype::DatatypeClass;

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

        let file = hdf5_pure_rust::File::open(path)
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
            ds.attr_names().ok().and_then(|names| {
                if names.iter().any(|n| n == "_Unsigned") {
                    ds.attr("_Unsigned").ok().map(|attr| {
                        // true => unsigned; anything else (the attribute is
                        // present but not "true...") => signed.
                        attr.read_string()
                            .trim_start_matches(['"', '\''])
                            .to_ascii_lowercase()
                            .starts_with("true")
                    })
                } else {
                    None
                }
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
        // TODO: per-plane read_slice. The MincReader caches the whole volume in
        // self.pixel_data during set_id and serves planes by byte offset in
        // open_bytes; the plane index is not available here, so we read the
        // entire volume up front (preserving the original behaviour).
        let (pixel_type, bits, pixels): (PixelType, u8, Vec<u8>) =
            match (dtype.class(), dtype.size()) {
                (DatatypeClass::FixedPoint, 1) => {
                    let raw = ds
                        .read::<u8>()
                        .map_err(|e| BioFormatsError::Format(format!("MINC/HDF5 read: {e}")))?;
                    let pt = if signed {
                        PixelType::Int8
                    } else {
                        PixelType::Uint8
                    };
                    (pt, 8, raw)
                }
                (DatatypeClass::FixedPoint, 2) => {
                    let raw = ds
                        .read::<i16>()
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
                (DatatypeClass::FixedPoint, 4) => {
                    let raw = ds
                        .read::<i32>()
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
                (DatatypeClass::FloatingPoint, 4) => {
                    let raw = ds
                        .read::<f32>()
                        .map_err(|e| BioFormatsError::Format(format!("MINC/HDF5 read: {e}")))?;
                    let mut bytes = Vec::with_capacity(raw.len() * 4);
                    for v in &raw {
                        bytes.extend_from_slice(&v.to_le_bytes());
                    }
                    (PixelType::Float32, 32, bytes)
                }
                (DatatypeClass::FloatingPoint, 8) => {
                    let raw = ds
                        .read::<f64>()
                        .map_err(|e| BioFormatsError::Format(format!("MINC/HDF5 read: {e}")))?;
                    let mut bytes = Vec::with_capacity(raw.len() * 8);
                    for v in &raw {
                        bytes.extend_from_slice(&v.to_le_bytes());
                    }
                    (PixelType::Float64, 64, bytes)
                }
                (class, size) => {
                    return Err(BioFormatsError::UnsupportedFormat(format!(
                        "MINC/HDF5: unsupported image datatype {class:?} ({size} bytes)"
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
/// Improvision Openlab LIFF reader (`.liff`).
///
/// Faithful port of the Java `OpenlabReader` block/layer parsing and the
/// uncompressed / LZO pixel-read paths. The file is a big-endian sequence of
/// tagged blocks (`readTagHeader`): each `IMAGE_TYPE_1`/`IMAGE_TYPE_2` tag
/// carries a plane (volume type, name, dimensions and pixel offset). Planes are
/// grouped into series by matching width/height/volume-type against previously
/// seen "representative" planes, exactly as the Java reader does.
///
/// Pixel reads cover the documented cases:
/// - **version 2**: raw uncompressed planes;
/// - **version 5**: LZO-compressed planes (8-bit greyscale and 24-bit colour),
///   decoded with [`crate::common::codec::decompress_lzo`] and unpacked using
///   the same stride logic as Java's `openBytes`.
/// - embedded Apple PICT bitmap/pixmap payloads through the shared PICT reader.
///
/// MAC_256 greyscale/colour planes are bit-inverted as in Java.
///
/// NOT PORTED: richer OME stage/detector metadata is omitted.
const OPENLAB_LIFF_MAGIC: u64 = 0x0000_ffff_696d_7072;

// Openlab image (volume) types.
const OL_MAC_1_BIT: i32 = 1;
const OL_MAC_4_GREYS: i32 = 2;
const OL_MAC_16_GREYS: i32 = 3;
const OL_MAC_16_COLORS: i32 = 4;
const OL_MAC_256_GREYS: i32 = 5;
const OL_MAC_256_COLORS: i32 = 6;
const OL_MAC_16_BIT_COLOR: i32 = 7;
const OL_MAC_24_BIT_COLOR: i32 = 8;
const OL_DEEP_GREY_9: i32 = 9;
const OL_DEEP_GREY_16: i32 = 16;

// Tag types.
const OL_IMAGE_TYPE_1: i32 = 67;
const OL_IMAGE_TYPE_2: i32 = 68;
const OL_CALIBRATION: i32 = 69;

fn parse_axis_token(token: &str, axis: char) -> Option<u32> {
    let mut chars = token.chars();
    if chars.next()? != axis {
        return None;
    }
    chars.as_str().parse::<u32>().ok().filter(|v| *v > 0)
}

#[derive(Clone)]
struct OpenlabPlane {
    plane_offset: usize,
    volume_type: i32,
    pict: bool,
    width: u32,
    height: u32,
    name: String,
    series: i32,
}

pub struct OpenlabLiffReader {
    path: Option<PathBuf>,
    version: i32,
    planes: Vec<OpenlabPlane>,
    /// Per-series list of indices into `planes`.
    plane_offsets: Vec<Vec<usize>>,
    /// Optional per-series Z/C/T coordinates inferred from plane names.
    plane_zct: Vec<Option<Vec<(u32, u32, u32)>>>,
    metas: Vec<ImageMetadata>,
    current: usize,
}

impl OpenlabLiffReader {
    pub fn new() -> Self {
        OpenlabLiffReader {
            path: None,
            version: 0,
            planes: Vec::new(),
            plane_offsets: Vec::new(),
            plane_zct: Vec::new(),
            metas: Vec::new(),
            current: 0,
        }
    }

    fn volume_type_name(volume_type: i32) -> &'static str {
        match volume_type {
            OL_MAC_1_BIT => "MAC_1_BIT",
            OL_MAC_4_GREYS => "MAC_4_GREYS",
            OL_MAC_16_GREYS => "MAC_16_GREYS",
            OL_MAC_16_COLORS => "MAC_16_COLORS",
            OL_MAC_256_GREYS => "MAC_256_GREYS",
            OL_MAC_256_COLORS => "MAC_256_COLORS",
            OL_MAC_16_BIT_COLOR => "MAC_16_BIT_COLOR",
            OL_MAC_24_BIT_COLOR => "MAC_24_BIT_COLOR",
            OL_DEEP_GREY_9 => "DEEP_GREY_9",
            10 => "DEEP_GREY_10",
            11 => "DEEP_GREY_11",
            12 => "DEEP_GREY_12",
            13 => "DEEP_GREY_13",
            14 => "DEEP_GREY_14",
            15 => "DEEP_GREY_15",
            OL_DEEP_GREY_16 => "DEEP_GREY_16",
            _ => "UNKNOWN",
        }
    }

    fn infer_name_axes(names: &[String]) -> Option<(String, Vec<(u32, u32, u32)>, u32, u32, u32)> {
        let mut coords = Vec::with_capacity(names.len());
        let mut image_name: Option<String> = None;
        let mut max_z = 0u32;
        let mut max_c = 0u32;
        let mut max_t = 0u32;

        for name in names {
            let tokens: Vec<&str> = name.split_whitespace().collect();
            if tokens.len() < 4 {
                return None;
            }
            let z = parse_axis_token(tokens[tokens.len() - 3], 'Z')?;
            let c = parse_axis_token(tokens[tokens.len() - 2], 'C')?;
            let t = parse_axis_token(tokens[tokens.len() - 1], 'T')?;
            let prefix = tokens[..tokens.len() - 3].join(" ");
            if prefix.is_empty() {
                return None;
            }
            match &image_name {
                Some(existing) if existing != &prefix => return None,
                None => image_name = Some(prefix),
                _ => {}
            }
            let z0 = z.checked_sub(1)?;
            let c0 = c.checked_sub(1)?;
            let t0 = t.checked_sub(1)?;
            max_z = max_z.max(z);
            max_c = max_c.max(c);
            max_t = max_t.max(t);
            coords.push((z0, c0, t0));
        }

        Some((image_name?, coords, max_z, max_c, max_t))
    }

    /// Read one tag header (Java `readTagHeader`). Returns
    /// (tag, sub_tag, next_tag, fmt).
    fn read_tag_header(c: &mut Cursor, version: i32) -> (i32, i32, i64, String) {
        let tag = c.read_short() as i32;
        let sub_tag = c.read_short() as i32;
        let next_tag = if version == 2 {
            c.read_int() as i64
        } else {
            c.read_long()
        };
        let fmt = c.read_string(4);
        c.skip(if version == 2 { 4 } else { 8 });
        (tag, sub_tag, next_tag, fmt)
    }

    fn parse(path: &Path) -> Result<OpenlabLiffReader> {
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        let mut c = Cursor::new(&data, false); // big-endian

        c.seek(4);
        if c.read_string(4) != "impr" {
            return Err(BioFormatsError::Format("Invalid LIFF file.".into()));
        }
        let version = c.read_int();
        if version != 2 && version != 5 {
            return Err(BioFormatsError::Format(format!(
                "Invalid Openlab LIFF version : {version}"
            )));
        }
        let _plane_count = c.read_short();
        c.skip(2); // ID seed
        let first_offset = c.read_int();
        c.seek(first_offset.max(0) as usize);

        let mut planes: Vec<OpenlabPlane> = Vec::new();
        // Representative planes: (width, height, volume_type).
        let mut reps: Vec<(u32, u32, i32)> = Vec::new();
        let mut xcal = 0.0f32;
        let mut ycal = 0.0f32;
        let total = data.len();

        while c.fp() + 8 < total {
            let mut fp = c.fp() as i64;
            let (mut tag, mut sub_tag, mut next_tag, mut fmt) =
                Self::read_tag_header(&mut c, version);
            // Resync: back up one byte at a time until the tag is in range.
            while (tag < OL_IMAGE_TYPE_1 || tag > 76) && fp > 0 {
                fp -= 1;
                c.seek(fp as usize);
                let h = Self::read_tag_header(&mut c, version);
                tag = h.0;
                sub_tag = h.1;
                next_tag = h.2;
                fmt = h.3;
            }
            if tag < OL_IMAGE_TYPE_1 || tag > 76 {
                break; // could not resync
            }
            let _ = sub_tag;

            if tag == OL_IMAGE_TYPE_1 || tag == OL_IMAGE_TYPE_2 {
                let pict = fmt.to_lowercase() == "pict";
                c.skip(24);
                let volume_type = c.read_short() as i32;
                c.skip(16);
                let pointer = c.fp();
                let name = c.read_cstring().trim().to_string();
                c.skip(256 - c.fp() as i64 + pointer as i64);
                let plane_offset = c.fp();

                let (width, height) = if version == 2 {
                    c.skip(2);
                    let top = c.read_short() as i32;
                    let left = c.read_short() as i32;
                    let bottom = c.read_short() as i32;
                    let right = c.read_short() as i32;
                    ((right - left).max(0) as u32, (bottom - top).max(0) as u32)
                } else {
                    (c.read_int().max(0) as u32, c.read_int().max(0) as u32)
                };

                let mut series = -1i32;
                for i in (0..reps.len()).rev() {
                    let (rw, rh, rv) = reps[i];
                    if width == rw
                        && height == rh
                        && (volume_type == rv
                            || (volume_type >= OL_DEEP_GREY_9 && rv >= OL_DEEP_GREY_9))
                    {
                        series = i as i32;
                        break;
                    }
                }
                if series == -1 && name != "Original Image" {
                    series = reps.len() as i32;
                    reps.push((width, height, volume_type));
                }

                planes.push(OpenlabPlane {
                    plane_offset,
                    volume_type,
                    pict,
                    width,
                    height,
                    name,
                    series,
                });
            } else if tag == OL_CALIBRATION {
                c.skip(4);
                let units = c.read_short() as i32;
                let scaling = if units == 3 { 0.001f32 } else { 1.0f32 };
                c.skip(12);
                xcal = c.read_float() * scaling;
                ycal = c.read_float() * scaling;
            }

            if next_tag <= fp || next_tag as usize > total {
                // Avoid looping forever on a malformed / final block.
                if next_tag <= 0 {
                    break;
                }
            }
            c.seek(next_tag.max(0) as usize);
        }

        let n_series = reps.len();
        if n_series == 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "Openlab LIFF: no image planes found".into(),
            ));
        }

        // Group plane indices by series.
        let mut plane_offsets: Vec<Vec<usize>> = vec![Vec::new(); n_series];
        for (q, p) in planes.iter().enumerate() {
            if p.series >= 0 && (p.series as usize) < n_series {
                plane_offsets[p.series as usize].push(q);
            }
        }

        let mut metas = Vec::with_capacity(n_series);
        let mut plane_zct = Vec::with_capacity(n_series);
        for (i, list) in plane_offsets.iter().enumerate() {
            if list.is_empty() {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "Openlab LIFF: series {i} has no planes"
                )));
            }
            let first = &planes[list[0]];
            let mut meta = ImageMetadata::default();
            meta.size_x = first.width;
            meta.size_y = first.height;
            meta.image_count = list.len() as u32;
            meta.size_c = 1;
            let mut bits = 8u8;
            match first.volume_type {
                v if v == OL_MAC_1_BIT || v == OL_MAC_4_GREYS || v == OL_MAC_256_GREYS => {
                    meta.pixel_type = PixelType::Uint8;
                    meta.is_indexed = first.pict;
                }
                v if v == OL_MAC_256_COLORS => {
                    meta.pixel_type = PixelType::Uint8;
                    meta.is_indexed = true;
                }
                v if v == OL_MAC_16_COLORS
                    || v == OL_MAC_16_BIT_COLOR
                    || v == OL_MAC_24_BIT_COLOR =>
                {
                    meta.pixel_type = PixelType::Uint8;
                    meta.size_c = 3;
                }
                v if (OL_DEEP_GREY_9..OL_DEEP_GREY_16).contains(&v) => {
                    bits = v as u8; // 9..15
                    meta.pixel_type = PixelType::Uint16;
                }
                v if v == OL_MAC_16_GREYS || v == OL_DEEP_GREY_16 => {
                    bits = 16;
                    meta.pixel_type = PixelType::Uint16;
                }
                other => {
                    return Err(BioFormatsError::UnsupportedFormat(format!(
                        "Openlab LIFF: unsupported plane type {other}"
                    )));
                }
            }
            if bits > 8 {
                meta.pixel_type = PixelType::Uint16;
            }
            meta.bits_per_pixel = if meta.pixel_type == PixelType::Uint16 {
                bits.max(9)
            } else {
                8
            };
            meta.is_rgb = meta.size_c > 1;
            meta.is_interleaved = meta.is_rgb && version == 5;
            meta.size_t = 1;
            meta.size_z = meta.image_count;
            meta.dimension_order = DimensionOrder::XYCZT;
            meta.is_little_endian = false;
            meta.series_metadata
                .insert("openlab.version".into(), MetadataValue::Int(version as i64));
            meta.series_metadata.insert(
                "openlab.volume_type".into(),
                MetadataValue::Int(first.volume_type as i64),
            );
            meta.series_metadata.insert(
                "openlab.volume_type_name".into(),
                MetadataValue::String(Self::volume_type_name(first.volume_type).into()),
            );
            meta.series_metadata.insert(
                "openlab.pixel_payload".into(),
                MetadataValue::String(if first.pict {
                    "pict".into()
                } else if version == 5 {
                    "lzo".into()
                } else {
                    "raw".into()
                }),
            );
            if i == 0 {
                if xcal != 0.0 {
                    meta.series_metadata.insert(
                        "openlab.physical_size_x".into(),
                        MetadataValue::Float(xcal as f64),
                    );
                }
                if ycal != 0.0 {
                    meta.series_metadata.insert(
                        "openlab.physical_size_y".into(),
                        MetadataValue::Float(ycal as f64),
                    );
                }
            }
            let names: Vec<String> = list.iter().map(|&idx| planes[idx].name.clone()).collect();
            let inferred = Self::infer_name_axes(&names);
            if let Some((image_name, coords, size_z, size_c, size_t)) = inferred {
                meta.size_z = size_z;
                meta.size_c = size_c;
                meta.size_t = size_t;
                meta.image_count = list.len() as u32;
                meta.series_metadata.insert(
                    "openlab.image_name".into(),
                    MetadataValue::String(image_name),
                );
                meta.series_metadata.insert(
                    "openlab.image_name_zct_inference".into(),
                    MetadataValue::Bool(true),
                );
                plane_zct.push(Some(coords));
            } else {
                plane_zct.push(None);
            }
            metas.push(meta);
        }

        Ok(OpenlabLiffReader {
            path: Some(path.to_path_buf()),
            version,
            planes,
            plane_offsets,
            plane_zct,
            metas,
            current: 0,
        })
    }

    /// Read and decode a plane (full image).
    fn read_plane(
        &self,
        data: &[u8],
        plane: &OpenlabPlane,
        meta: &ImageMetadata,
    ) -> Result<Vec<u8>> {
        if plane.pict {
            let start = plane
                .plane_offset
                .checked_add(10)
                .ok_or_else(|| BioFormatsError::Format("Openlab PICT offset overflows".into()))?;
            if start >= data.len() {
                return Err(BioFormatsError::Format(
                    "Openlab LIFF PICT payload is missing".into(),
                ));
            }
            return crate::formats::legacy::parse_pict_bytes(&data[start..])
                .map(|decoded| decoded.pixels);
        }
        let w = meta.size_x as usize;
        let h = meta.size_y as usize;
        let bpp = meta.pixel_type.bytes_per_sample();
        let channels = if meta.is_rgb { meta.size_c as usize } else { 1 };
        let plane_size = w * h * bpp * channels;
        let first = plane.plane_offset;

        let mut buf: Vec<u8> = if self.version == 2 {
            let end = first
                .checked_add(plane_size)
                .ok_or_else(|| BioFormatsError::Format("Openlab plane offset overflows".into()))?;
            if end > data.len() {
                return Err(BioFormatsError::InvalidData(
                    "Openlab LIFF plane extends past end of file".into(),
                ));
            }
            data[first..end].to_vec()
        } else {
            // version 5: LZO-compressed.
            let last = first + plane_size * 2;
            let comp_start = (first + 16).min(data.len());
            let comp_end = last.min(data.len());
            let comp = &data[comp_start..comp_end.max(comp_start)];
            let b = crate::common::codec::decompress_lzo(comp)
                .map_err(|e| BioFormatsError::Codec(format!("Openlab LZO: {e}")))?;

            if w * h * 4 <= b.len() {
                // 32-bit ARGB-ish source with a (w + 4) stride; emit RGB.
                let mut out = vec![0u8; w * h * 3];
                for yy in 0..h {
                    for xx in 0..w {
                        let src = (yy * (w + 4) + xx) * 4 + 1;
                        if src + 3 <= b.len() {
                            let dst = (yy * w + xx) * 3;
                            out[dst..dst + 3].copy_from_slice(&b[src..src + 3]);
                        }
                    }
                }
                out
            } else {
                let mut src = if h > 0 { b.len() / h } else { 0 };
                if src as i64 - (w * bpp) as i64 != 16 {
                    src = w * bpp;
                }
                let dest = w * bpp;
                let mut out = vec![0u8; h * dest];
                for row in 0..h {
                    let s = row * src;
                    if s + dest <= b.len() {
                        out[row * dest..row * dest + dest].copy_from_slice(&b[s..s + dest]);
                    }
                }
                out
            }
        };

        if plane.volume_type == OL_MAC_256_GREYS || plane.volume_type == OL_MAC_256_COLORS {
            for byte in buf.iter_mut() {
                *byte = !*byte;
            }
        }
        Ok(buf)
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
        header.len() >= 8
            && u64::from_be_bytes(header[..8].try_into().unwrap()) == OPENLAB_LIFF_MAGIC
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        *self = Self::parse(path)?;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.version = 0;
        self.planes.clear();
        self.plane_offsets.clear();
        self.plane_zct.clear();
        self.metas.clear();
        self.current = 0;
        Ok(())
    }

    fn series_count(&self) -> usize {
        self.metas.len()
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if s < self.metas.len() {
            self.current = s;
            Ok(())
        } else {
            Err(BioFormatsError::SeriesOutOfRange(s))
        }
    }

    fn series(&self) -> usize {
        self.current
    }

    fn metadata(&self) -> &ImageMetadata {
        self.metas
            .get(self.current)
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let path = self.path.clone().ok_or(BioFormatsError::NotInitialized)?;
        let no = plane_index as usize;
        let list = self
            .plane_offsets
            .get(self.current)
            .ok_or(BioFormatsError::NotInitialized)?;
        if no >= list.len() {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let plane = self.planes[list[no]].clone();
        let meta = self.metas[self.current].clone();
        let data = std::fs::read(&path).map_err(BioFormatsError::Io)?;
        self.read_plane(&data, &plane, &meta)
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
        let meta = self.metadata().clone();
        let channels = if meta.is_rgb { meta.size_c as usize } else { 1 };
        if meta.is_interleaved {
            crop_full_plane("Openlab LIFF", &full, &meta, channels, x, y, w, h)
        } else {
            crop_planar("Openlab LIFF", &full, &meta, channels, x, y, w, h)
        }
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let (sx, sy) = {
            let m = self.metadata();
            (m.size_x, m.size_y)
        };
        let tw = sx.min(256);
        let th = sy.min(256);
        let tx = (sx - tw) / 2;
        let ty = (sy - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        use crate::common::ome_metadata::{create_lsid, OmeMetadata, OmePlane, OmePlate};

        let meta = self.metas.get(self.current)?;
        let mut ome = OmeMetadata::from_image_metadata(meta);
        if let Some(image) = ome.images.get_mut(0) {
            if let Some(MetadataValue::String(name)) =
                meta.series_metadata.get("openlab.image_name")
            {
                image.name = Some(name.clone());
            }
            if let Some(MetadataValue::Float(v)) =
                meta.series_metadata.get("openlab.physical_size_x")
            {
                image.physical_size_x = Some(*v);
            }
            if let Some(MetadataValue::Float(v)) =
                meta.series_metadata.get("openlab.physical_size_y")
            {
                image.physical_size_y = Some(*v);
            }
            if let Some(Some(coords)) = self.plane_zct.get(self.current) {
                image.planes = coords
                    .iter()
                    .map(|&(the_z, the_c, the_t)| OmePlane {
                        the_z,
                        the_c,
                        the_t,
                        ..Default::default()
                    })
                    .collect();
            }
        }

        if let Some(MetadataValue::String(name)) = meta.series_metadata.get("openlab.image_name") {
            if let Some(plate_name) = name.split_whitespace().next() {
                if !plate_name.is_empty() {
                    ome.plates.push(OmePlate {
                        id: Some(create_lsid("Plate", &[0])),
                        name: Some(plate_name.to_string()),
                        ..Default::default()
                    });
                }
            }
        }
        let _ = ome.add_original_metadata_annotations(meta, 0);
        Some(ome)
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
