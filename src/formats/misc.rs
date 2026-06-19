//! Placeholder readers for miscellaneous / proprietary formats.
//!
//! Extension-only placeholder readers return `UnsupportedFormat` instead of
//! exposing synthetic metadata or zero-filled planes. Partial readers in this
//! module only decode documented/simple payload cases.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::common::codec::decompress_rpza;
use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::ome_metadata::{OmeMetadata, OmePlane};
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

    fn read_double(&mut self) -> f64 {
        f64::from_bits(self.read_u64())
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
pub struct QtReader {
    path: Option<PathBuf>,
    series: Vec<QuickTimeParsed>,
    current_series: usize,
}

impl QtReader {
    pub fn new() -> Self {
        QtReader {
            path: None,
            series: Vec::new(),
            current_series: 0,
        }
    }
}

impl Default for QtReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for QtReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("mov") | Some("qt"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        if header.len() >= 12 && (&header[4..8] == b"ftyp" || &header[4..8] == b"moov") {
            return true;
        }
        let scan_len = header.len().min(64);
        let scan = &header[..scan_len];
        [
            b"moov".as_slice(),
            b"trak",
            b"udta",
            b"tref",
            b"imap",
            b"mdia",
            b"minf",
            b"stbl",
            b"edts",
            b"mdra",
            b"rmra",
            b"vnrp",
            b"dinf",
            b"wide",
            b"mdat",
            b"ftypqt",
        ]
        .iter()
        .any(|needle| scan.windows(needle.len()).any(|window| window == *needle))
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
        let sample_index = series
            .sample_read_order
            .as_ref()
            .and_then(|order| order.get(index).copied())
            .unwrap_or(index);
        let offset = series.sample_offsets[sample_index];
        let sample_size = series.sample_sizes[sample_index] as usize;
        let data = std::fs::read(self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?)
            .map_err(BioFormatsError::Io)?;
        let start = offset as usize;
        let end = start
            .checked_add(sample_size)
            .ok_or_else(|| BioFormatsError::Format("QuickTime sample offset overflows".into()))?;
        if end > data.len() {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "QuickTime sample {sample_index} extends past end of file"
            )));
        }
        let sample = &data[start..end];
        let sample_codec = series
            .sample_codecs
            .get(sample_index)
            .copied()
            .unwrap_or(series.codec);
        match sample_codec {
            QuickTimeCodec::UncompressedRgb | QuickTimeCodec::UncompressedGray => {
                let sample_depth = series
                    .sample_depths
                    .get(sample_index)
                    .copied()
                    .unwrap_or(series.depth);
                decode_quicktime_uncompressed_sample(
                    sample,
                    meta,
                    sample_index as u32,
                    sample_codec,
                    sample_depth,
                )
            }
            QuickTimeCodec::Jpeg => decode_quicktime_jpeg_sample(sample, meta, sample_index as u32),
            QuickTimeCodec::Mjpb => decode_quicktime_mjpb_sample(sample, sample_index as u32),
            QuickTimeCodec::Png => decode_quicktime_png_sample(sample, meta, sample_index as u32),
            QuickTimeCodec::Rpza => {
                // Java QTReader's RPZA branch (QTReader.java lines 204-210) does
                // `t[i] = (byte)(255 - t[i])` on the decoded plane `t`, but then
                // `return buf;` — it returns the (untouched) caller buffer, never
                // copying the inverted `t` into it. So the inversion is dead code
                // with no observable effect on the output, and the un-inverted
                // RPZA pixels are what callers actually receive. Do NOT invert.
                quicktime_decompress_rpza(sample, meta, sample_index as u32)
            }
            QuickTimeCodec::AnimationRle { depth } => {
                let mut previous = None;
                for current in 0..=sample_index {
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
                    previous = Some(quicktime_decompress_qtrle(
                        &data[offset..end],
                        meta,
                        current as u32,
                        depth,
                        previous.as_deref(),
                    )?);
                }
                previous.ok_or(BioFormatsError::PlaneOutOfRange(plane_index))
            }
            QuickTimeCodec::Cinepak { depth } => {
                let mut previous = None;
                for current in 0..=sample_index {
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
                    previous = Some(quicktime_decompress_cinepak(
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

    fn ome_metadata(&self) -> Option<OmeMetadata> {
        let meta = &self.series.get(self.current_series)?.meta;
        let mut ome = OmeMetadata::from_image_metadata(meta);
        if let Some(image) = ome.images.get_mut(0) {
            let delta_t = match meta
                .series_metadata
                .get("quicktime.sample_presentation_time_seconds")
            {
                Some(MetadataValue::String(values)) => values
                    .split(',')
                    .map(|value| value.parse::<f64>().ok())
                    .collect::<Option<Vec<_>>>()
                    .unwrap_or_default(),
                _ => Vec::new(),
            };
            image.planes = (0..meta.image_count)
                .map(|plane| OmePlane {
                    the_z: 0,
                    the_c: 0,
                    the_t: plane,
                    delta_t: delta_t.get(plane as usize).copied(),
                    exposure_time: None,
                    position_x: None,
                    position_y: None,
                    position_z: None,
                })
                .collect();
        }
        let _ = ome.add_original_metadata_annotations(meta, 0);
        Some(ome)
    }
}

struct QuickTimeParsed {
    meta: ImageMetadata,
    sample_offsets: Vec<u64>,
    sample_sizes: Vec<u32>,
    sample_read_order: Option<Vec<usize>>,
    sample_codecs: Vec<QuickTimeCodec>,
    sample_depths: Vec<u16>,
    samples_per_pixel: usize,
    codec: QuickTimeCodec,
    depth: u16,
}

struct QuickTimeSampleDescription {
    codec_fourcc: [u8; 4],
    codec: QuickTimeCodec,
    depth: u16,
    width: u32,
    height: u32,
    samples_per_pixel: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum QuickTimeCodec {
    UncompressedRgb,
    UncompressedGray,
    Jpeg,
    Mjpb,
    Png,
    Cinepak { depth: u16 },
    Rpza,
    AnimationRle { depth: u16 },
}

#[derive(Clone, Copy)]
struct QuickTimeSttsEntry {
    sample_count: u32,
    sample_delta: u32,
}

#[derive(Clone, Copy)]
struct QuickTimeCttsEntry {
    sample_count: u32,
    sample_offset: i64,
}

#[derive(Clone, Copy)]
struct QuickTimeStscEntry {
    first_chunk: u32,
    samples_per_chunk: u32,
    sample_description_index: u32,
}

#[derive(Clone, Copy)]
struct QuickTimeEditEntry {
    segment_duration: u64,
    media_time: i64,
    media_rate: f64,
}

struct QuickTimeEditPresentationMap {
    presentation_times: Vec<i64>,
    media_times: Vec<u64>,
    segment_indices: Vec<u32>,
    sample_indices: Vec<usize>,
}

struct QuickTimeEditPresentationResult {
    presentation_times: Vec<i64>,
    sample_read_order: Option<Vec<usize>>,
}

struct QuickTimeEditListDiagnostic {
    reason: &'static str,
    message: String,
    segment_index: Option<usize>,
    sample_index: Option<usize>,
}

impl QuickTimeEditListDiagnostic {
    fn new(reason: &'static str, message: impl Into<String>) -> Self {
        Self {
            reason,
            message: message.into(),
            segment_index: None,
            sample_index: None,
        }
    }

    fn with_segment(mut self, segment_index: usize) -> Self {
        self.segment_index = Some(segment_index);
        self
    }

    fn with_sample(mut self, sample_index: usize) -> Self {
        self.sample_index = Some(sample_index);
        self
    }
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

/// Inverts every byte of a decoded plane in place (`b = 255 - b`).
///
/// Mirrors the `255 - x` inversion loops in Java `QTReader.openBytes`
/// (RPZA: lines 204-210; 8-bit/grayscale uncompressed: lines 269-274).
fn quicktime_invert_pixels(buf: &mut [u8]) {
    for b in buf.iter_mut() {
        *b = 255u8.wrapping_sub(*b);
    }
}

fn quicktime_codec_from_fourcc(fourcc: &[u8], depth: u16) -> Result<QuickTimeCodec> {
    match fourcc {
        b"raw " | b"RAW " | b"rgb " => Ok(QuickTimeCodec::UncompressedRgb),
        b"gray" | b"GREY" | b"y800" => Ok(QuickTimeCodec::UncompressedGray),
        b"jpeg" | b"mjpa" | b"mjpg" | b"MJPG" => Ok(QuickTimeCodec::Jpeg),
        b"mjpb" => Ok(QuickTimeCodec::Mjpb),
        b"png " => Ok(QuickTimeCodec::Png),
        b"rpza" => Ok(QuickTimeCodec::Rpza),
        b"rle " => {
            let depth = match depth {
                0 | 24 => 24,
                16 | 32 => depth,
                other => {
                    return Err(BioFormatsError::UnsupportedFormat(format!(
                        "QuickTime Animation RLE depth {other} is unsupported"
                    )))
                }
            };
            Ok(QuickTimeCodec::AnimationRle { depth })
        }
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
        other => Err(quicktime_unsupported_codec_error(other)),
    }
}

fn quicktime_samples_per_pixel(codec: QuickTimeCodec) -> usize {
    match codec {
        QuickTimeCodec::UncompressedRgb => 3,
        QuickTimeCodec::UncompressedGray => 1,
        QuickTimeCodec::Jpeg | QuickTimeCodec::Mjpb | QuickTimeCodec::Png => 3,
        QuickTimeCodec::Rpza | QuickTimeCodec::AnimationRle { .. } => 3,
        QuickTimeCodec::Cinepak { depth: 8 } => 1,
        QuickTimeCodec::Cinepak { .. } => 3,
    }
}

fn parse_quicktime_stsd(stsd: Atom<'_>) -> Result<Vec<QuickTimeSampleDescription>> {
    if stsd.data.len() < 8 {
        return Err(BioFormatsError::UnsupportedFormat(
            "QuickTime stsd atom is truncated".into(),
        ));
    }
    let entry_count = be_u32_at(stsd.data, 4).unwrap() as usize;
    if entry_count == 0 {
        return Err(BioFormatsError::UnsupportedFormat(
            "QuickTime stsd contains no video sample descriptions".into(),
        ));
    }
    let mut offset = 8usize;
    let mut descriptions = Vec::with_capacity(entry_count);
    for _ in 0..entry_count {
        if stsd.data.len().saturating_sub(offset) < 86 {
            return Err(BioFormatsError::UnsupportedFormat(
                "QuickTime stsd sample description is truncated".into(),
            ));
        }
        let entry_size = be_u32_at(stsd.data, offset).unwrap_or(0) as usize;
        if entry_size < 86 || offset + entry_size > stsd.data.len() {
            return Err(BioFormatsError::UnsupportedFormat(
                "QuickTime stsd sample description has invalid size".into(),
            ));
        }
        let entry = &stsd.data[offset..offset + entry_size];
        let codec_fourcc = [entry[4], entry[5], entry[6], entry[7]];
        let depth = be_u16_at(entry, 82).unwrap_or(0);
        let codec = quicktime_codec_from_fourcc(&codec_fourcc, depth)?;
        let width = be_u16_at(entry, 32).unwrap_or(0) as u32;
        let height = be_u16_at(entry, 34).unwrap_or(0) as u32;
        if width == 0 || height == 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "QuickTime video sample entry has non-positive dimensions".into(),
            ));
        }
        descriptions.push(QuickTimeSampleDescription {
            codec_fourcc,
            codec,
            depth,
            width,
            height,
            samples_per_pixel: quicktime_samples_per_pixel(codec),
        });
        offset += entry_size;
    }
    Ok(descriptions)
}

fn quicktime_unsupported_codec_error(fourcc: &[u8]) -> BioFormatsError {
    let family = quicktime_codec_family(fourcc);
    let decoder_note = if quicktime_codec_family_requires_external_decoder(fourcc) {
        "Bio-Formats Java delegates this codec family to QuickTime/platform video decoders; bioformats-rs has no external video decoder backend"
    } else {
        "no native bioformats-rs decoder is available"
    };
    BioFormatsError::UnsupportedFormat(format!(
        "QuickTime codec {} is unsupported (family: {family}); {decoder_note}",
        String::from_utf8_lossy(fourcc),
    ))
}

fn quicktime_codec_family(fourcc: &[u8]) -> &'static str {
    match fourcc {
        b"raw " | b"RAW " | b"rgb " => "uncompressed RGB",
        b"gray" | b"GREY" | b"y800" => "uncompressed grayscale",
        b"jpeg" | b"mjpa" | b"mjpb" | b"mjpg" | b"MJPG" => "Motion JPEG",
        b"png " => "PNG",
        b"cvid" => "Cinepak",
        b"rpza" => "Apple Video",
        b"rle " => "Animation RLE",
        b"avc1" | b"avc2" | b"avc3" | b"avc4" | b"h264" | b"H264" | b"x264" | b"X264" => {
            "H.264/AVC"
        }
        b"hvc1" | b"hev1" => "H.265/HEVC",
        b"apch" | b"apcn" | b"apcs" | b"apco" | b"ap4h" | b"ap4x" => "Apple ProRes",
        b"mjp2" | b"mj2k" => "Motion JPEG 2000",
        b"dv  " | b"dvc " | b"dvcp" | b"dvhq" | b"dv25" | b"dv50" | b"dv5n" | b"dv5p" => "DV",
        _ => "unknown codec family",
    }
}

fn quicktime_codec_family_requires_external_decoder(fourcc: &[u8]) -> bool {
    matches!(
        fourcc,
        b"avc1"
            | b"avc2"
            | b"avc3"
            | b"avc4"
            | b"h264"
            | b"H264"
            | b"x264"
            | b"X264"
            | b"hvc1"
            | b"hev1"
            | b"apch"
            | b"apcn"
            | b"apcs"
            | b"apco"
            | b"ap4h"
            | b"ap4x"
            | b"mjp2"
            | b"mj2k"
            | b"dv  "
            | b"dvc "
            | b"dvcp"
            | b"dvhq"
            | b"dv25"
            | b"dv50"
            | b"dv5n"
            | b"dv5p"
    )
}

fn quicktime_insert_edit_list_pixel_order_diagnostic(
    metadata: &mut HashMap<String, MetadataValue>,
    status: &str,
    diagnostic: &str,
) {
    metadata.insert(
        "quicktime.edit_list.pixel_order_status".into(),
        MetadataValue::String(status.into()),
    );
    metadata.insert(
        "quicktime.edit_list.pixel_order_diagnostic".into(),
        MetadataValue::String(diagnostic.into()),
    );
}

fn decode_quicktime_uncompressed_sample(
    sample: &[u8],
    meta: &ImageMetadata,
    plane_index: u32,
    codec: QuickTimeCodec,
    depth: u16,
) -> Result<Vec<u8>> {
    let width = meta.size_x as usize;
    let height = meta.size_y as usize;
    let channels = meta.size_c as usize;
    let expected = width
        .checked_mul(height)
        .and_then(|px| px.checked_mul(channels))
        .and_then(|samples| samples.checked_mul(meta.pixel_type.bytes_per_sample()))
        .ok_or_else(|| BioFormatsError::Format("QuickTime plane size overflows".into()))?;

    let mut out = match (codec, depth) {
        (QuickTimeCodec::UncompressedRgb, 32) => {
            let stored_row = width.checked_mul(4).ok_or_else(|| {
                BioFormatsError::Format("QuickTime uncompressed row size overflows".into())
            })?;
            let required = stored_row.checked_mul(height).ok_or_else(|| {
                BioFormatsError::Format("QuickTime uncompressed plane size overflows".into())
            })?;
            if sample.len() != required {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "QuickTime sample {plane_index} has {} bytes, expected {required} for 32-bit uncompressed pixels",
                    sample.len()
                )));
            }
            let mut decoded = Vec::with_capacity(expected);
            for px in sample.chunks_exact(4) {
                decoded.extend_from_slice(&px[1..4]);
            }
            decoded
        }
        _ => {
            let stored_channels = match codec {
                QuickTimeCodec::UncompressedGray => 1usize,
                _ => channels,
            };
            let pixel_bytes = stored_channels
                .checked_mul(meta.pixel_type.bytes_per_sample())
                .ok_or_else(|| {
                    BioFormatsError::Format("QuickTime uncompressed pixel size overflows".into())
                })?;
            let row_bytes = width.checked_mul(pixel_bytes).ok_or_else(|| {
                BioFormatsError::Format("QuickTime uncompressed row size overflows".into())
            })?;
            let pad = (4 - (width % 4)) % 4;
            let padded_row = row_bytes.checked_add(pad).ok_or_else(|| {
                BioFormatsError::Format("QuickTime uncompressed row size overflows".into())
            })?;
            let padded_expected = padded_row.checked_mul(height).ok_or_else(|| {
                BioFormatsError::Format("QuickTime uncompressed plane size overflows".into())
            })?;
            if sample.len() == expected {
                sample.to_vec()
            } else if pad > 0 && sample.len() == padded_expected {
                let mut decoded = Vec::with_capacity(expected);
                for row in sample.chunks_exact(padded_row) {
                    decoded.extend_from_slice(&row[..row_bytes]);
                }
                decoded
            } else {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "QuickTime sample {plane_index} has {} bytes, expected {expected} for uncompressed pixels",
                    sample.len()
                )));
            }
        }
    };

    // Java QTReader inverts 8-bit and 40-bit uncompressed planes after
    // cropping, except for mjpb. In bioformats-rs this corresponds to the
    // explicit grayscale uncompressed codecs.
    if matches!(codec, QuickTimeCodec::UncompressedGray) {
        quicktime_invert_pixels(&mut out);
    }
    Ok(out)
}

fn decode_quicktime_jpeg_sample(
    sample: &[u8],
    meta: &ImageMetadata,
    plane_index: u32,
) -> Result<Vec<u8>> {
    let sample = crate::common::codec::jpeg_payload(sample);
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

fn decode_quicktime_mjpb_sample(sample: &[u8], plane_index: u32) -> Result<Vec<u8>> {
    crate::common::codec::decompress_mjpb(sample).map_err(|err| {
        BioFormatsError::UnsupportedFormat(format!(
            "QuickTime Motion JPEG-B sample {plane_index} failed to decode: {err}"
        ))
    })
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

/// QuickTime RPZA codec entry point, mirroring Java `RPZACodec.decompress`.
///
/// Validates the requested output shape, then dispatches to the shared
/// `decompress_rpza` block decoder in `common::codec` (the equivalent of
/// `ome.codecs.RPZACodec`).
fn quicktime_decompress_rpza(
    sample: &[u8],
    meta: &ImageMetadata,
    plane_index: u32,
) -> Result<Vec<u8>> {
    if meta.size_c != 3 || meta.pixel_type != PixelType::Uint8 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "QuickTime RPZA sample {plane_index} only supports 3-channel Uint8 output"
        )));
    }
    decompress_rpza(sample, meta.size_x, meta.size_y).map_err(|err| {
        BioFormatsError::UnsupportedFormat(format!(
            "QuickTime RPZA sample {plane_index} failed to decode: {err}"
        ))
    })
}

/// QuickTime Animation (QTRLE) codec entry point, mirroring Java
/// `QTRLECodec.decompress`.
///
/// Java implements QTRLE as a single codec class with one `decompress`
/// method that walks the per-line opcode stream, applying skip/repeat/literal
/// runs against the previous frame (delta) or a fresh frame (key frame). This
/// is the 1:1 Rust port of that method: it handles 16/24/32-bit RGB depths and
/// both delta and key frames in one place, so the QTRLE decode logic is no
/// longer split across the reader and `common::codec`.
fn quicktime_decompress_qtrle(
    sample: &[u8],
    meta: &ImageMetadata,
    plane_index: u32,
    depth: u16,
    previous: Option<&[u8]>,
) -> Result<Vec<u8>> {
    if meta.size_c != 3 || meta.pixel_type != PixelType::Uint8 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "QuickTime Animation RLE sample {plane_index} only supports RGB Uint8 output"
        )));
    }
    if !matches!(depth, 16 | 24 | 32) {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "QuickTime Animation RLE depth {depth} is unsupported"
        )));
    }
    let width = meta.size_x as usize;
    let height = meta.size_y as usize;
    let expected = width
        .checked_mul(height)
        .and_then(|px| px.checked_mul(3))
        .ok_or_else(|| {
            BioFormatsError::Format("QuickTime Animation RLE plane size overflows".into())
        })?;

    // Delta frames (header flag 0x0008) patch the previous frame; key frames
    // start from a fresh buffer. Reject a delta frame that has no predecessor.
    let is_delta = quicktime_rle_is_delta(sample)?;
    let mut out = if is_delta {
        let previous = previous.ok_or_else(|| {
            BioFormatsError::UnsupportedFormat(format!(
                "QuickTime Animation RLE sample {plane_index} is a partial/delta frame without a previous frame"
            ))
        })?;
        if previous.len() != expected {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "QuickTime Animation RLE sample {plane_index} previous frame has {} bytes, expected {expected}",
                previous.len()
            )));
        }
        previous.to_vec()
    } else {
        vec![0u8; expected]
    };

    if sample.len() < 6 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "QuickTime Animation RLE sample {plane_index} is truncated"
        )));
    }
    let chunk_size = u32::from_be_bytes([sample[0], sample[1], sample[2], sample[3]]) as usize;
    if chunk_size < 6 || chunk_size > sample.len() {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "QuickTime Animation RLE sample {plane_index} has invalid chunk size"
        )));
    }
    let mut i = 4usize;
    let header = quicktime_rle_read_u16(sample, &mut i, chunk_size)?;
    if header & !0x0008 != 0 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "QuickTime Animation RLE sample {plane_index} has unsupported header flags 0x{header:04x}"
        )));
    }
    let (start_line, changed_lines) = if header & 0x0008 != 0 {
        let start_line = quicktime_rle_read_u16(sample, &mut i, chunk_size)? as usize;
        quicktime_rle_skip(sample, &mut i, chunk_size, 2)?;
        let changed_lines = quicktime_rle_read_u16(sample, &mut i, chunk_size)? as usize;
        quicktime_rle_skip(sample, &mut i, chunk_size, 2)?;
        (start_line, changed_lines)
    } else {
        (0, height)
    };
    let end_line = start_line.checked_add(changed_lines).ok_or_else(|| {
        BioFormatsError::UnsupportedFormat(format!(
            "QuickTime Animation RLE sample {plane_index} changed-line range overflows"
        ))
    })?;
    if end_line > height {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "QuickTime Animation RLE sample {plane_index} changed-line range exceeds image height"
        )));
    }
    for y in start_line..end_line {
        let initial_skip = quicktime_rle_read_u8(sample, &mut i, chunk_size)? as usize;
        if initial_skip == 0 {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "QuickTime Animation RLE sample {plane_index} line skip underflows"
            )));
        }
        let mut x = initial_skip - 1;
        loop {
            let opcode = quicktime_rle_read_i8(sample, &mut i, chunk_size)?;
            match opcode {
                -1 => break,
                0 => {
                    let skip = quicktime_rle_read_u8(sample, &mut i, chunk_size)? as usize;
                    if skip == 0 {
                        return Err(BioFormatsError::UnsupportedFormat(format!(
                            "QuickTime Animation RLE sample {plane_index} skip underflows"
                        )));
                    }
                    x = x.checked_add(skip - 1).ok_or_else(|| {
                        BioFormatsError::UnsupportedFormat(format!(
                            "QuickTime Animation RLE sample {plane_index} skip overflows"
                        ))
                    })?;
                    if x > width {
                        return Err(BioFormatsError::UnsupportedFormat(format!(
                            "QuickTime Animation RLE sample {plane_index} skip exceeds row width"
                        )));
                    }
                }
                n if n < 0 => {
                    let count = (-n) as usize;
                    let pixel = quicktime_rle_read_pixel(sample, &mut i, chunk_size, depth)?;
                    quicktime_rle_write_pixels(&mut out, width, y, &mut x, count, pixel)?;
                }
                n => {
                    for _ in 0..n as usize {
                        let pixel = quicktime_rle_read_pixel(sample, &mut i, chunk_size, depth)?;
                        quicktime_rle_write_pixels(&mut out, width, y, &mut x, 1, pixel)?;
                    }
                }
            }
        }
    }
    Ok(out)
}

fn quicktime_rle_is_delta(sample: &[u8]) -> Result<bool> {
    if sample.len() < 6 {
        return Err(BioFormatsError::UnsupportedFormat(
            "QuickTime Animation RLE sample is truncated".into(),
        ));
    }
    let chunk_size = u32::from_be_bytes([sample[0], sample[1], sample[2], sample[3]]) as usize;
    if chunk_size < 6 || chunk_size > sample.len() {
        return Err(BioFormatsError::UnsupportedFormat(
            "QuickTime Animation RLE sample has invalid chunk size".into(),
        ));
    }
    let header = u16::from_be_bytes([sample[4], sample[5]]);
    Ok(header & 0x0008 != 0)
}

fn quicktime_rle_read_u16(sample: &[u8], i: &mut usize, limit: usize) -> Result<u16> {
    if *i + 2 > limit {
        return Err(BioFormatsError::UnsupportedFormat(
            "QuickTime Animation RLE sample is truncated".into(),
        ));
    }
    let value = u16::from_be_bytes([sample[*i], sample[*i + 1]]);
    *i += 2;
    Ok(value)
}

fn quicktime_rle_read_u8(sample: &[u8], i: &mut usize, limit: usize) -> Result<u8> {
    if *i >= limit {
        return Err(BioFormatsError::UnsupportedFormat(
            "QuickTime Animation RLE sample is truncated".into(),
        ));
    }
    let value = sample[*i];
    *i += 1;
    Ok(value)
}

fn quicktime_rle_read_i8(sample: &[u8], i: &mut usize, limit: usize) -> Result<i8> {
    Ok(quicktime_rle_read_u8(sample, i, limit)? as i8)
}

fn quicktime_rle_skip(sample: &[u8], i: &mut usize, limit: usize, count: usize) -> Result<()> {
    if *i + count > limit || limit > sample.len() {
        return Err(BioFormatsError::UnsupportedFormat(
            "QuickTime Animation RLE sample is truncated".into(),
        ));
    }
    *i += count;
    Ok(())
}

fn quicktime_rle_read_pixel(
    sample: &[u8],
    i: &mut usize,
    limit: usize,
    depth: u16,
) -> Result<[u8; 3]> {
    match depth {
        16 => {
            let value = quicktime_rle_read_u16(sample, i, limit)?;
            Ok(quicktime_rgb555_to_rgb24(value))
        }
        24 => {
            if *i + 3 > limit {
                return Err(BioFormatsError::UnsupportedFormat(
                    "QuickTime Animation RLE sample is truncated".into(),
                ));
            }
            let rgb = [sample[*i], sample[*i + 1], sample[*i + 2]];
            *i += 3;
            Ok(rgb)
        }
        32 => {
            if *i + 4 > limit {
                return Err(BioFormatsError::UnsupportedFormat(
                    "QuickTime Animation RLE sample is truncated".into(),
                ));
            }
            let rgb = [sample[*i + 1], sample[*i + 2], sample[*i + 3]];
            *i += 4;
            Ok(rgb)
        }
        other => Err(BioFormatsError::UnsupportedFormat(format!(
            "QuickTime Animation RLE depth {other} is unsupported"
        ))),
    }
}

fn quicktime_rle_write_pixels(
    out: &mut [u8],
    width: usize,
    y: usize,
    x: &mut usize,
    count: usize,
    pixel: [u8; 3],
) -> Result<()> {
    if *x + count > width {
        return Err(BioFormatsError::UnsupportedFormat(
            "QuickTime Animation RLE repeat run exceeds row width".into(),
        ));
    }
    for _ in 0..count {
        let dst = (y * width + *x) * 3;
        out[dst..dst + 3].copy_from_slice(&pixel);
        *x += 1;
    }
    Ok(())
}

fn quicktime_rgb555_to_rgb24(color: u16) -> [u8; 3] {
    let r = ((color >> 10) & 0x1f) as u8;
    let g = ((color >> 5) & 0x1f) as u8;
    let b = (color & 0x1f) as u8;
    [
        (r << 3) | (r >> 2),
        (g << 3) | (g >> 2),
        (b << 3) | (b >> 2),
    ]
}

/// QuickTime Cinepak (`cvid`) codec entry point, mirroring Java
/// `CinepakCodec.decompress`.
///
/// Validates the requested output shape and previous-frame buffer, then
/// dispatches to the shared `decompress_cinepak` block decoder in
/// `common::codec` (the equivalent of `ome.codecs.CinepakCodec`).
fn quicktime_decompress_cinepak(
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

fn parse_quicktime_ctts(
    ctts: Atom<'_>,
    expected_samples: usize,
) -> Result<Vec<QuickTimeCttsEntry>> {
    if ctts.data.len() < 8 {
        return Err(BioFormatsError::UnsupportedFormat(
            "QuickTime ctts atom is truncated".into(),
        ));
    }
    let version = ctts.data[0];
    if version > 1 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "QuickTime ctts version {version} is unsupported"
        )));
    }
    let entry_count = be_u32_at(ctts.data, 4).unwrap() as usize;
    if ctts.data.len() < 8 + entry_count * 8 {
        return Err(BioFormatsError::UnsupportedFormat(
            "QuickTime ctts table is truncated".into(),
        ));
    }
    let mut entries = Vec::with_capacity(entry_count);
    let mut total = 0usize;
    for i in 0..entry_count {
        let base = 8 + i * 8;
        let sample_count = be_u32_at(ctts.data, base).unwrap();
        let sample_offset = if version == 0 {
            i64::from(be_u32_at(ctts.data, base + 4).unwrap())
        } else {
            i64::from(be_i32_at(ctts.data, base + 4).unwrap())
        };
        if sample_count == 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "QuickTime ctts contains a zero sample count".into(),
            ));
        }
        total += sample_count as usize;
        entries.push(QuickTimeCttsEntry {
            sample_count,
            sample_offset,
        });
    }
    if total != expected_samples {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "QuickTime ctts sample count {total} does not match stsz sample count {expected_samples}"
        )));
    }
    Ok(entries)
}

fn parse_quicktime_stsc(stsc: Atom<'_>) -> Result<Vec<QuickTimeStscEntry>> {
    if stsc.data.len() < 8 {
        return Err(BioFormatsError::UnsupportedFormat(
            "QuickTime stsc atom is truncated".into(),
        ));
    }
    let entry_count = be_u32_at(stsc.data, 4).unwrap() as usize;
    if stsc.data.len() < 8 + entry_count * 12 {
        return Err(BioFormatsError::UnsupportedFormat(
            "QuickTime stsc table is truncated".into(),
        ));
    }
    let mut entries = Vec::with_capacity(entry_count);
    for i in 0..entry_count {
        let base = 8 + i * 12;
        entries.push(QuickTimeStscEntry {
            first_chunk: be_u32_at(stsc.data, base).unwrap(),
            samples_per_chunk: be_u32_at(stsc.data, base + 4).unwrap(),
            sample_description_index: be_u32_at(stsc.data, base + 8).unwrap(),
        });
    }
    Ok(entries)
}

fn quicktime_sample_offsets_from_chunks(
    chunk_offsets: &[u64],
    sample_sizes: &[u32],
    stsc_entries: Option<&[QuickTimeStscEntry]>,
    chunk_offset_table_type: &str,
) -> Result<Vec<u64>> {
    let Some(stsc_entries) = stsc_entries else {
        if chunk_offsets.len() != sample_sizes.len() {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "QuickTime blind parser requires one {chunk_offset_table_type} chunk offset per sample"
            )));
        }
        return Ok(chunk_offsets.to_vec());
    };

    if stsc_entries.is_empty() {
        return Err(BioFormatsError::UnsupportedFormat(
            "QuickTime stsc table contains no entries".into(),
        ));
    }
    if stsc_entries[0].first_chunk != 1 {
        return Err(BioFormatsError::UnsupportedFormat(
            "QuickTime stsc first entry must start at chunk 1".into(),
        ));
    }
    if stsc_entries
        .iter()
        .any(|entry| entry.first_chunk == 0 || entry.samples_per_chunk == 0)
    {
        return Err(BioFormatsError::UnsupportedFormat(
            "QuickTime stsc contains an invalid chunk or sample count".into(),
        ));
    }
    if stsc_entries
        .windows(2)
        .any(|window| window[1].first_chunk <= window[0].first_chunk)
    {
        return Err(BioFormatsError::UnsupportedFormat(
            "QuickTime stsc entries are not strictly increasing".into(),
        ));
    }

    let mut sample_offsets = Vec::with_capacity(sample_sizes.len());
    let mut sample_index = 0usize;
    let mut stsc_index = 0usize;
    for (chunk_index, &chunk_offset) in chunk_offsets.iter().enumerate() {
        let chunk_number = (chunk_index + 1) as u32;
        while stsc_index + 1 < stsc_entries.len()
            && stsc_entries[stsc_index + 1].first_chunk <= chunk_number
        {
            stsc_index += 1;
        }
        let samples_per_chunk = stsc_entries[stsc_index].samples_per_chunk as usize;
        let mut offset = chunk_offset;
        for _ in 0..samples_per_chunk {
            if sample_index >= sample_sizes.len() {
                return Ok(sample_offsets);
            }
            sample_offsets.push(offset);
            offset = offset
                .checked_add(sample_sizes[sample_index] as u64)
                .ok_or_else(|| {
                    BioFormatsError::Format("QuickTime sample offset overflows".into())
                })?;
            sample_index += 1;
        }
    }

    if sample_offsets.len() != sample_sizes.len() {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "QuickTime stsc maps {} samples from {} chunks, but stsz declares {} samples",
            sample_offsets.len(),
            chunk_offsets.len(),
            sample_sizes.len()
        )));
    }
    Ok(sample_offsets)
}

fn quicktime_sample_description_indices_from_chunks(
    chunk_count: usize,
    sample_count: usize,
    stsc_entries: Option<&[QuickTimeStscEntry]>,
) -> Result<Vec<u32>> {
    let Some(stsc_entries) = stsc_entries else {
        return Ok(vec![1; sample_count]);
    };

    let mut sample_description_indices = Vec::with_capacity(sample_count);
    let mut stsc_index = 0usize;
    for chunk_index in 0..chunk_count {
        let chunk_number = (chunk_index + 1) as u32;
        while stsc_index + 1 < stsc_entries.len()
            && stsc_entries[stsc_index + 1].first_chunk <= chunk_number
        {
            stsc_index += 1;
        }
        for _ in 0..stsc_entries[stsc_index].samples_per_chunk {
            if sample_description_indices.len() >= sample_count {
                return Ok(sample_description_indices);
            }
            sample_description_indices.push(stsc_entries[stsc_index].sample_description_index);
        }
    }

    if sample_description_indices.len() != sample_count {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "QuickTime stsc maps {} sample descriptions, but stsz declares {sample_count} samples",
            sample_description_indices.len()
        )));
    }
    Ok(sample_description_indices)
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

fn quicktime_sample_composition_offsets(entries: &[QuickTimeCttsEntry]) -> Option<Vec<i64>> {
    let sample_count = entries.iter().try_fold(0usize, |total, entry| {
        total.checked_add(entry.sample_count as usize)
    })?;
    let mut offsets = Vec::with_capacity(sample_count);
    for entry in entries {
        for _ in 0..entry.sample_count {
            offsets.push(entry.sample_offset);
        }
    }
    Some(offsets)
}

fn quicktime_apply_composition_offsets(
    presentation_times: &[i64],
    composition_offsets: &[i64],
) -> Option<Vec<i64>> {
    if presentation_times.len() != composition_offsets.len() {
        return None;
    }
    presentation_times
        .iter()
        .zip(composition_offsets)
        .map(|(time, offset)| time.checked_add(*offset))
        .collect()
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

fn quicktime_insert_u32_list_metadata(
    metadata: &mut HashMap<String, MetadataValue>,
    key: &str,
    values: &[u32],
) {
    metadata.insert(
        key.into(),
        MetadataValue::String(
            values
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(","),
        ),
    );
}

fn quicktime_insert_clipped_sample_range_metadata(
    metadata: &mut HashMap<String, MetadataValue>,
    sample_indices: &[usize],
    sample_media_times: &[u64],
    media_duration_ticks: u64,
) -> Option<()> {
    let first_index = *sample_indices.first()?;
    let last_index = *sample_indices.last()?;
    if last_index < first_index || last_index >= sample_media_times.len() {
        return None;
    }
    if sample_indices
        .windows(2)
        .any(|window| window[1] != window[0] + 1)
    {
        return None;
    }
    let retained_count = last_index.checked_sub(first_index)?.checked_add(1)?;
    let after_count = sample_media_times.len().checked_sub(last_index + 1)?;
    quicktime_insert_u64_metadata(
        metadata,
        "quicktime.edit_list.clipped_sample_range_start_index",
        u64::try_from(first_index).ok()?,
    );
    quicktime_insert_u64_metadata(
        metadata,
        "quicktime.edit_list.clipped_sample_range_end_index_exclusive",
        u64::try_from(last_index + 1).ok()?,
    );
    quicktime_insert_u64_metadata(
        metadata,
        "quicktime.edit_list.clipped_sample_count",
        u64::try_from(retained_count).ok()?,
    );
    quicktime_insert_u64_metadata(
        metadata,
        "quicktime.edit_list.clipped_before_sample_count",
        u64::try_from(first_index).ok()?,
    );
    quicktime_insert_u64_metadata(
        metadata,
        "quicktime.edit_list.clipped_after_sample_count",
        u64::try_from(after_count).ok()?,
    );
    quicktime_insert_u64_metadata(
        metadata,
        "quicktime.edit_list.clipped_source_start_media_time_ticks",
        sample_media_times[first_index],
    );
    let end_media_time = sample_media_times
        .get(last_index + 1)
        .copied()
        .unwrap_or(media_duration_ticks);
    quicktime_insert_u64_metadata(
        metadata,
        "quicktime.edit_list.clipped_source_end_media_time_ticks",
        end_media_time,
    );
    Some(())
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
        return Some(0);
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
    sample_media_times: &[u64],
    media_duration_ticks: u64,
    media_timescale: Option<u32>,
    movie_timescale: Option<u32>,
) -> std::result::Result<QuickTimeEditPresentationMap, QuickTimeEditListDiagnostic> {
    let mut out = vec![None; sample_media_times.len()];
    let mut source_times = vec![None; sample_media_times.len()];
    let mut segment_indices = vec![None; sample_media_times.len()];
    let mut cursor = 0u64;
    let mut media_segment_index = 0usize;
    for (entry_index, entry) in entries.iter().enumerate() {
        let duration = quicktime_movie_ticks_to_media_ticks(
            entry.segment_duration,
            media_timescale,
            movie_timescale,
        )
        .ok_or_else(|| {
            QuickTimeEditListDiagnostic::new(
                "non_integral_segment_duration",
                "media segment duration cannot be represented exactly in media ticks",
            )
            .with_segment(entry_index)
        })?;
        if entry.media_time < 0 {
            cursor = cursor.checked_add(duration).ok_or_else(|| {
                QuickTimeEditListDiagnostic::new(
                    "presentation_timeline_overflow",
                    "presentation timeline duration overflows",
                )
                .with_segment(entry_index)
            })?;
            continue;
        }
        let segment_index = media_segment_index;
        media_segment_index += 1;
        let start = u64::try_from(entry.media_time).map_err(|_| {
            QuickTimeEditListDiagnostic::new("negative_media_time", "negative media_time")
                .with_segment(segment_index)
        })?;
        let end = start.checked_add(duration).ok_or_else(|| {
            QuickTimeEditListDiagnostic::new(
                "segment_duration_overflow",
                "media segment duration overflows",
            )
            .with_segment(segment_index)
        })?;
        if end > media_duration_ticks {
            return Err(QuickTimeEditListDiagnostic::new(
                "segment_extends_past_media_duration",
                "media segment extends past media duration",
            )
            .with_segment(segment_index));
        }
        let start_index = sample_media_times.binary_search(&start).map_err(|_| {
            QuickTimeEditListDiagnostic::new(
                "non_sample_aligned_start",
                "media segment start is not sample-aligned",
            )
            .with_segment(segment_index)
        })?;
        let end_index = if end == media_duration_ticks {
            sample_media_times.len()
        } else {
            sample_media_times.binary_search(&end).map_err(|_| {
                QuickTimeEditListDiagnostic::new(
                    "non_sample_aligned_end",
                    "media segment end is not sample-aligned",
                )
                .with_segment(segment_index)
            })?
        };
        if start_index >= end_index {
            return Err(QuickTimeEditListDiagnostic::new(
                "empty_media_segment",
                "media segment contains no complete samples",
            )
            .with_segment(segment_index));
        }
        for sample_index in start_index..end_index {
            if out[sample_index].is_some() {
                return Err(QuickTimeEditListDiagnostic::new(
                    "overlapping_media_segments",
                    "media segments overlap in sample space",
                )
                .with_segment(segment_index)
                .with_sample(sample_index));
            }
            let t = cursor
                .checked_add(sample_media_times[sample_index] - start)
                .ok_or_else(|| {
                    QuickTimeEditListDiagnostic::new(
                        "presentation_time_overflow",
                        "presentation time overflows",
                    )
                    .with_segment(segment_index)
                    .with_sample(sample_index)
                })?;
            out[sample_index] = Some(i64::try_from(t).map_err(|_| {
                QuickTimeEditListDiagnostic::new(
                    "presentation_time_out_of_range",
                    "presentation time exceeds i64 range",
                )
                .with_segment(segment_index)
                .with_sample(sample_index)
            })?);
            source_times[sample_index] = Some(sample_media_times[sample_index]);
            segment_indices[sample_index] = Some(u32::try_from(segment_index).map_err(|_| {
                QuickTimeEditListDiagnostic::new(
                    "segment_index_out_of_range",
                    "edit segment index exceeds u32 range",
                )
                .with_segment(segment_index)
                .with_sample(sample_index)
            })?);
        }
        cursor = cursor.checked_add(duration).ok_or_else(|| {
            QuickTimeEditListDiagnostic::new(
                "presentation_timeline_overflow",
                "presentation timeline duration overflows",
            )
            .with_segment(segment_index)
        })?;
    }
    let covered_indices = out
        .iter()
        .enumerate()
        .filter_map(|(index, time)| time.map(|_| index))
        .collect::<Vec<_>>();
    if covered_indices.is_empty() {
        return Err(QuickTimeEditListDiagnostic::new(
            "empty_media_segment",
            "media segments contain no complete samples",
        ));
    }
    if covered_indices.len() != sample_media_times.len() {
        let first_covered = *covered_indices.first().unwrap();
        let last_covered = *covered_indices.last().unwrap();
        if let Some(sample_index) =
            (first_covered..=last_covered).find(|&index| out[index].is_none())
        {
            return Err(QuickTimeEditListDiagnostic::new(
                "gapped_media_segments",
                "edit list media segments do not cover every sample",
            )
            .with_sample(sample_index));
        }
    }
    let presentation_times = covered_indices
        .iter()
        .map(|&index| out[index])
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| {
            QuickTimeEditListDiagnostic::new(
                "gapped_media_segments",
                "edit list media segments do not cover every sample",
            )
        })?;
    let media_times = covered_indices
        .iter()
        .map(|&index| source_times[index])
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| {
            QuickTimeEditListDiagnostic::new(
                "gapped_media_segments",
                "edit list media segments do not cover every sample",
            )
        })?;
    let segment_indices = covered_indices
        .iter()
        .map(|&index| segment_indices[index])
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| {
            QuickTimeEditListDiagnostic::new(
                "gapped_media_segments",
                "edit list media segments do not cover every sample",
            )
        })?;
    Ok(QuickTimeEditPresentationMap {
        presentation_times,
        media_times,
        segment_indices,
        sample_indices: covered_indices,
    })
}

fn quicktime_sample_read_order_from_presentation_times(
    presentation_times: &[i64],
) -> Option<Vec<usize>> {
    let mut indexed = presentation_times
        .iter()
        .copied()
        .enumerate()
        .collect::<Vec<_>>();
    indexed.sort_by_key(|&(_, time)| time);
    if indexed.windows(2).any(|window| window[0].1 == window[1].1) {
        return None;
    }
    Some(indexed.into_iter().map(|(index, _)| index).collect())
}

fn quicktime_edit_presentation_times(
    entries: &[QuickTimeEditEntry],
    media_timescale: Option<u32>,
    movie_timescale: Option<u32>,
    sample_media_times: &[u64],
    media_duration_ticks: u64,
    metadata: &mut HashMap<String, MetadataValue>,
) -> Option<QuickTimeEditPresentationResult> {
    let mut empty_count = 0usize;
    let mut empty_movie_ticks = 0u64;
    let mut leading_empty_movie_ticks = 0u64;
    let mut empty_after_media_count = 0usize;
    let mut empty_after_media_movie_ticks = 0u64;
    let mut first_empty_after_media_segment_index = None;
    let mut first_internal_empty_segment_index = None;
    let mut media_segments = Vec::new();
    for (entry_index, entry) in entries.iter().enumerate() {
        if (entry.media_rate - 1.0).abs() > f64::EPSILON {
            metadata.insert(
                "quicktime.edit_list.presentation_status".into(),
                MetadataValue::String("not_applied_non_unit_rate".into()),
            );
            quicktime_insert_edit_list_pixel_order_diagnostic(
                metadata,
                "not_reordered_non_unit_rate",
                "edit-list pixel-plane reordering is not applied for non-unit media rates",
            );
            metadata.insert(
                "quicktime.edit_list.presentation_diagnostic".into(),
                MetadataValue::String(format!(
                    "edit list contains media_rate {}",
                    entry.media_rate
                )),
            );
            metadata.insert(
                "quicktime.edit_list.unsupported_reason".into(),
                MetadataValue::String("non_unit_rate".into()),
            );
            metadata.insert(
                "quicktime.edit_list.first_problem_segment_index".into(),
                MetadataValue::Int(entry_index as i64),
            );
            metadata.insert(
                "quicktime.edit_list.media_rate".into(),
                MetadataValue::Float(entry.media_rate),
            );
            return None;
        }
        if entry.media_time < 0 {
            if !media_segments.is_empty() {
                empty_after_media_count += 1;
                empty_after_media_movie_ticks =
                    empty_after_media_movie_ticks.checked_add(entry.segment_duration)?;
                if first_empty_after_media_segment_index.is_none() {
                    first_empty_after_media_segment_index = Some(entry_index);
                }
                if entries[entry_index + 1..]
                    .iter()
                    .any(|later| later.media_time >= 0)
                    && first_internal_empty_segment_index.is_none()
                {
                    first_internal_empty_segment_index = Some(entry_index);
                }
            } else {
                leading_empty_movie_ticks =
                    leading_empty_movie_ticks.checked_add(entry.segment_duration)?;
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
    if empty_after_media_count > 0 {
        metadata.insert(
            "quicktime.edit_list.empty_after_media_count".into(),
            MetadataValue::Int(empty_after_media_count as i64),
        );
        quicktime_insert_u64_metadata(
            metadata,
            "quicktime.edit_list.empty_after_media_duration_movie_ticks",
            empty_after_media_movie_ticks,
        );
        if let Some(segment_index) = first_empty_after_media_segment_index {
            metadata.insert(
                "quicktime.edit_list.first_empty_after_media_segment_index".into(),
                MetadataValue::Int(segment_index as i64),
            );
        }
    }
    let first = media_segments.first()?;
    metadata.insert(
        "quicktime.edit_list.media_time_ticks".into(),
        MetadataValue::Int(first.media_time),
    );
    metadata.insert(
        "quicktime.edit_list.media_rate".into(),
        MetadataValue::Float(first.media_rate),
    );
    let empty_media_ticks = quicktime_movie_ticks_to_media_ticks(
        leading_empty_movie_ticks,
        media_timescale,
        movie_timescale,
    )?;
    let has_leading_empty_edits = leading_empty_movie_ticks > 0;
    let has_trailing_empty_edits = empty_after_media_count > 0;
    if media_segments.len() == 1 {
        let single_segment_boundary_diagnostic = (!has_leading_empty_edits
            && !has_trailing_empty_edits)
            .then_some(())
            .and_then(|_| u64::try_from(first.media_time).ok())
            .and_then(|start| {
                let duration = quicktime_movie_ticks_to_media_ticks(
                    first.segment_duration,
                    media_timescale,
                    movie_timescale,
                )?;
                let end = start.checked_add(duration)?;
                if end > media_duration_ticks {
                    return None;
                }
                let start_index = match sample_media_times.binary_search(&start) {
                    Ok(index) => index,
                    Err(_) => {
                        return Some(
                            QuickTimeEditListDiagnostic::new(
                                "non_sample_aligned_start",
                                "media segment start is not sample-aligned",
                            )
                            .with_segment(0),
                        );
                    }
                };
                let end_index = if end == media_duration_ticks {
                    sample_media_times.len()
                } else {
                    match sample_media_times.binary_search(&end) {
                        Ok(index) => index,
                        Err(_) => {
                            return Some(
                                QuickTimeEditListDiagnostic::new(
                                    "non_sample_aligned_end",
                                    "media segment end is not sample-aligned",
                                )
                                .with_segment(0),
                            );
                        }
                    }
                };
                if start_index >= end_index {
                    Some(
                        QuickTimeEditListDiagnostic::new(
                            "empty_media_segment",
                            "media segment contains no complete samples",
                        )
                        .with_segment(0),
                    )
                } else {
                    None
                }
            });
        if let Some(diagnostic) = single_segment_boundary_diagnostic {
            metadata.insert(
                "quicktime.edit_list.presentation_status".into(),
                MetadataValue::String("not_applied_complex_edit_list".into()),
            );
            quicktime_insert_edit_list_pixel_order_diagnostic(
                metadata,
                "not_reordered_complex_edit_list",
                "edit-list pixel-plane reordering is not applied for non-sample-aligned, gapped, overlapping, or clipped media segments",
            );
            metadata.insert(
                "quicktime.edit_list.presentation_diagnostic".into(),
                MetadataValue::String(format!(
                    "single media segment not applied: {}",
                    diagnostic.message
                )),
            );
            metadata.insert(
                "quicktime.edit_list.unsupported_reason".into(),
                MetadataValue::String(diagnostic.reason.into()),
            );
            if let Some(segment_index) = diagnostic.segment_index {
                metadata.insert(
                    "quicktime.edit_list.first_problem_segment_index".into(),
                    MetadataValue::Int(segment_index as i64),
                );
            }
            if let Some(sample_index) = diagnostic.sample_index {
                metadata.insert(
                    "quicktime.edit_list.first_problem_sample_index".into(),
                    MetadataValue::Int(sample_index as i64),
                );
            }
            return None;
        }
        let clipped_single = u64::try_from(first.media_time).ok().and_then(|start| {
            let duration = quicktime_movie_ticks_to_media_ticks(
                first.segment_duration,
                media_timescale,
                movie_timescale,
            )?;
            let end = start.checked_add(duration)?;
            if end > media_duration_ticks {
                return None;
            }
            let start_index = sample_media_times.binary_search(&start).ok()?;
            let end_index = if end == media_duration_ticks {
                sample_media_times.len()
            } else {
                sample_media_times.binary_search(&end).ok()?
            };
            if start_index >= end_index
                || (start_index == 0 && end_index == sample_media_times.len())
            {
                None
            } else {
                Some((start, start_index, end_index))
            }
        });
        if let Some((start, start_index, end_index)) = clipped_single {
            metadata.insert(
                "quicktime.edit_list.presentation_status".into(),
                MetadataValue::String(
                    match (has_leading_empty_edits, has_trailing_empty_edits) {
                        (true, true) => {
                            "applied_leading_and_trailing_empty_edits_clipped_normal_speed_media_segments"
                        }
                        (true, false) => {
                            "applied_leading_empty_edits_clipped_normal_speed_media_segments"
                        }
                        (false, true) => {
                            "applied_trailing_empty_edits_clipped_normal_speed_media_segments"
                        }
                        (false, false) => "applied_clipped_normal_speed_media_segments",
                    }
                    .into(),
                ),
            );
            quicktime_insert_edit_list_pixel_order_diagnostic(
                metadata,
                "clipped_sample_aligned_normal_speed",
                "open_bytes clips to edit-list presentation samples for a sample-aligned normal-speed media segment",
            );
            metadata.insert(
                "quicktime.edit_list.presentation_diagnostic".into(),
                MetadataValue::String(format!(
                    "media_time {} applied at normal speed with sample-aligned clipped sample range",
                    first.media_time
                )),
            );
            let presentation_times = sample_media_times[start_index..end_index]
                .iter()
                .map(|time| {
                    let t = empty_media_ticks.checked_add(*time - start)?;
                    i64::try_from(t).ok()
                })
                .collect::<Option<Vec<_>>>()?;
            quicktime_insert_u64_list_metadata(
                metadata,
                "quicktime.edit_list.sample_source_media_time_ticks",
                &sample_media_times[start_index..end_index],
            );
            let segment_indices = vec![0; end_index - start_index];
            quicktime_insert_u32_list_metadata(
                metadata,
                "quicktime.edit_list.sample_media_segment_index",
                &segment_indices,
            );
            let sample_read_order = (start_index..end_index).collect::<Vec<_>>();
            let sample_read_order_u32 = sample_read_order
                .iter()
                .copied()
                .map(u32::try_from)
                .collect::<std::result::Result<Vec<_>, _>>()
                .ok()?;
            quicktime_insert_u32_list_metadata(
                metadata,
                "quicktime.edit_list.sample_read_order",
                &sample_read_order_u32,
            );
            quicktime_insert_u32_list_metadata(
                metadata,
                "quicktime.edit_list.clipped_sample_indices",
                &sample_read_order_u32,
            );
            quicktime_insert_clipped_sample_range_metadata(
                metadata,
                &sample_read_order,
                sample_media_times,
                media_duration_ticks,
            )?;
            return Some(QuickTimeEditPresentationResult {
                presentation_times,
                sample_read_order: Some(sample_read_order),
            });
        }
        let offset = i64::try_from(empty_media_ticks)
            .ok()?
            .checked_sub(first.media_time)?;
        metadata.insert(
            "quicktime.edit_list.presentation_status".into(),
            MetadataValue::String(
                match (has_leading_empty_edits, has_trailing_empty_edits) {
                    (true, true) => {
                        "applied_leading_and_trailing_empty_edits_single_normal_speed_media_segment"
                    }
                    (true, false) => {
                        "applied_leading_empty_edits_single_normal_speed_media_segment"
                    }
                    (false, true) => {
                        "applied_trailing_empty_edits_single_normal_speed_media_segment"
                    }
                    (false, false) => "applied_single_normal_speed_media_segment",
                }
                .into(),
            ),
        );
        quicktime_insert_edit_list_pixel_order_diagnostic(
            metadata,
            "metadata_only_sample_table_order",
            "edit-list timestamps and source mappings are recorded; open_bytes uses sample-table order and does not reorder or clip planes",
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
        quicktime_insert_u64_list_metadata(
            metadata,
            "quicktime.edit_list.sample_source_media_time_ticks",
            sample_media_times,
        );
        let segment_indices = vec![0; sample_media_times.len()];
        quicktime_insert_u32_list_metadata(
            metadata,
            "quicktime.edit_list.sample_media_segment_index",
            &segment_indices,
        );
        let presentation_times = sample_media_times
            .iter()
            .map(|time| i64::try_from(*time).ok()?.checked_add(offset))
            .collect::<Option<Vec<_>>>()?;
        return Some(QuickTimeEditPresentationResult {
            presentation_times,
            sample_read_order: None,
        });
    }
    match quicktime_multi_segment_presentation_times(
        entries,
        sample_media_times,
        media_duration_ticks,
        media_timescale,
        movie_timescale,
    ) {
        Ok(edit_map) => {
            let clipped = edit_map.sample_indices.len() != sample_media_times.len();
            let has_internal_empty_edits = first_internal_empty_segment_index.is_some();
            metadata.insert(
                "quicktime.edit_list.presentation_status".into(),
                MetadataValue::String(if clipped {
                    if has_internal_empty_edits {
                        "applied_internal_empty_edits_clipped_normal_speed_media_segments"
                    } else {
                        match (has_leading_empty_edits, has_trailing_empty_edits) {
                            (true, true) => {
                                "applied_leading_and_trailing_empty_edits_clipped_normal_speed_media_segments"
                            }
                            (true, false) => {
                                "applied_leading_empty_edits_clipped_normal_speed_media_segments"
                            }
                            (false, true) => {
                                "applied_trailing_empty_edits_clipped_normal_speed_media_segments"
                            }
                            (false, false) => "applied_clipped_normal_speed_media_segments",
                        }
                    }
                } else if has_leading_empty_edits && has_trailing_empty_edits {
                    "applied_leading_and_trailing_empty_edits_multiple_normal_speed_media_segments"
                } else if has_internal_empty_edits {
                    "applied_internal_empty_edits_multiple_normal_speed_media_segments"
                } else if has_leading_empty_edits {
                    "applied_leading_empty_edits_multiple_normal_speed_media_segments"
                } else if has_trailing_empty_edits {
                    "applied_trailing_empty_edits_multiple_normal_speed_media_segments"
                } else {
                    "applied_multiple_normal_speed_media_segments"
                }
                .into()),
            );
            quicktime_insert_edit_list_pixel_order_diagnostic(
                metadata,
                if clipped {
                    "clipped_sample_aligned_normal_speed"
                } else {
                    "reordered_sample_aligned_normal_speed"
                },
                if has_internal_empty_edits {
                    "open_bytes uses edit-list presentation order for sample-aligned normal-speed media segments and skips empty edits"
                } else if clipped {
                    "open_bytes clips to edit-list presentation samples for sample-aligned normal-speed media segments"
                } else {
                    "open_bytes uses edit-list presentation order for complete sample-aligned normal-speed media segments"
                },
            );
            metadata.insert(
                "quicktime.edit_list.presentation_diagnostic".into(),
                MetadataValue::String(format!(
                    "{} media segments applied at normal speed with sample-aligned boundaries{}{}",
                    media_segments.len(),
                    if has_internal_empty_edits {
                        " and internal empty edits"
                    } else {
                        ""
                    },
                    if clipped {
                        " and clipped sample range"
                    } else {
                        ""
                    }
                )),
            );
            quicktime_insert_u64_list_metadata(
                metadata,
                "quicktime.edit_list.sample_source_media_time_ticks",
                &edit_map.media_times,
            );
            quicktime_insert_u32_list_metadata(
                metadata,
                "quicktime.edit_list.sample_media_segment_index",
                &edit_map.segment_indices,
            );
            let sample_read_positions =
                quicktime_sample_read_order_from_presentation_times(&edit_map.presentation_times)?;
            let sample_read_order = sample_read_positions
                .iter()
                .map(|&position| edit_map.sample_indices.get(position).copied())
                .collect::<Option<Vec<_>>>()?;
            let sample_read_order_u32 = sample_read_order
                .iter()
                .copied()
                .map(u32::try_from)
                .collect::<std::result::Result<Vec<_>, _>>()
                .ok()?;
            quicktime_insert_u32_list_metadata(
                metadata,
                "quicktime.edit_list.sample_read_order",
                &sample_read_order_u32,
            );
            if clipped {
                let source_indices = edit_map
                    .sample_indices
                    .iter()
                    .copied()
                    .map(u32::try_from)
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .ok()?;
                quicktime_insert_u32_list_metadata(
                    metadata,
                    "quicktime.edit_list.clipped_sample_indices",
                    &source_indices,
                );
                quicktime_insert_clipped_sample_range_metadata(
                    metadata,
                    &edit_map.sample_indices,
                    sample_media_times,
                    media_duration_ticks,
                )?;
            }
            Some(QuickTimeEditPresentationResult {
                presentation_times: edit_map.presentation_times,
                sample_read_order: Some(sample_read_order),
            })
        }
        Err(diagnostic) => {
            metadata.insert(
                "quicktime.edit_list.presentation_status".into(),
                MetadataValue::String("not_applied_complex_edit_list".into()),
            );
            quicktime_insert_edit_list_pixel_order_diagnostic(
                metadata,
                "not_reordered_complex_edit_list",
                "edit-list pixel-plane reordering is not applied for non-sample-aligned, gapped, overlapping, or clipped media segments",
            );
            metadata.insert(
                "quicktime.edit_list.presentation_diagnostic".into(),
                MetadataValue::String(format!(
                    "multiple media segments not applied: {}",
                    diagnostic.message
                )),
            );
            metadata.insert(
                "quicktime.edit_list.unsupported_reason".into(),
                MetadataValue::String(diagnostic.reason.into()),
            );
            if let Some(segment_index) = diagnostic.segment_index {
                metadata.insert(
                    "quicktime.edit_list.first_problem_segment_index".into(),
                    MetadataValue::Int(segment_index as i64),
                );
            }
            if let Some(sample_index) = diagnostic.sample_index {
                metadata.insert(
                    "quicktime.edit_list.first_problem_sample_index".into(),
                    MetadataValue::Int(sample_index as i64),
                );
            }
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
    let stco = descendant(trak, &[*b"mdia", *b"minf", *b"stbl", *b"stco"])?;
    let co64 = descendant(trak, &[*b"mdia", *b"minf", *b"stbl", *b"co64"])?;
    let (chunk_offsets_atom, chunk_offset_table_type) = match (stco, co64) {
        (Some(stco), None) => (stco, "stco"),
        (None, Some(co64)) => (co64, "co64"),
        (Some(_), Some(_)) => {
            return Err(BioFormatsError::UnsupportedFormat(
                "QuickTime track contains both stco and co64 chunk offset tables".into(),
            ))
        }
        (None, None) => {
            return Err(BioFormatsError::UnsupportedFormat(
                "QuickTime missing stco/co64 atom".into(),
            ))
        }
    };
    let descriptions = parse_quicktime_stsd(stsd)?;
    let first_description = descriptions.first().ok_or_else(|| {
        BioFormatsError::UnsupportedFormat(
            "QuickTime stsd contains no video sample descriptions".into(),
        )
    })?;
    let codec = first_description.codec_fourcc;
    let qt_codec = first_description.codec;
    let width = first_description.width;
    let height = first_description.height;
    let mut samples_per_pixel = first_description.samples_per_pixel;

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

    if chunk_offsets_atom.data.len() < 8 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "QuickTime {chunk_offset_table_type} atom is truncated"
        )));
    }
    let chunk_count = be_u32_at(chunk_offsets_atom.data, 4).unwrap() as usize;
    let offset_entry_size = if chunk_offset_table_type == "co64" {
        8usize
    } else {
        4usize
    };
    if chunk_offsets_atom.data.len() < 8 + chunk_count * offset_entry_size {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "QuickTime {chunk_offset_table_type} table is truncated"
        )));
    }
    let chunk_offsets: Vec<u64> = (0..chunk_count)
        .map(|i| {
            let base = 8 + i * offset_entry_size;
            if chunk_offset_table_type == "co64" {
                let bytes = chunk_offsets_atom.data.get(base..base + 8).unwrap();
                u64::from_be_bytes([
                    bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
                ])
            } else {
                be_u32_at(chunk_offsets_atom.data, base).unwrap() as u64
            }
        })
        .collect();
    let stsc_entries = descendant(trak, &[*b"mdia", *b"minf", *b"stbl", *b"stsc"])?
        .map(parse_quicktime_stsc)
        .transpose()?;
    let sample_offsets = quicktime_sample_offsets_from_chunks(
        &chunk_offsets,
        &sample_sizes,
        stsc_entries.as_deref(),
        chunk_offset_table_type,
    )?;
    let sample_description_indices = quicktime_sample_description_indices_from_chunks(
        chunk_offsets.len(),
        sample_sizes.len(),
        stsc_entries.as_deref(),
    )?;
    let mut sample_codecs = Vec::with_capacity(sample_description_indices.len());
    let mut sample_depths = Vec::with_capacity(sample_description_indices.len());
    for description_index in &sample_description_indices {
        let description = descriptions
            .get(description_index.saturating_sub(1) as usize)
            .ok_or_else(|| {
                BioFormatsError::UnsupportedFormat(format!(
                    "QuickTime stsc references missing sample description {description_index}"
                ))
            })?;
        if description.width != width
            || description.height != height
            || description.samples_per_pixel != first_description.samples_per_pixel
        {
            return Err(BioFormatsError::UnsupportedFormat(
                "QuickTime alternate sample descriptions with different dimensions or channel counts are unsupported".into(),
            ));
        }
        sample_codecs.push(description.codec);
        sample_depths.push(description.depth);
    }
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
            let mut decoder =
                jpeg_decoder::Decoder::new(crate::common::codec::jpeg_payload(first_sample));
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
        QuickTimeCodec::Mjpb => {
            decode_quicktime_mjpb_sample(first_sample, 0)?;
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
                thumbnail: false,
                series_metadata: HashMap::new(),
                lookup_table: None,
                modulo_z: None,
                modulo_c: None,
                modulo_t: None,
            };
            quicktime_decompress_cinepak(first_sample, &probe_meta, 0, depth, None)?;
        }
        QuickTimeCodec::Rpza => {
            let probe_meta = ImageMetadata {
                size_x: width,
                size_y: height,
                size_z: 1,
                size_c: 3,
                size_t: sample_sizes.len() as u32,
                pixel_type: PixelType::Uint8,
                bits_per_pixel: 8,
                image_count: sample_sizes.len() as u32,
                dimension_order: DimensionOrder::XYZCT,
                is_rgb: true,
                is_interleaved: true,
                is_indexed: false,
                is_little_endian: false,
                resolution_count: 1,
                thumbnail: false,
                series_metadata: HashMap::new(),
                lookup_table: None,
                modulo_z: None,
                modulo_c: None,
                modulo_t: None,
            };
            quicktime_decompress_rpza(first_sample, &probe_meta, 0)?;
        }
        QuickTimeCodec::AnimationRle { depth } => {
            let probe_meta = ImageMetadata {
                size_x: width,
                size_y: height,
                size_z: 1,
                size_c: 3,
                size_t: sample_sizes.len() as u32,
                pixel_type: PixelType::Uint8,
                bits_per_pixel: 8,
                image_count: sample_sizes.len() as u32,
                dimension_order: DimensionOrder::XYZCT,
                is_rgb: true,
                is_interleaved: true,
                is_indexed: false,
                is_little_endian: false,
                resolution_count: 1,
                thumbnail: false,
                series_metadata: HashMap::new(),
                lookup_table: None,
                modulo_z: None,
                modulo_c: None,
                modulo_t: None,
            };
            quicktime_decompress_qtrle(first_sample, &probe_meta, 0, depth, None)?;
        }
        _ => {}
    }

    let pixel_type = PixelType::Uint8;
    let mut metadata = HashMap::new();
    metadata.insert(
        "quicktime.codec".into(),
        MetadataValue::String(String::from_utf8_lossy(&codec).into_owned()),
    );
    metadata.insert(
        "quicktime.codec_family".into(),
        MetadataValue::String(quicktime_codec_family(&codec).into()),
    );
    if let Some(second_description) = descriptions.get(1) {
        metadata.insert(
            "Second codec".into(),
            MetadataValue::String(
                String::from_utf8_lossy(&second_description.codec_fourcc).into_owned(),
            ),
        );
    }
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
    quicktime_insert_u32_list_metadata(&mut metadata, "quicktime.sample_sizes", &sample_sizes);
    quicktime_insert_u64_list_metadata(&mut metadata, "quicktime.chunk_offsets", &chunk_offsets);
    quicktime_insert_u64_list_metadata(&mut metadata, "quicktime.sample_offsets", &sample_offsets);
    metadata.insert(
        "quicktime.chunk_offset_table_type".into(),
        MetadataValue::String(chunk_offset_table_type.into()),
    );
    if let QuickTimeCodec::Cinepak { depth } = qt_codec {
        metadata.insert(
            "quicktime.cinepak.depth".into(),
            MetadataValue::Int(depth as i64),
        );
    }
    if let QuickTimeCodec::AnimationRle { depth } = qt_codec {
        metadata.insert(
            "quicktime.rle.depth".into(),
            MetadataValue::Int(depth as i64),
        );
    }
    let media_header = descendant(trak, &[*b"mdia", *b"mdhd"])?
        .map(|atom| parse_quicktime_time_header(atom, "mdhd"))
        .transpose()?;
    let stts_entries = descendant(trak, &[*b"mdia", *b"minf", *b"stbl", *b"stts"])?
        .map(|atom| parse_quicktime_stts(atom, sample_sizes.len()))
        .transpose()?;
    let ctts_entries = descendant(trak, &[*b"mdia", *b"minf", *b"stbl", *b"ctts"])?
        .map(|atom| parse_quicktime_ctts(atom, sample_sizes.len()))
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
    let mut sample_read_order = None;
    if let Some(entries) = &stts_entries {
        metadata.insert(
            "quicktime.stts.entries".into(),
            MetadataValue::String(
                entries
                    .iter()
                    .map(|entry| format!("{}x{}", entry.sample_count, entry.sample_delta))
                    .collect::<Vec<_>>()
                    .join(","),
            ),
        );
        if let Some(duration) = quicktime_stts_total_duration(entries) {
            quicktime_insert_u64_metadata(&mut metadata, "quicktime.stts.duration_ticks", duration);
            if let Some((timescale, _)) = media_header {
                metadata.insert(
                    "quicktime.average_frame_duration_seconds".into(),
                    MetadataValue::Float(
                        duration as f64 / f64::from(timescale) / sample_sizes.len() as f64,
                    ),
                );
            }
        }
        if let Some(sample_media_times) = quicktime_sample_media_times(entries) {
            quicktime_insert_u64_list_metadata(
                &mut metadata,
                "quicktime.sample_media_time_ticks",
                &sample_media_times,
            );
            let sample_presentation_result = if let Some(edit_entries) = &edit_entries {
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
                    .map(|presentation_times| QuickTimeEditPresentationResult {
                        presentation_times,
                        sample_read_order: None,
                    })
            };
            if let Some(sample_presentation_result) = sample_presentation_result {
                if sample_read_order.is_none() {
                    sample_read_order = sample_presentation_result.sample_read_order;
                }
                let sample_presentation_times = sample_presentation_result.presentation_times;
                let sample_presentation_times = if let Some(ctts_entries) = &ctts_entries {
                    if let Some(composition_offsets) =
                        quicktime_sample_composition_offsets(ctts_entries)
                    {
                        metadata.insert(
                            "quicktime.ctts.entries".into(),
                            MetadataValue::String(
                                ctts_entries
                                    .iter()
                                    .map(|entry| {
                                        format!("{}x{}", entry.sample_count, entry.sample_offset)
                                    })
                                    .collect::<Vec<_>>()
                                    .join(","),
                            ),
                        );
                        quicktime_insert_i64_list_metadata(
                            &mut metadata,
                            "quicktime.sample_composition_offset_ticks",
                            &composition_offsets,
                        );
                        match quicktime_apply_composition_offsets(
                            &sample_presentation_times,
                            &composition_offsets,
                        ) {
                            Some(times) => {
                                metadata.insert(
                                    "quicktime.ctts.presentation_status".into(),
                                    MetadataValue::String("applied".into()),
                                );
                                times
                            }
                            None => {
                                metadata.insert(
                                    "quicktime.ctts.presentation_status".into(),
                                    MetadataValue::String("not_applied_overflow".into()),
                                );
                                sample_presentation_times
                            }
                        }
                    } else {
                        metadata.insert(
                            "quicktime.ctts.presentation_status".into(),
                            MetadataValue::String("not_applied_overflow".into()),
                        );
                        sample_presentation_times
                    }
                } else {
                    sample_presentation_times
                };
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
        metadata.insert(
            "quicktime.edit_list.entries".into(),
            MetadataValue::String(
                entries
                    .iter()
                    .map(|entry| {
                        format!(
                            "duration={},media_time={},rate={}",
                            entry.segment_duration, entry.media_time, entry.media_rate
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(";"),
            ),
        );
    }
    if let Some(entries) = &stsc_entries {
        metadata.insert(
            "quicktime.stsc.entry_count".into(),
            MetadataValue::Int(entries.len() as i64),
        );
        metadata.insert(
            "quicktime.stsc.entries".into(),
            MetadataValue::String(
                entries
                    .iter()
                    .map(|entry| {
                        format!(
                            "first_chunk={},samples_per_chunk={},sample_description_index={}",
                            entry.first_chunk,
                            entry.samples_per_chunk,
                            entry.sample_description_index
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(";"),
            ),
        );
    }
    let displayed_sample_count = sample_read_order
        .as_ref()
        .map(Vec::len)
        .unwrap_or(sample_sizes.len());
    let meta = ImageMetadata {
        size_x: width,
        size_y: height,
        size_z: 1,
        size_c: samples_per_pixel as u32,
        size_t: displayed_sample_count as u32,
        pixel_type,
        bits_per_pixel: 8,
        image_count: displayed_sample_count as u32,
        dimension_order: DimensionOrder::XYZCT,
        is_rgb: samples_per_pixel >= 3,
        is_interleaved: samples_per_pixel > 1,
        is_indexed: false,
        is_little_endian: false,
        resolution_count: 1,
        thumbnail: false,
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
        sample_read_order,
        sample_codecs,
        sample_depths,
        samples_per_pixel,
        codec: qt_codec,
        depth: first_description.depth,
    })
}

// ---------------------------------------------------------------------------
// 1b. Apple QuickTime writer (port of loci.formats.out.QTWriter)
// ---------------------------------------------------------------------------
/// Apple QuickTime movie writer (`.mov`).
///
/// Faithful port of the Java `QTWriter` (formats-bsd) uncompressed/RAW path.
/// Accumulates planes, then on [`close`](FormatWriter::close) writes a
/// `wide`/`mdat` pixel container followed by the full `moov`/`trak`/`mdia`/
/// `minf`/`stbl` atom tree (`stsd`/`stts`/`stsc`/`stsz`/`stco`), mirroring
/// Java's atom layout and offsets byte-for-byte.
///
/// Only the RAW (`"raw "`) codec is implemented, matching Java's default
/// `CODEC_RAW` path; the lossy/encoded codecs (Motion JPEG-B, Cinepak,
/// Animation, H.263, Sorenson, Sorenson 3, MPEG-4) require encoders that this
/// pure-Rust port does not provide and are rejected with an explicit
/// "unsupported compression" error rather than faked.
///
/// As in Java, grayscale (single-channel) planes are written with each pixel
/// inverted (`255 - p`) and rows padded to a multiple of 4 bytes; RGB planes
/// are written verbatim with no padding. Note that the bundled
/// [`QtReader`] maps the `"raw "` codec to interleaved RGB, so RGB
/// output round-trips through it directly, whereas Java-style inverted
/// grayscale would be re-read as 3-channel RGB.
pub struct QtWriter {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    planes: Vec<Vec<u8>>,
    /// Frames per second used for the duration / `stts` timing (default: 10).
    fps: f64,
}

impl QtWriter {
    pub fn new() -> Self {
        QtWriter {
            path: None,
            meta: None,
            planes: Vec::new(),
            fps: 10.0,
        }
    }

    /// Set frames per second (default: 10).
    pub fn with_fps(mut self, fps: f64) -> Self {
        self.fps = fps;
        self
    }
}

impl Default for QtWriter {
    fn default() -> Self {
        Self::new()
    }
}

/// Number of channels stored per pixel (`getSamplesPerPixel()` in Java).
fn qt_writer_channels(meta: &ImageMetadata) -> usize {
    if meta.is_rgb {
        meta.size_c.max(1) as usize
    } else {
        1
    }
}

/// Validate that the metadata is writable by the RAW path. Mirrors Java's
/// `getPixelTypes` (UINT8 only) plus the codec restriction in `saveBytes`.
fn validate_qt_writer_metadata(meta: &ImageMetadata) -> Result<()> {
    if meta.pixel_type != PixelType::Uint8 {
        return Err(BioFormatsError::UnsupportedFormat(
            "QuickTime writer supports only 8-bit (UINT8) pixel data".into(),
        ));
    }
    let nchannels = qt_writer_channels(meta);
    if meta.is_rgb && nchannels != 3 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "QuickTime writer supports grayscale or 3-channel RGB UINT8 data, got {nchannels} RGB channels"
        )));
    }
    crate::formats::stack_writer::expected_plane_count("QuickTime", meta)?;
    Ok(())
}

/// Write the 3x3 fixed-point matrix describing image rotation (identity).
/// Port of `QTWriter.writeRotationMatrix`.
fn qt_write_rotation_matrix(out: &mut Vec<u8>) {
    out.extend_from_slice(&1i32.to_be_bytes());
    out.extend_from_slice(&0i32.to_be_bytes());
    out.extend_from_slice(&0i32.to_be_bytes());
    out.extend_from_slice(&0i32.to_be_bytes());
    out.extend_from_slice(&1i32.to_be_bytes());
    out.extend_from_slice(&0i32.to_be_bytes());
    out.extend_from_slice(&0i32.to_be_bytes());
    out.extend_from_slice(&0i32.to_be_bytes());
    out.extend_from_slice(&16384i32.to_be_bytes());
}

/// Write the atom length and 4-byte type. Port of `QTWriter.writeAtom`.
fn qt_write_atom(out: &mut Vec<u8>, length: i32, atom_type: &str) {
    out.extend_from_slice(&length.to_be_bytes());
    out.extend_from_slice(atom_type.as_bytes());
}

impl crate::common::writer::FormatWriter for QtWriter {
    fn is_this_type(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("mov"))
    }

    fn set_metadata(&mut self, meta: &ImageMetadata) -> Result<()> {
        validate_qt_writer_metadata(meta)?;
        self.meta = Some(meta.clone());
        self.planes.clear();
        Ok(())
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn save_bytes(&mut self, plane_index: u32, data: &[u8]) -> Result<()> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        crate::formats::stack_writer::validate_next_plane(
            "QuickTime",
            meta,
            self.planes.len(),
            plane_index,
            data.len(),
        )?;
        self.planes.push(data.to_vec());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        // Nothing buffered and nothing to flush: allow idempotent close.
        let meta = match self.meta.as_ref() {
            Some(m) => m,
            None => {
                self.path = None;
                self.planes.clear();
                return Ok(());
            }
        };
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;

        validate_qt_writer_metadata(meta)?;
        crate::formats::stack_writer::validate_complete("QuickTime", meta, self.planes.len())?;

        let width = meta.size_x as i32;
        let height = meta.size_y as i32;
        let nchannels = qt_writer_channels(meta);

        // pad = nChannels > 1 ? 0 : (4 - (width % 4)) % 4;  (QTWriter.setId)
        let pad: i32 = if nchannels > 1 {
            0
        } else {
            (4 - (width % 4)) % 4
        };

        let plane_size = (width * height * nchannels as i32) as i64;
        let stored_plane = plane_size + (pad as i64) * (height as i64);
        let num_written = self.planes.len() as i32;
        // Total pixel bytes (Java `numBytes`).
        let num_bytes = stored_plane * (num_written as i64);

        // -- assemble the whole file in memory (Java seeks/streams via `out`) --
        let mut out: Vec<u8> = Vec::new();

        // -- write the first header (QTWriter.setId, fresh file branch) --
        // writeAtom(8, "wide"); writeAtom(numBytes + 8, "mdat");
        qt_write_atom(&mut out, 8, "wide");
        qt_write_atom(&mut out, (num_bytes + 8) as i32, "mdat");
        debug_assert_eq!(out.len(), 16);

        // Plane offsets: 16 + i * (planeSize + pad * height).  (QTWriter.setId)
        let offsets: Vec<i32> = (0..num_written)
            .map(|i| 16 + i * (stored_plane as i32))
            .collect();

        // -- write the mdat pixel payload, plane by plane (QTWriter.saveBytes) --
        // Java inverts single-channel pixels and pads each row to a 4-byte
        // multiple; RGB is copied verbatim with pad == 0. Input is assumed to
        // be a full interleaved plane (interleaved == true, full plane), so the
        // generic x/y/w/h sub-region path collapses to a straight row copy.
        let row_len = (nchannels as i32 * width) as usize;
        for plane in &self.planes {
            let plane = if meta.is_rgb && !meta.is_interleaved {
                crate::common::writer::to_interleaved_samples(meta, plane)?
            } else {
                plane.clone()
            };
            for row in 0..height as usize {
                let src = &plane[row * row_len..row * row_len + row_len];
                if nchannels == 1 {
                    out.extend(src.iter().map(|&b| 255u8.wrapping_sub(b)));
                } else {
                    out.extend_from_slice(src);
                }
                for _ in 0..pad {
                    out.push(0);
                }
            }
        }
        debug_assert_eq!(out.len() as i64, 16 + num_bytes);

        // -- write footer (QTWriter.writeFooter) --
        let time_scale: i32 = 1000;
        let duration: i32 = (num_written as f64 * (time_scale as f64 / self.fps)) as i32;
        let bits_per_pixel: i32 = if nchannels > 1 { 24 } else { 40 };
        let channels: i32 = if bits_per_pixel >= 40 { 1 } else { 3 };
        // `created` mirrors Java's `(int) System.currentTimeMillis()` truncation.
        let created: i32 = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i32)
            .unwrap_or(0);
        let modified: i32 = created;

        // -- moov atom --
        let mut atom_length: i32 = 685 + 8 * num_written;
        qt_write_atom(&mut out, atom_length, "moov");

        // -- mvhd atom --
        qt_write_atom(&mut out, 108, "mvhd");
        out.extend_from_slice(&0i16.to_be_bytes()); // version
        out.extend_from_slice(&0i16.to_be_bytes()); // flags
        out.extend_from_slice(&created.to_be_bytes()); // creation time
        out.extend_from_slice(&modified.to_be_bytes());
        out.extend_from_slice(&time_scale.to_be_bytes()); // time scale
        out.extend_from_slice(&duration.to_be_bytes()); // duration
        out.extend_from_slice(&[0, 1, 0, 0]); // preferred rate & volume
        out.extend_from_slice(&[0, 0xFF, 0, 0, 0, 0, 0, 0, 0, 0]); // reserved (Java {0,-1,...})
        qt_write_rotation_matrix(&mut out);
        out.extend_from_slice(&0i16.to_be_bytes()); // not sure what this is
        out.extend_from_slice(&0i32.to_be_bytes()); // preview duration
        out.extend_from_slice(&0i32.to_be_bytes()); // preview time
        out.extend_from_slice(&0i32.to_be_bytes()); // poster time
        out.extend_from_slice(&0i32.to_be_bytes()); // selection time
        out.extend_from_slice(&0i32.to_be_bytes()); // selection duration
        out.extend_from_slice(&0i32.to_be_bytes()); // current time
        out.extend_from_slice(&2i32.to_be_bytes()); // next track's id

        // -- trak atom --
        atom_length -= 116;
        qt_write_atom(&mut out, atom_length, "trak");

        // -- tkhd atom --
        qt_write_atom(&mut out, 92, "tkhd");
        out.extend_from_slice(&0i16.to_be_bytes()); // version
        out.extend_from_slice(&15i16.to_be_bytes()); // flags
        out.extend_from_slice(&created.to_be_bytes()); // creation time
        out.extend_from_slice(&modified.to_be_bytes());
        out.extend_from_slice(&1i32.to_be_bytes()); // track id
        out.extend_from_slice(&0i32.to_be_bytes()); // reserved
        out.extend_from_slice(&duration.to_be_bytes()); // duration
        out.extend_from_slice(&0i32.to_be_bytes()); // reserved
        out.extend_from_slice(&0i32.to_be_bytes()); // reserved
        out.extend_from_slice(&0i16.to_be_bytes()); // reserved
        out.extend_from_slice(&0i32.to_be_bytes()); // unknown
        qt_write_rotation_matrix(&mut out);
        out.extend_from_slice(&width.to_be_bytes()); // image width
        out.extend_from_slice(&height.to_be_bytes()); // image height
        out.extend_from_slice(&0i16.to_be_bytes()); // reserved

        // -- edts atom --
        qt_write_atom(&mut out, 36, "edts");

        // -- elst atom --
        qt_write_atom(&mut out, 28, "elst");
        out.extend_from_slice(&0i16.to_be_bytes()); // version
        out.extend_from_slice(&0i16.to_be_bytes()); // flags
        out.extend_from_slice(&1i32.to_be_bytes()); // number of entries in the table
        out.extend_from_slice(&duration.to_be_bytes()); // duration
        out.extend_from_slice(&0i16.to_be_bytes()); // time
        out.extend_from_slice(&1i32.to_be_bytes()); // rate
        out.extend_from_slice(&0i16.to_be_bytes()); // unknown

        // -- mdia atom --
        atom_length -= 136;
        qt_write_atom(&mut out, atom_length, "mdia");

        // -- mdhd atom --
        qt_write_atom(&mut out, 32, "mdhd");
        out.extend_from_slice(&0i16.to_be_bytes()); // version
        out.extend_from_slice(&0i16.to_be_bytes()); // flags
        out.extend_from_slice(&created.to_be_bytes()); // creation time
        out.extend_from_slice(&modified.to_be_bytes());
        out.extend_from_slice(&time_scale.to_be_bytes()); // time scale
        out.extend_from_slice(&duration.to_be_bytes()); // duration
        out.extend_from_slice(&0i16.to_be_bytes()); // language
        out.extend_from_slice(&0i16.to_be_bytes()); // quality

        // -- hdlr atom (media handler) --
        qt_write_atom(&mut out, 58, "hdlr");
        out.extend_from_slice(&0i16.to_be_bytes()); // version
        out.extend_from_slice(&0i16.to_be_bytes()); // flags
        out.extend_from_slice(b"mhlr");
        out.extend_from_slice(b"vide");
        out.extend_from_slice(b"appl");
        out.extend_from_slice(&[16, 0, 0, 0, 0, 1, 1, 11, 25]);
        out.extend_from_slice(b"Apple Video Media Handler");

        // -- minf atom --
        atom_length -= 98;
        qt_write_atom(&mut out, atom_length, "minf");

        // -- vmhd atom --
        qt_write_atom(&mut out, 20, "vmhd");
        out.extend_from_slice(&0i16.to_be_bytes()); // version
        out.extend_from_slice(&1i16.to_be_bytes()); // flags
        out.extend_from_slice(&64i16.to_be_bytes()); // graphics mode
        out.extend_from_slice(&(32768u16).to_be_bytes()); // opcolor 1
        out.extend_from_slice(&(32768u16).to_be_bytes()); // opcolor 2
        out.extend_from_slice(&(32768u16).to_be_bytes()); // opcolor 3

        // -- hdlr atom (data handler) --
        qt_write_atom(&mut out, 57, "hdlr");
        out.extend_from_slice(&0i16.to_be_bytes()); // version
        out.extend_from_slice(&0i16.to_be_bytes()); // flags
        out.extend_from_slice(b"dhlr");
        out.extend_from_slice(b"alis");
        out.extend_from_slice(b"appl");
        out.extend_from_slice(&[16, 0, 0, 1, 0, 1, 1, 31, 24]);
        out.extend_from_slice(b"Apple Alias Data Handler");

        // -- dinf atom --
        qt_write_atom(&mut out, 36, "dinf");

        // -- dref atom --
        qt_write_atom(&mut out, 28, "dref");
        out.extend_from_slice(&0i16.to_be_bytes()); // version
        out.extend_from_slice(&0i16.to_be_bytes()); // flags
        out.extend_from_slice(&0i16.to_be_bytes()); // version 2
        out.extend_from_slice(&1i16.to_be_bytes()); // flags 2
        out.extend_from_slice(&[0, 0, 0, 12]);
        out.extend_from_slice(b"alis");
        out.extend_from_slice(&0i16.to_be_bytes()); // version 3
        out.extend_from_slice(&1i16.to_be_bytes()); // flags 3

        // -- stbl atom --
        atom_length -= 121;
        qt_write_atom(&mut out, atom_length, "stbl");

        // -- stsd atom --
        qt_write_atom(&mut out, 118, "stsd");
        out.extend_from_slice(&0i16.to_be_bytes()); // version
        out.extend_from_slice(&0i16.to_be_bytes()); // flags
        out.extend_from_slice(&1i32.to_be_bytes()); // number of entries in the table
        out.extend_from_slice(&[0, 0, 0, 102]);
        out.extend_from_slice(b"raw "); // codec
        out.extend_from_slice(&[0, 0, 0, 0, 0, 0]); // reserved
        out.extend_from_slice(&1i16.to_be_bytes()); // data reference
        out.extend_from_slice(&1i16.to_be_bytes()); // version
        out.extend_from_slice(&1i16.to_be_bytes()); // revision
        out.extend_from_slice(b"appl");
        out.extend_from_slice(&0i32.to_be_bytes()); // temporal quality
        out.extend_from_slice(&768i32.to_be_bytes()); // spatial quality
        out.extend_from_slice(&(width as i16).to_be_bytes()); // image width
        out.extend_from_slice(&(height as i16).to_be_bytes()); // image height
        let dpi = [0u8, 72, 0, 0];
        out.extend_from_slice(&dpi); // horizontal dpi
        out.extend_from_slice(&dpi); // vertical dpi
        out.extend_from_slice(&0i32.to_be_bytes()); // data size
        out.extend_from_slice(&1i16.to_be_bytes()); // frames per sample
        out.extend_from_slice(&12i16.to_be_bytes()); // length of compressor name
        out.extend_from_slice(b"Uncompressed"); // compressor name
        out.extend_from_slice(&bits_per_pixel.to_be_bytes());
        out.extend_from_slice(&bits_per_pixel.to_be_bytes());
        out.extend_from_slice(&bits_per_pixel.to_be_bytes());
        out.extend_from_slice(&bits_per_pixel.to_be_bytes());
        out.extend_from_slice(&bits_per_pixel.to_be_bytes());
        out.extend_from_slice(&(bits_per_pixel as i16).to_be_bytes()); // bits per pixel
        out.extend_from_slice(&65535i32.to_be_bytes()); // ctab ID
        out.extend_from_slice(&[12, 103, 97, 108]); // gamma
        out.extend_from_slice(&[97, 1, 0xCC, 0xCC, 0, 0, 0, 0]); // unknown (Java {97,1,-52,-52,...})

        // -- stts atom --
        qt_write_atom(&mut out, 24, "stts");
        out.extend_from_slice(&0i16.to_be_bytes()); // version
        out.extend_from_slice(&0i16.to_be_bytes()); // flags
        out.extend_from_slice(&1i32.to_be_bytes()); // number of entries in the table
        out.extend_from_slice(&num_written.to_be_bytes()); // number of planes
        out.extend_from_slice(&((time_scale as f64 / self.fps) as i32).to_be_bytes()); // ms per frame

        // -- stsc atom --
        qt_write_atom(&mut out, 28, "stsc");
        out.extend_from_slice(&0i16.to_be_bytes()); // version
        out.extend_from_slice(&0i16.to_be_bytes()); // flags
        out.extend_from_slice(&1i32.to_be_bytes()); // number of entries in the table
        out.extend_from_slice(&1i32.to_be_bytes()); // chunk
        out.extend_from_slice(&1i32.to_be_bytes()); // samples
        out.extend_from_slice(&1i32.to_be_bytes()); // id

        // -- stsz atom --
        qt_write_atom(&mut out, 20 + 4 * num_written, "stsz");
        out.extend_from_slice(&0i16.to_be_bytes()); // version
        out.extend_from_slice(&0i16.to_be_bytes()); // flags
        out.extend_from_slice(&0i32.to_be_bytes()); // sample size
        out.extend_from_slice(&num_written.to_be_bytes()); // number of planes
        for _ in 0..num_written {
            out.extend_from_slice(&(channels * height * (width + pad)).to_be_bytes());
        }

        // -- stco atom --
        qt_write_atom(&mut out, 16 + 4 * num_written, "stco");
        out.extend_from_slice(&0i16.to_be_bytes()); // version
        out.extend_from_slice(&0i16.to_be_bytes()); // flags
        out.extend_from_slice(&num_written.to_be_bytes()); // number of planes
        for off in &offsets {
            out.extend_from_slice(&off.to_be_bytes());
        }

        std::fs::write(path, &out).map_err(BioFormatsError::Io)?;

        self.path = None;
        self.meta = None;
        self.planes.clear();
        Ok(())
    }

    fn can_do_stacks(&self) -> bool {
        true
    }
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
        fn planarize_u8(src: &[u8], pixels: usize, bands: usize) -> Vec<u8> {
            let mut out = vec![0u8; pixels * bands];
            for p in 0..pixels {
                for b in 0..bands {
                    out[b * pixels + p] = src[p * bands + b];
                }
            }
            out
        }
        fn planarize_u16(src: &[u16], pixels: usize, bands: usize) -> Vec<u8> {
            let mut out = vec![0u8; pixels * bands * 2];
            for p in 0..pixels {
                for b in 0..bands {
                    let be = src[p * bands + b].to_be_bytes();
                    let dst = (b * pixels + p) * 2;
                    out[dst] = be[0];
                    out[dst + 1] = be[1];
                }
            }
            out
        }
        match pt {
            PixelType::Uint8 => match img {
                image::DynamicImage::ImageLuma8(buf) => buf.as_raw().clone(),
                image::DynamicImage::ImageLumaA8(buf) => planarize_u8(buf.as_raw(), pixels, 2),
                image::DynamicImage::ImageRgb8(buf) => planarize_u8(buf.as_raw(), pixels, 3),
                image::DynamicImage::ImageRgba8(buf) => planarize_u8(buf.as_raw(), pixels, 4),
                _ => planarize_u8(img.to_rgba8().as_raw(), pixels, bands.min(4)),
            },
            PixelType::Uint16 => match img {
                image::DynamicImage::ImageLuma16(buf) => {
                    buf.as_raw().iter().flat_map(|v| v.to_be_bytes()).collect()
                }
                image::DynamicImage::ImageLumaA16(buf) => planarize_u16(buf.as_raw(), pixels, 2),
                image::DynamicImage::ImageRgb16(buf) => planarize_u16(buf.as_raw(), pixels, 3),
                image::DynamicImage::ImageRgba16(buf) => planarize_u16(buf.as_raw(), pixels, 4),
                _ => planarize_u16(img.to_rgba16().as_raw(), pixels, bands.min(4)),
            },
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
/// 3i SlideBook reader (`.sld`, `.spl`).
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
/// uncompressed and read directly by byte offset. Spool files (`.spl`) may
/// carry Java's 256-byte in-plane metadata records, which are skipped by their
/// `SLD_MAGIC_BYTES_3` marker.
///
/// NOT PORTED (Java lines ~758-1207): the extensive heuristic dimension
/// disambiguation, montage handling, image-name based series flattening,
/// and physical-size/channel-name metadata. When the recovered geometry cannot
/// be factored cleanly into the available planes, this reader returns an honest
/// `UnsupportedFormat`/`Format` error rather than fabricating a layout.
struct SlideBookSeries {
    meta: ImageMetadata,
    plane_offsets: Vec<usize>,
    plane_bytes: usize,
}

pub struct SlidebookReader {
    path: Option<PathBuf>,
    series: Vec<SlideBookSeries>,
    current: usize,
}

impl SlidebookReader {
    pub fn new() -> Self {
        SlidebookReader {
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

    fn is_spool(path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("spl"))
            .unwrap_or(false)
    }

    fn is_spool_metadata(data: &[u8], offset: usize) -> bool {
        const SLD_MAGIC_BYTES_3: u32 = 0xf6010101;
        offset
            .checked_add(4)
            .and_then(|end| data.get(offset..end))
            .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]) == SLD_MAGIC_BYTES_3)
            .unwrap_or(false)
    }

    fn plane_offsets_for_block(
        data: &[u8],
        start: usize,
        length: usize,
        plane_bytes: usize,
        is_spool: bool,
    ) -> Vec<usize> {
        let Some(block_end) = start.checked_add(length).map(|end| end.min(data.len())) else {
            return Vec::new();
        };
        if plane_bytes == 0 || start >= block_end {
            return Vec::new();
        }

        if !is_spool {
            let plane_count = (block_end - start) / plane_bytes;
            return (0..plane_count).map(|p| start + p * plane_bytes).collect();
        }

        let mut offsets = Vec::new();
        let mut pos = start;
        while pos < block_end {
            for _ in 0..8 {
                if pos + 256 <= block_end && Self::is_spool_metadata(data, pos) {
                    pos += 256;
                } else {
                    break;
                }
            }
            if pos + plane_bytes > block_end {
                break;
            }
            offsets.push(pos);
            pos += plane_bytes;
        }
        offsets
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
        let is_spool = Self::is_spool(path);
        let (little, metadata_offsets, mut pixel_offsets, mut pixel_lengths) =
            Self::scan_offsets(&data)?;

        // Drop pixel blocks that run off the end of the file (padding = 7 for
        // non-spool .sld files, 0 for spool .spl files).
        let mut i = 0;
        while i < pixel_offsets.len() {
            let length = pixel_lengths.get(i).copied().unwrap_or(0);
            let offset = pixel_offsets[i];
            let padding = if is_spool { 0 } else { 7 };
            if length + offset + padding > data.len() {
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
            let start = pixel_offsets[idx];
            let block_plane_offsets =
                Self::plane_offsets_for_block(&data, start, length, plane_bytes, is_spool);
            let plane_count = block_plane_offsets.len();
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

            let mut plane_offsets = Vec::with_capacity(image_count as usize);
            for p in 0..image_count as usize {
                plane_offsets.push(block_plane_offsets[p]);
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

impl Default for SlidebookReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for SlidebookReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("sld") | Some("spl"))
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

        // NetCDF-3 stores values big-endian, and Java MINCReader sets
        // littleEndian = isMINC2, so MINC-1 planes are exposed as big-endian
        // bytes.
        let pixels = raw.to_vec();

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
            is_little_endian: false,
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
        let row_bytes = meta.size_x as usize * bps;
        let mut out = vec![0u8; plane_bytes];
        for row in 0..meta.size_y as usize {
            let src_row = meta.size_y as usize - row - 1;
            let src = offset + src_row * row_bytes;
            let dst = row * row_bytes;
            out[dst..dst + row_bytes].copy_from_slice(&pixels[src..src + row_bytes]);
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
/// OME stage/detector object projection is intentionally limited to provenance:
/// this bounded tag parser distinguishes the safe LIFF fields it inspected
/// from fields that would be needed to construct stage or detector OME objects,
/// so it does not invent stage coordinates or detector identities.
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
const OL_USER: i32 = 72;
const OPENLAB_MAX_TAG_HEADERS: usize = 1024;

fn parse_axis_token(token: &str, axis: char) -> Option<u32> {
    let mut chars = token.chars();
    if chars.next()? != axis {
        return None;
    }
    let rest = chars.as_str().strip_prefix('=').unwrap_or(chars.as_str());
    rest.parse::<u32>().ok().filter(|v| *v > 0)
}

#[derive(Clone)]
struct OpenlabTagHeader {
    offset: usize,
    tag: i32,
    sub_tag: i32,
    next_offset: i64,
    format_code: String,
}

#[derive(Clone)]
struct OpenlabPlane {
    plane_offset: usize,
    tag: i32,
    sub_tag: i32,
    format_code: String,
    volume_type: i32,
    pict: bool,
    width: u32,
    height: u32,
    name: String,
    series: i32,
}

/// Accumulator for the `USER` / `CVariableList` global metadata Java reads via
/// `readVariable`. Mirrors the instance fields `gain`, `detectorOffset`,
/// `xPos`, `yPos`, `zPos` plus the `addGlobalMeta(name, value)` pairs.
#[derive(Default)]
struct OpenlabUserVars {
    /// All `(name, value)` pairs Java emits via `addGlobalMeta`, in order
    /// (including the synthesized "X/Y/Z position for position #1" keys).
    metas: Vec<(String, String)>,
    gain: Option<String>,
    detector_offset: Option<String>,
    x_pos: Option<String>,
    y_pos: Option<String>,
    z_pos: Option<String>,
}

pub struct OpenlabReader {
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

impl OpenlabReader {
    pub fn new() -> Self {
        OpenlabReader {
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

    fn tag_name(tag: i32) -> &'static str {
        match tag {
            OL_IMAGE_TYPE_1 => "IMAGE_TYPE_1",
            OL_IMAGE_TYPE_2 => "IMAGE_TYPE_2",
            OL_CALIBRATION => "CALIBRATION",
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

    /// Read one `CVariableList` entry (Java `readVariable`). Decodes the
    /// variable's class, value and name, records it via the same
    /// `addGlobalMeta`/instance-field assignments Java performs, and stores the
    /// results in `vars`. Returns `Err` on the same invalid-revision conditions
    /// Java raises a `FormatException` for.
    fn read_variable(c: &mut Cursor, vars: &mut OpenlabUserVars) -> Result<()> {
        let class_name = c.read_cstring();

        let name;
        let mut value = String::new();

        let derived_class_version = c.read();
        if derived_class_version != 1 {
            return Err(BioFormatsError::Format(
                "Openlab LIFF: invalid revision".into(),
            ));
        }

        if class_name == "CStringVariable" {
            let str_size = c.read_int();
            value = c.read_string(str_size.max(0) as usize);
            c.skip(1);
        } else if class_name == "CFloatVariable" {
            value = c.read_double().to_string();
        }

        let base_class_version = c.read();
        if base_class_version == 1 || base_class_version == 2 {
            let str_size = c.read_int();
            name = c.read_string(str_size.max(0) as usize);
            c.skip((base_class_version as i64) * 2 + 1);
        } else {
            return Err(BioFormatsError::Format(format!(
                "Openlab LIFF: invalid revision: {base_class_version}"
            )));
        }

        vars.metas.push((name.clone(), value.clone()));

        if name == "Gain" {
            vars.gain = Some(value);
        } else if name == "Offset" {
            vars.detector_offset = Some(value);
        } else if name == "X-Y Stage: X Position" {
            vars.x_pos = Some(value.clone());
            vars.metas
                .push(("X position for position #1".into(), value));
        } else if name == "X-Y Stage: Y Position" {
            vars.y_pos = Some(value.clone());
            vars.metas
                .push(("Y position for position #1".into(), value));
        } else if name == "ZPosition" {
            vars.z_pos = Some(value.clone());
            vars.metas
                .push(("Z position for position #1".into(), value));
        }

        Ok(())
    }

    fn parse(path: &Path) -> Result<OpenlabReader> {
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
        let mut tag_headers: Vec<OpenlabTagHeader> = Vec::new();
        let mut tag_headers_truncated = false;
        // Representative planes: (width, height, volume_type).
        let mut reps: Vec<(u32, u32, i32)> = Vec::new();
        let mut xcal = 0.0f32;
        let mut ycal = 0.0f32;
        let mut user_vars = OpenlabUserVars::default();
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

            if tag == OL_IMAGE_TYPE_1 || tag == OL_IMAGE_TYPE_2 {
                let format_code = fmt.trim_matches('\0').trim().to_string();
                let pict = format_code.eq_ignore_ascii_case("pict");
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
                    tag,
                    sub_tag,
                    format_code,
                    volume_type,
                    pict,
                    width,
                    height,
                    name,
                    series,
                });
            } else {
                if tag_headers.len() < OPENLAB_MAX_TAG_HEADERS {
                    tag_headers.push(OpenlabTagHeader {
                        offset: fp.max(0) as usize,
                        tag,
                        sub_tag,
                        next_offset: next_tag,
                        format_code: fmt.trim_matches('\0').trim().to_string(),
                    });
                } else {
                    tag_headers_truncated = true;
                }
                if tag == OL_CALIBRATION {
                    c.skip(4);
                    let units = c.read_short() as i32;
                    let scaling = if units == 3 { 0.001f32 } else { 1.0f32 };
                    c.skip(12);
                    xcal = c.read_float() * scaling;
                    ycal = c.read_float() * scaling;
                } else if tag == OL_USER {
                    let class_name = c.read_cstring();
                    if class_name == "CVariableList" {
                        let check = c.read() as i8;
                        if check == 1 {
                            let num_vars = c.read_short() as i32;
                            for _ in 0..num_vars {
                                Self::read_variable(&mut c, &mut user_vars)?;
                            }
                        }
                    }
                }
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

        let has_stage =
            user_vars.x_pos.is_some() || user_vars.y_pos.is_some() || user_vars.z_pos.is_some();
        let has_detector = user_vars.gain.is_some() || user_vars.detector_offset.is_some();

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
            // Combined stage+detector projection status (legacy key). Only the
            // CVariableList branch can supply explicit stage/detector fields; if
            // none were read it is faithful to report no safe LIFF fields.
            meta.series_metadata.insert(
                "openlab.ome.stage_detector_projection".into(),
                MetadataValue::String(
                    if has_stage || has_detector {
                        "projected_from_cvariablelist_stage_detector_fields"
                    } else {
                        "not_projected_no_safe_liff_fields"
                    }
                    .into(),
                ),
            );
            meta.series_metadata.insert(
                "openlab.ome.stage_detector_projection.source_fields".into(),
                MetadataValue::String(
                    "plane tag/sub_tag/format/name/offset; non-image tag headers; calibration physical_size_x/y; CVariableList stage/detector variables"
                        .into(),
                ),
            );
            // Stage X/Y/Z projection (Java sets PlanePositionX/Y/Z).
            meta.series_metadata.insert(
                "openlab.ome.stage_projection".into(),
                MetadataValue::String(
                    if has_stage {
                        "projected_from_cvariablelist_stage_positions"
                    } else {
                        "not_projected_no_explicit_stage_coordinates"
                    }
                    .into(),
                ),
            );
            meta.series_metadata.insert(
                "openlab.ome.stage_projection.inspected_fields".into(),
                MetadataValue::String(
                    if has_stage {
                        "CVariableList variables 'X-Y Stage: X Position', 'X-Y Stage: Y Position', 'ZPosition'"
                    } else {
                        "plane names may encode image/Z/C/T labels; calibration stores physical pixel size only"
                    }
                    .into(),
                ),
            );
            meta.series_metadata.insert(
                "openlab.ome.stage_projection.reason".into(),
                MetadataValue::String(
                    if has_stage {
                        "explicit stage X/Y/Z coordinates were read from the CVariableList USER tag and projected to PlanePositionX/Y/Z"
                    } else {
                        "no parsed LIFF field contains explicit stage X/Y/Z coordinates; plane names are only used for image/Z/C/T indexing and calibration values are pixel sizes"
                    }
                    .into(),
                ),
            );
            // Detector gain/offset projection (Java sets DetectorSettingsGain/Offset
            // on a Detector of type "Other").
            meta.series_metadata.insert(
                "openlab.ome.detector_projection".into(),
                MetadataValue::String(
                    if has_detector {
                        "projected_from_cvariablelist_gain_offset"
                    } else {
                        "not_projected_no_explicit_detector_fields"
                    }
                    .into(),
                ),
            );
            meta.series_metadata.insert(
                "openlab.ome.detector_projection.inspected_fields".into(),
                MetadataValue::String(
                    if has_detector {
                        "CVariableList variables 'Gain', 'Offset'; detector type set to 'Other'"
                    } else {
                        "volume_type and pixel_payload describe pixel storage, not detector identity"
                    }
                    .into(),
                ),
            );
            meta.series_metadata.insert(
                "openlab.ome.detector_projection.reason".into(),
                MetadataValue::String(
                    if has_detector {
                        "Gain and/or Offset were read from the CVariableList USER tag and projected to DetectorSettings on a Detector of type 'Other'"
                    } else {
                        "no parsed LIFF field contains detector model, type, gain, offset, or channel light-path identity; volume_type, pixel_payload, and tag format are storage descriptors"
                    }
                    .into(),
                ),
            );
            for (plane_index, &plane_idx) in list.iter().enumerate() {
                let plane = &planes[plane_idx];
                meta.series_metadata.insert(
                    format!("openlab.plane.{plane_index}.tag"),
                    MetadataValue::Int(plane.tag as i64),
                );
                meta.series_metadata.insert(
                    format!("openlab.plane.{plane_index}.tag_name"),
                    MetadataValue::String(Self::tag_name(plane.tag).into()),
                );
                meta.series_metadata.insert(
                    format!("openlab.plane.{plane_index}.sub_tag"),
                    MetadataValue::Int(plane.sub_tag as i64),
                );
                if !plane.format_code.is_empty() {
                    meta.series_metadata.insert(
                        format!("openlab.plane.{plane_index}.format"),
                        MetadataValue::String(plane.format_code.clone()),
                    );
                }
                if !plane.name.is_empty() {
                    meta.series_metadata.insert(
                        format!("openlab.plane.{plane_index}.name"),
                        MetadataValue::String(plane.name.clone()),
                    );
                }
                meta.series_metadata.insert(
                    format!("openlab.plane.{plane_index}.offset"),
                    MetadataValue::Int(plane.plane_offset as i64),
                );
            }
            if !tag_headers.is_empty() {
                meta.series_metadata.insert(
                    "openlab.tag_header.count".into(),
                    MetadataValue::Int(tag_headers.len() as i64),
                );
            }
            if tag_headers_truncated {
                meta.series_metadata.insert(
                    "openlab.tag_header.truncated".into(),
                    MetadataValue::Bool(true),
                );
            }
            for (tag_index, header) in tag_headers.iter().enumerate() {
                meta.series_metadata.insert(
                    format!("openlab.tag_header.{tag_index}.tag"),
                    MetadataValue::Int(header.tag as i64),
                );
                meta.series_metadata.insert(
                    format!("openlab.tag_header.{tag_index}.tag_name"),
                    MetadataValue::String(Self::tag_name(header.tag).into()),
                );
                meta.series_metadata.insert(
                    format!("openlab.tag_header.{tag_index}.sub_tag"),
                    MetadataValue::Int(header.sub_tag as i64),
                );
                if !header.format_code.is_empty() {
                    meta.series_metadata.insert(
                        format!("openlab.tag_header.{tag_index}.format"),
                        MetadataValue::String(header.format_code.clone()),
                    );
                }
                meta.series_metadata.insert(
                    format!("openlab.tag_header.{tag_index}.offset"),
                    MetadataValue::Int(header.offset as i64),
                );
                meta.series_metadata.insert(
                    format!("openlab.tag_header.{tag_index}.next_offset"),
                    MetadataValue::Int(header.next_offset),
                );
            }
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
                // CVariableList global metadata (Java `addGlobalMeta` pairs from
                // `readVariable`). Stored under the exact Java key names.
                for (name, value) in &user_vars.metas {
                    if name.is_empty() {
                        continue;
                    }
                    meta.series_metadata
                        .insert(name.clone(), MetadataValue::String(value.clone()));
                }
                // Typed accessors for the OME stage/detector projection. Mirror
                // Java's `gain`, `detectorOffset`, `xPos`, `yPos`, `zPos` fields.
                if let Some(g) = &user_vars.gain {
                    meta.series_metadata
                        .insert("openlab.gain".into(), MetadataValue::String(g.clone()));
                }
                if let Some(o) = &user_vars.detector_offset {
                    meta.series_metadata.insert(
                        "openlab.detector_offset".into(),
                        MetadataValue::String(o.clone()),
                    );
                }
                if let Some(x) = &user_vars.x_pos {
                    meta.series_metadata.insert(
                        "openlab.stage_position_x".into(),
                        MetadataValue::String(x.clone()),
                    );
                }
                if let Some(y) = &user_vars.y_pos {
                    meta.series_metadata.insert(
                        "openlab.stage_position_y".into(),
                        MetadataValue::String(y.clone()),
                    );
                }
                if let Some(z) = &user_vars.z_pos {
                    meta.series_metadata.insert(
                        "openlab.stage_position_z".into(),
                        MetadataValue::String(z.clone()),
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
                for (plane_index, &(z, c, t)) in coords.iter().enumerate() {
                    meta.series_metadata.insert(
                        format!("openlab.plane.{plane_index}.the_z"),
                        MetadataValue::Int(z as i64),
                    );
                    meta.series_metadata.insert(
                        format!("openlab.plane.{plane_index}.the_c"),
                        MetadataValue::Int(c as i64),
                    );
                    meta.series_metadata.insert(
                        format!("openlab.plane.{plane_index}.the_t"),
                        MetadataValue::Int(t as i64),
                    );
                }
                plane_zct.push(Some(coords));
            } else {
                plane_zct.push(None);
            }
            metas.push(meta);
        }

        Ok(OpenlabReader {
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
                let bytes = bpp * channels;
                let mut src = if h > 0 { b.len() / h } else { 0 };
                if src as i64 - (w * bytes) as i64 != 16 {
                    src = w * bytes;
                }
                let dest = w * bytes;
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

impl Default for OpenlabReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for OpenlabReader {
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
        use crate::common::ome_metadata::{
            create_lsid, OmeDetector, OmeInstrument, OmeMetadata, OmePlane, OmePlate,
        };

        let meta = self.metas.get(self.current)?;
        // Stage/detector global metadata is parsed once and stored on series 0
        // (Java keeps it on the single global MetadataStore). Read it from
        // there regardless of the current series so the projection is stable.
        let global = self.metas.first().unwrap_or(meta);
        let read_pos = |key: &str| -> Option<f64> {
            match global.series_metadata.get(key) {
                Some(MetadataValue::String(s)) => s.trim().parse::<f64>().ok(),
                _ => None,
            }
        };
        let gain = read_pos("openlab.gain");
        let detector_offset = read_pos("openlab.detector_offset");
        let stage_x = read_pos("openlab.stage_position_x");
        let stage_y = read_pos("openlab.stage_position_y");
        let stage_z = read_pos("openlab.stage_position_z");

        let mut ome = OmeMetadata::from_image_metadata(meta);

        // Detector projection (Java: Instrument + Detector type "Other" +
        // DetectorSettings gain/offset). We attach gain/offset to the single
        // detector with type "Other".
        if gain.is_some() || detector_offset.is_some() {
            let instrument_index = ome.instruments.len();
            ome.instruments.push(OmeInstrument {
                id: Some(create_lsid("Instrument", &[0])),
                detectors: vec![OmeDetector {
                    id: Some(create_lsid("Detector", &[0, 0])),
                    detector_type: Some("Other".into()),
                    gain,
                    offset: detector_offset,
                    ..Default::default()
                }],
                ..Default::default()
            });
            if let Some(image) = ome.images.get_mut(0) {
                image.instrument_ref = Some(instrument_index);
            }
        }

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
            // Stage-position projection (Java sets PlanePositionX/Y/Z on every
            // plane of every series). If no per-plane Z/C/T were inferred, emit
            // one plane carrying the positions so they are not lost.
            if stage_x.is_some() || stage_y.is_some() || stage_z.is_some() {
                if image.planes.is_empty() {
                    image.planes.push(OmePlane::default());
                }
                for plane in image.planes.iter_mut() {
                    plane.position_x = stage_x;
                    plane.position_y = stage_y;
                    plane.position_z = stage_z;
                }
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
            Some("jp2") | Some("j2k") | Some("jpf") | Some("j2c") | Some("jpc")
        )
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // J2C codestream: Java accepts the SOC marker.
        if header.len() >= 2 && header[..2] == [0xFF, 0x4F] {
            return true;
        }
        // JP2 container signature box, excluding JPX-branded files like Java.
        if header.len() >= 24 && header[..8] == [0x00, 0x00, 0x00, 0x0C, 0x6A, 0x50, 0x20, 0x20] {
            return &header[20..24] != b"jpx ";
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
        let signed = components[0].is_signed();
        let (pixel_type, bpp) = jpeg2000_pixel_type(prec, signed);
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
                    append_jpeg2000_sample(&mut pixels, val, bps, signed);
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
            dimension_order: DimensionOrder::XYCZT,
            is_rgb,
            is_interleaved: true,
            is_indexed: false,
            is_little_endian: false,
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

fn jpeg2000_pixel_type(precision: u8, signed: bool) -> (PixelType, u8) {
    if precision <= 8 {
        (
            if signed {
                PixelType::Int8
            } else {
                PixelType::Uint8
            },
            8,
        )
    } else if precision <= 16 {
        (
            if signed {
                PixelType::Int16
            } else {
                PixelType::Uint16
            },
            16,
        )
    } else {
        (
            if signed {
                PixelType::Int32
            } else {
                PixelType::Uint32
            },
            32,
        )
    }
}

fn append_jpeg2000_sample(out: &mut Vec<u8>, value: i32, bytes_per_sample: usize, signed: bool) {
    match (bytes_per_sample, signed) {
        (1, _) => out.push(value as u8),
        (2, true) => out.extend_from_slice(&(value as i16).to_be_bytes()),
        (2, false) => out.extend_from_slice(&(value as u16).to_be_bytes()),
        (_, true) => out.extend_from_slice(&value.to_be_bytes()),
        (_, false) => out.extend_from_slice(&(value as u32).to_be_bytes()),
    }
}

#[cfg(test)]
mod jpeg2000_tests {
    use super::*;

    #[test]
    fn jpeg2000_name_and_byte_detection_matches_java_contract() {
        let reader = Jpeg2000Reader::new();
        assert!(reader.is_this_type_by_name(Path::new("image.jpf")));
        assert!(reader.is_this_type_by_name(Path::new("image.jp2")));
        assert!(reader.is_this_type_by_bytes(&[0xff, 0x4f, 0x00, 0x00]));

        let mut jp2 = vec![0u8; 24];
        jp2[..8].copy_from_slice(&[0x00, 0x00, 0x00, 0x0c, 0x6a, 0x50, 0x20, 0x20]);
        jp2[20..24].copy_from_slice(b"jp2 ");
        assert!(reader.is_this_type_by_bytes(&jp2));
        jp2[20..24].copy_from_slice(b"jpx ");
        assert!(!reader.is_this_type_by_bytes(&jp2));
    }

    #[test]
    fn jpeg2000_pixel_type_and_sample_bytes_follow_java_big_endian() {
        assert_eq!(jpeg2000_pixel_type(8, false), (PixelType::Uint8, 8));
        assert_eq!(jpeg2000_pixel_type(8, true), (PixelType::Int8, 8));
        assert_eq!(jpeg2000_pixel_type(12, false), (PixelType::Uint16, 16));
        assert_eq!(jpeg2000_pixel_type(12, true), (PixelType::Int16, 16));
        assert_eq!(jpeg2000_pixel_type(17, false), (PixelType::Uint32, 32));
        assert_eq!(jpeg2000_pixel_type(17, true), (PixelType::Int32, 32));

        let mut out = Vec::new();
        append_jpeg2000_sample(&mut out, 0x1234, 2, false);
        append_jpeg2000_sample(&mut out, -2, 2, true);
        append_jpeg2000_sample(&mut out, 0x01020304, 4, false);
        assert_eq!(out, vec![0x12, 0x34, 0xff, 0xfe, 0x01, 0x02, 0x03, 0x04]);
    }
}

/// JPEG 2000 writer (`.jp2`, `.j2k`).
///
/// Encodes a single 2D plane (1 grayscale or 3 RGB components) to a lossless
/// JP2 file using the pure-Rust `openjp2` encoder, mirroring the lossless
/// output of Java `JPEG2000Writer`. Gated behind the default-on
/// `jpeg2000-write` feature.
#[cfg(feature = "jpeg2000-write")]
pub struct Jpeg2000Writer {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    wrote: bool,
}

#[cfg(feature = "jpeg2000-write")]
impl Jpeg2000Writer {
    pub fn new() -> Self {
        Jpeg2000Writer {
            path: None,
            meta: None,
            wrote: false,
        }
    }
}

#[cfg(feature = "jpeg2000-write")]
impl Default for Jpeg2000Writer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "jpeg2000-write")]
impl crate::common::writer::FormatWriter for Jpeg2000Writer {
    fn is_this_type(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(
            ext.as_deref(),
            Some("jp2") | Some("j2k") | Some("j2c") | Some("jpc")
        )
    }

    fn set_metadata(&mut self, meta: &ImageMetadata) -> Result<()> {
        let components = meta.size_c.max(1);
        if components != 1 && components != 3 {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "JPEG 2000 writer supports 1 (gray) or 3 (RGB) channels, got {components}"
            )));
        }
        if meta.size_z.max(1) > 1 || meta.size_t.max(1) > 1 {
            return Err(BioFormatsError::UnsupportedFormat(
                "JPEG 2000 writer supports a single 2D plane only".into(),
            ));
        }
        // Map pixel type to (precision, signed). JP2 stores integer samples.
        match meta.pixel_type {
            PixelType::Uint8
            | PixelType::Int8
            | PixelType::Uint16
            | PixelType::Int16
            | PixelType::Uint32
            | PixelType::Int32 => {}
            other => {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "JPEG 2000 writer does not support pixel type {other:?}"
                )));
            }
        }
        self.meta = Some(meta.clone());
        Ok(())
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.meta
            .as_ref()
            .ok_or_else(|| BioFormatsError::Format("set_metadata first".into()))?;
        self.path = Some(path.to_path_buf());
        self.wrote = false;
        Ok(())
    }

    fn save_bytes(&mut self, plane_index: u32, data: &[u8]) -> Result<()> {
        if plane_index != 0 || self.wrote {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;

        let components = meta.size_c.max(1);
        let (precision, signed) = match meta.pixel_type {
            PixelType::Uint8 => (8, false),
            PixelType::Int8 => (8, true),
            PixelType::Uint16 => (16, false),
            PixelType::Int16 => (16, true),
            PixelType::Uint32 => (32, false),
            PixelType::Int32 => (32, true),
            other => {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "JPEG 2000 writer does not support pixel type {other:?}"
                )));
            }
        };

        crate::common::codec::compress_jpeg2000(
            data,
            meta.size_x,
            meta.size_y,
            components,
            precision,
            signed,
            path,
        )?;
        self.wrote = true;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.wrote = false;
        Ok(())
    }

    fn can_do_stacks(&self) -> bool {
        false
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
    fn is_this_type_by_name(&self, _path: &Path) -> bool {
        false
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

        let size_y = i16::from_be_bytes([data[524], data[525]]) as i32;
        let size_x = i16::from_be_bytes([data[532], data[533]]) as i32;
        if size_x <= 0 || size_y <= 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "SM-Camera header has invalid image dimensions".to_string(),
            ));
        }
        let size_x = size_x as u32;
        let size_y = size_y as u32;

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
        let mut series_metadata = HashMap::new();
        series_metadata.insert("Image width".into(), MetadataValue::Int(size_x as i64));
        series_metadata.insert("Image height".into(), MetadataValue::Int(size_y as i64));

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
/// Plain text image reader (`.txt`, `.csv`).
///
/// Parses Bio-Formats TextReader-style coordinate tables.
///
/// A header row containing `x` and `y` coordinate columns identifies the table;
/// every other column is exposed as one Float32 channel. Missing sparse pixels
/// are initialized to NaN. Plain numeric grids are retained as a compatibility
/// fallback.
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
        matches!(ext.as_deref(), Some("txt") | Some("csv"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        std::str::from_utf8(header)
            .ok()
            .map(|text| {
                let lines = split_text_lines(text);
                matches!(parse_text_coordinate_table(&lines), Ok(Some(_)))
            })
            .unwrap_or(false)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let text = std::fs::read_to_string(path).map_err(BioFormatsError::Io)?;
        let (width, height, channels, pixel_data, metadata) = parse_text_pixels(&text)?;
        self.path = Some(path.to_path_buf());
        self.pixel_data = pixel_data;
        self.meta = Some(ImageMetadata {
            size_x: width,
            size_y: height,
            size_z: 1,
            size_c: channels,
            size_t: 1,
            pixel_type: PixelType::Float32,
            bits_per_pixel: 32,
            image_count: channels,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: false,
            resolution_count: 1,
            thumbnail: false,
            series_metadata: metadata,
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
        let plane_size = (meta.size_x as usize)
            .checked_mul(meta.size_y as usize)
            .and_then(|v| v.checked_mul(4))
            .ok_or_else(|| BioFormatsError::Format("TextReader: plane size overflows".into()))?;
        let start = plane_size
            .checked_mul(plane_index as usize)
            .ok_or_else(|| BioFormatsError::Format("TextReader: plane offset overflows".into()))?;
        let end = start
            .checked_add(plane_size)
            .ok_or_else(|| BioFormatsError::Format("TextReader: plane end overflows".into()))?;
        self.pixel_data
            .get(start..end)
            .map(|plane| plane.to_vec())
            .ok_or_else(|| {
                BioFormatsError::InvalidData("TextReader: plane buffer truncated".into())
            })
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

fn parse_text_pixels(
    text: &str,
) -> Result<(u32, u32, u32, Vec<u8>, HashMap<String, MetadataValue>)> {
    let lines = split_text_lines(text);

    if let Some(parsed) = parse_text_coordinate_table(&lines)? {
        return Ok(parsed);
    }
    parse_text_dense_grid(&lines)
}

fn split_text_lines(text: &str) -> Vec<Vec<String>> {
    text.lines()
        .filter_map(|line| {
            let tokens: Vec<String> = split_text_row(line)
                .into_iter()
                .map(|s| s.to_string())
                .collect();
            if tokens.is_empty() {
                None
            } else {
                Some(tokens)
            }
        })
        .collect()
}

fn split_text_row(line: &str) -> Vec<&str> {
    line.trim()
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|s| !s.is_empty())
        .collect()
}

fn parse_text_coordinate_table(
    lines: &[Vec<String>],
) -> Result<Option<(u32, u32, u32, Vec<u8>, HashMap<String, MetadataValue>)>> {
    let Some((header_index, data_index)) = find_text_table_header(lines) else {
        return Ok(None);
    };
    let header = &lines[header_index];
    let row_len = header.len();
    let x_index = header.iter().position(|token| token == "x");
    let y_index = header.iter().position(|token| token == "y");
    let (Some(x_index), Some(y_index)) = (x_index, y_index) else {
        if parse_text_numeric_row(header).is_some() {
            return Ok(None);
        }
        return Err(BioFormatsError::UnsupportedFormat(
            "TextReader: no X/Y coordinate columns found".into(),
        ));
    };
    let channel_columns: Vec<usize> = (0..row_len)
        .filter(|&i| i != x_index && i != y_index)
        .collect();
    let channels = u32::try_from(channel_columns.len())
        .map_err(|_| BioFormatsError::Format("TextReader: channel count overflows".into()))?;

    let mut parsed_rows: Vec<Vec<f64>> = Vec::new();
    let mut width = 0u32;
    let mut height = 0u32;
    for tokens in &lines[data_index..] {
        if tokens.len() != row_len {
            continue;
        }
        let Some(row) = parse_text_numeric_row(tokens) else {
            continue;
        };
        let x = row[x_index] as i32;
        let y = row[y_index] as i32;
        if x < 0 {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "TextReader: invalid X coordinate {x}"
            )));
        }
        if y < 0 {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "TextReader: invalid Y coordinate {y}"
            )));
        }
        width = width.max(x as u32 + 1);
        height = height.max(y as u32 + 1);
        parsed_rows.push(row);
    }
    if parsed_rows.is_empty() || width == 0 || height == 0 {
        return Err(BioFormatsError::UnsupportedFormat(
            "TextReader: file contains no tabular numeric data".into(),
        ));
    }

    let plane_values = (width as usize)
        .checked_mul(height as usize)
        .ok_or_else(|| BioFormatsError::Format("TextReader: plane size overflows".into()))?;
    let total_values = plane_values
        .checked_mul(channels as usize)
        .ok_or_else(|| BioFormatsError::Format("TextReader: image size overflows".into()))?;
    let mut values = vec![f32::NAN; total_values];
    for row in parsed_rows {
        let x = row[x_index] as usize;
        let y = row[y_index] as usize;
        let pixel = y * width as usize + x;
        for (c, &column) in channel_columns.iter().enumerate() {
            values[c * plane_values + pixel] = row[column] as f32;
        }
    }

    let mut metadata = HashMap::new();
    metadata.insert(
        "TextReader table header rows".into(),
        MetadataValue::Int(header_index as i64 + 1),
    );
    metadata.insert(
        "TextReader x column".into(),
        MetadataValue::String(header[x_index].clone()),
    );
    metadata.insert(
        "TextReader y column".into(),
        MetadataValue::String(header[y_index].clone()),
    );
    for (c, &column) in channel_columns.iter().enumerate() {
        metadata.insert(
            format!("TextReader channel {c}"),
            MetadataValue::String(header[column].clone()),
        );
    }

    Ok(Some((
        width,
        height,
        channels,
        floats_to_big_endian_bytes(&values)?,
        metadata,
    )))
}

fn find_text_table_header(lines: &[Vec<String>]) -> Option<(usize, usize)> {
    for data_index in 1..lines.len() {
        let header_index = data_index - 1;
        let header = &lines[header_index];
        let data = &lines[data_index];
        if data.len() < 3 || header.len() != data.len() || parse_text_numeric_row(data).is_none() {
            continue;
        }
        return Some((header_index, data_index));
    }
    None
}

fn parse_text_numeric_row(tokens: &[String]) -> Option<Vec<f64>> {
    tokens
        .iter()
        .map(|token| parse_java_double(token))
        .collect()
}

fn parse_java_double(token: &str) -> Option<f64> {
    let mut token = token.trim();
    if matches!(token.as_bytes().last(), Some(b'f' | b'F' | b'd' | b'D')) {
        token = &token[..token.len() - 1];
    }
    match token {
        "Infinity" | "+Infinity" => Some(f64::INFINITY),
        "-Infinity" => Some(f64::NEG_INFINITY),
        "NaN" | "+NaN" | "-NaN" => Some(f64::NAN),
        _ => token
            .parse::<f64>()
            .ok()
            .or_else(|| parse_java_hex_double(token)),
    }
}

fn parse_java_hex_double(token: &str) -> Option<f64> {
    let (negative, body) = token
        .strip_prefix('-')
        .map(|s| (true, s))
        .or_else(|| token.strip_prefix('+').map(|s| (false, s)))
        .unwrap_or((false, token));
    let body = body
        .strip_prefix("0x")
        .or_else(|| body.strip_prefix("0X"))?;
    let (mantissa, exponent) = body.split_once(['p', 'P'])?;
    let exponent = exponent.parse::<i32>().ok()?;
    let (whole, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if whole.is_empty() && frac.is_empty() {
        return None;
    }
    let mut value = 0.0f64;
    let mut seen_digit = false;
    for ch in whole.chars() {
        value = value * 16.0 + ch.to_digit(16)? as f64;
        seen_digit = true;
    }
    let mut scale = 1.0 / 16.0;
    for ch in frac.chars() {
        value += ch.to_digit(16)? as f64 * scale;
        scale /= 16.0;
        seen_digit = true;
    }
    if !seen_digit {
        return None;
    }
    value *= 2.0f64.powi(exponent);
    Some(if negative { -value } else { value })
}

fn parse_text_dense_grid(
    lines: &[Vec<String>],
) -> Result<(u32, u32, u32, Vec<u8>, HashMap<String, MetadataValue>)> {
    let mut rows: Vec<Vec<f32>> = Vec::new();
    for tokens in lines {
        let mut row = Vec::with_capacity(tokens.len());
        for cell in tokens {
            let value = parse_java_double(cell).ok_or_else(|| {
                BioFormatsError::UnsupportedFormat(format!("TextReader: non-numeric cell {cell:?}"))
            })?;
            row.push(value as f32);
        }
        if !row.is_empty() {
            rows.push(row);
        }
    }
    if rows.is_empty() {
        return Err(BioFormatsError::UnsupportedFormat(
            "TextReader: file contains no numeric data".to_string(),
        ));
    }
    let width = rows[0].len();
    if rows.iter().any(|row| row.len() != width) {
        return Err(BioFormatsError::UnsupportedFormat(
            "TextReader: rows have inconsistent column counts".to_string(),
        ));
    }
    let width = u32::try_from(width)
        .map_err(|_| BioFormatsError::Format("TextReader: width overflows".into()))?;
    let height = u32::try_from(rows.len())
        .map_err(|_| BioFormatsError::Format("TextReader: height overflows".into()))?;
    let values: Vec<f32> = rows.iter().flatten().copied().collect();
    Ok((
        width,
        height,
        1,
        floats_to_big_endian_bytes(&values)?,
        HashMap::new(),
    ))
}

fn floats_to_big_endian_bytes(values: &[f32]) -> Result<Vec<u8>> {
    let mut pixel_data = Vec::with_capacity(
        values
            .len()
            .checked_mul(4)
            .ok_or_else(|| BioFormatsError::Format("TextReader: byte count overflows".into()))?,
    );
    for &value in values {
        pixel_data.extend_from_slice(&value.to_be_bytes());
    }
    Ok(pixel_data)
}

#[cfg(test)]
mod sm_camera_reader_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_smc_path() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("bioformats_smc_{nanos}_{n}.smc"))
    }

    fn write_smc(path: &Path, size_x: i16, size_y: i16, pixels: &[u8]) {
        let mut bytes = vec![0u8; SMC_HEADER_SIZE];
        bytes[..SMC_MAGIC.len()].copy_from_slice(&SMC_MAGIC);
        bytes[524..526].copy_from_slice(&size_y.to_be_bytes());
        bytes[532..534].copy_from_slice(&size_x.to_be_bytes());
        bytes.extend_from_slice(pixels);
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn sm_camera_detection_uses_magic_not_suffix() {
        let reader = SmCameraReader::new();

        assert!(!reader.is_this_type_by_name(Path::new("image.smc")));
        assert!(reader.is_this_type_by_bytes(&SMC_MAGIC));
        assert!(!reader.is_this_type_by_bytes(b"not an sm camera file"));
    }

    #[test]
    fn sm_camera_reads_java_header_offsets_and_metadata() {
        let path = temp_smc_path();
        write_smc(&path, 3, 2, &[1, 2, 3, 4, 5, 6]);

        let mut reader = SmCameraReader::new();
        reader.set_id(&path).unwrap();
        let meta = reader.metadata();
        assert_eq!(meta.size_x, 3);
        assert_eq!(meta.size_y, 2);
        assert_eq!(meta.pixel_type, PixelType::Uint8);
        assert!(!meta.is_little_endian);
        assert!(matches!(
            meta.series_metadata.get("Image width"),
            Some(MetadataValue::Int(3))
        ));
        assert!(matches!(
            meta.series_metadata.get("Image height"),
            Some(MetadataValue::Int(2))
        ));
        assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4, 5, 6]);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn sm_camera_rejects_negative_java_short_dimensions() {
        let path = temp_smc_path();
        write_smc(&path, -1, 2, &[0; 8]);

        let mut reader = SmCameraReader::new();
        assert!(reader.set_id(&path).is_err());

        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod text_reader_tests {
    use super::*;

    fn f32_be_at(bytes: &[u8], index: usize) -> f32 {
        let start = index * 4;
        f32::from_be_bytes(bytes[start..start + 4].try_into().unwrap())
    }

    #[test]
    fn text_reader_coordinate_table_uses_columns_as_sparse_channels() {
        let text = "ignored preamble\nx,y,ch1,ch2\n0,0,1,10\n2,1,2,20\n";
        let (width, height, channels, bytes, metadata) = parse_text_pixels(text).unwrap();

        assert_eq!((width, height, channels), (3, 2, 2));
        assert!(!bytes.is_empty());
        assert_eq!(f32_be_at(&bytes, 0), 1.0);
        assert!(f32_be_at(&bytes, 1).is_nan());
        assert_eq!(f32_be_at(&bytes, 5), 2.0);
        assert_eq!(f32_be_at(&bytes, 6), 10.0);
        assert!(f32_be_at(&bytes, 7).is_nan());
        assert_eq!(f32_be_at(&bytes, 11), 20.0);
        assert!(matches!(
            metadata.get("TextReader channel 1"),
            Some(MetadataValue::String(value)) if value == "ch2"
        ));
    }

    #[test]
    fn text_reader_dense_fallback_uses_big_endian_float_bytes() {
        let (width, height, channels, bytes, _) = parse_text_pixels("1,2\n3,4\n").unwrap();

        assert_eq!((width, height, channels), (2, 2, 1));
        assert_eq!(&bytes[0..4], &1.0f32.to_be_bytes());
        assert_eq!(&bytes[12..16], &4.0f32.to_be_bytes());
    }

    #[test]
    fn text_reader_accepts_java_double_tokens() {
        let (width, height, channels, bytes, _) =
            parse_text_pixels("x,y,value\n0,0,Infinity\n1,0,0x1.8p1\n").unwrap();

        assert_eq!((width, height, channels), (2, 1, 1));
        assert!(f32_be_at(&bytes, 0).is_infinite());
        assert_eq!(f32_be_at(&bytes, 1), 3.0);
        assert_eq!(parse_java_double("-0x1p2D"), Some(-4.0));
    }

    #[test]
    fn text_reader_byte_detection_requires_java_style_coordinate_table() {
        let reader = TextReader::new();

        assert!(reader.is_this_type_by_bytes(b"preamble\nx,y,value\n0,0,1\n"));
        assert!(!reader.is_this_type_by_bytes(b"1,2\n3,4\n"));
        assert!(!reader.is_this_type_by_bytes(b"a,b,value\n0,0,1\n"));
    }

    #[test]
    fn text_reader_first_java_table_without_xy_is_terminal() {
        let err = parse_text_pixels("a,b,value\n0,0,1\nx,y,value\n0,0,9\n").unwrap_err();

        assert!(matches!(
            err,
            BioFormatsError::UnsupportedFormat(message)
                if message.contains("no X/Y coordinate columns")
        ));
    }
}

#[cfg(test)]
mod openlab_user_var_tests {
    use super::*;

    /// Encode a single `CStringVariable` entry in the big-endian layout
    /// `OpenlabReader::read_variable` expects.
    fn encode_string_var(class: &str, value: &str, name: &str, base_class_version: u8) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(class.as_bytes());
        b.push(0); // className NUL terminator
        b.push(1); // derivedClassVersion
        b.extend_from_slice(&(value.len() as i32).to_be_bytes());
        b.extend_from_slice(value.as_bytes());
        b.push(0); // skipBytes(1)
        b.push(base_class_version);
        b.extend_from_slice(&(name.len() as i32).to_be_bytes());
        b.extend_from_slice(name.as_bytes());
        // skipBytes(baseClassVersion * 2 + 1)
        b.extend(std::iter::repeat(0u8).take(base_class_version as usize * 2 + 1));
        b
    }

    /// Encode a single `CFloatVariable` entry.
    fn encode_float_var(value: f64, name: &str, base_class_version: u8) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(b"CFloatVariable");
        b.push(0);
        b.push(1); // derivedClassVersion
        b.extend_from_slice(&value.to_be_bytes());
        b.push(base_class_version);
        b.extend_from_slice(&(name.len() as i32).to_be_bytes());
        b.extend_from_slice(name.as_bytes());
        b.extend(std::iter::repeat(0u8).take(base_class_version as usize * 2 + 1));
        b
    }

    #[test]
    fn read_variable_captures_stage_and_detector_fields() {
        // X-Y stage X position (string) -> xPos + synthesized position key.
        let bytes = encode_string_var("CStringVariable", "123.5", "X-Y Stage: X Position", 1);
        let mut c = Cursor::new(&bytes, false);
        let mut vars = OpenlabUserVars::default();
        OpenlabReader::read_variable(&mut c, &mut vars).unwrap();
        assert_eq!(vars.x_pos.as_deref(), Some("123.5"));
        // The raw variable plus the synthesized "position #1" key are both emitted.
        assert!(vars
            .metas
            .iter()
            .any(|(n, v)| n == "X-Y Stage: X Position" && v == "123.5"));
        assert!(vars
            .metas
            .iter()
            .any(|(n, v)| n == "X position for position #1" && v == "123.5"));

        // Gain (float) -> detector gain.
        let bytes = encode_float_var(2.0, "Gain", 1);
        let mut c = Cursor::new(&bytes, false);
        let mut vars = OpenlabUserVars::default();
        OpenlabReader::read_variable(&mut c, &mut vars).unwrap();
        assert_eq!(vars.gain.as_deref(), Some("2"));
        assert!(vars.metas.iter().any(|(n, _)| n == "Gain"));

        // ZPosition (string) -> zPos + synthesized key.
        let bytes = encode_string_var("CStringVariable", "7", "ZPosition", 2);
        let mut c = Cursor::new(&bytes, false);
        let mut vars = OpenlabUserVars::default();
        OpenlabReader::read_variable(&mut c, &mut vars).unwrap();
        assert_eq!(vars.z_pos.as_deref(), Some("7"));
        assert!(vars
            .metas
            .iter()
            .any(|(n, v)| n == "Z position for position #1" && v == "7"));
    }

    #[test]
    fn read_variable_rejects_invalid_revision() {
        // derivedClassVersion != 1 must error.
        let mut b = Vec::new();
        b.extend_from_slice(b"CStringVariable");
        b.push(0);
        b.push(2); // invalid derivedClassVersion
        let mut c = Cursor::new(&b, false);
        let mut vars = OpenlabUserVars::default();
        assert!(OpenlabReader::read_variable(&mut c, &mut vars).is_err());
    }
}

#[cfg(test)]
mod qt_writer_tests {
    use super::*;
    use crate::common::writer::FormatWriter;

    fn rgb_meta(width: u32, height: u32, planes: u32) -> ImageMetadata {
        ImageMetadata {
            size_x: width,
            size_y: height,
            size_z: 1,
            size_c: 3,
            size_t: planes,
            pixel_type: PixelType::Uint8,
            bits_per_pixel: 8,
            image_count: planes,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: true,
            is_interleaved: true,
            is_little_endian: false,
            resolution_count: 1,
            ..ImageMetadata::default()
        }
    }

    /// Write a small uncompressed RGB `.mov` with `QtWriter`, then re-open it
    /// with `QtReader` and assert dimensions + pixels round-trip.
    #[test]
    fn round_trip_uncompressed_rgb() {
        let (w, h, n) = (6u32, 4u32, 3u32);
        let meta = rgb_meta(w, h, n);

        // Distinct interleaved RGB pixel pattern per plane.
        let plane_len = (w * h * 3) as usize;
        let planes: Vec<Vec<u8>> = (0..n)
            .map(|p| {
                (0..plane_len)
                    .map(|i| ((i as u32 + p * 17) % 251) as u8)
                    .collect()
            })
            .collect();

        let path =
            std::env::temp_dir().join(format!("bioformats_qtwriter_rt_{}.mov", std::process::id()));

        let mut writer = QtWriter::new();
        writer.set_metadata(&meta).unwrap();
        writer.set_id(&path).unwrap();
        for (i, plane) in planes.iter().enumerate() {
            writer.save_bytes(i as u32, plane).unwrap();
        }
        writer.close().unwrap();

        let mut reader = QtReader::new();
        reader.set_id(&path).unwrap();
        let rm = reader.metadata();
        assert_eq!(rm.size_x, w);
        assert_eq!(rm.size_y, h);
        assert_eq!(rm.image_count, n);
        assert_eq!(rm.size_c, 3);
        assert!(rm.is_rgb);
        assert_eq!(rm.pixel_type, PixelType::Uint8);

        for (i, expected) in planes.iter().enumerate() {
            let got = reader.open_bytes(i as u32).unwrap();
            assert_eq!(&got, expected, "plane {i} pixels must round-trip");
        }

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn reader_byte_detection_matches_java_quicktime_markers() {
        let reader = QtReader::new();
        let mut header = [0u8; 64];
        header[12..16].copy_from_slice(b"wide");
        assert!(reader.is_this_type_by_bytes(&header));

        let mut header = [0u8; 64];
        header[20..26].copy_from_slice(b"ftypqt");
        assert!(reader.is_this_type_by_bytes(&header));

        let mut header = [0u8; 64];
        header[12..16].copy_from_slice(b"imag");
        assert!(!reader.is_this_type_by_bytes(&header));
    }

    /// The lossy/encoded codecs are encoder-blocked; non-UINT8 input is rejected
    /// rather than faked.
    #[test]
    fn rejects_non_uint8() {
        let mut meta = rgb_meta(4, 4, 1);
        meta.pixel_type = PixelType::Uint16;
        meta.bits_per_pixel = 16;
        let mut writer = QtWriter::new();
        assert!(writer.set_metadata(&meta).is_err());
    }

    /// Grayscale planes are written faithfully (inverted, row-padded to a
    /// multiple of 4). The bundled reader maps `"raw "` to RGB, so this only
    /// checks the writer produces a parseable file with the expected geometry.
    #[test]
    fn writes_grayscale_atoms() {
        // width 6 is not a multiple of 4 -> pad = 2 per row.
        let (w, h) = (6u32, 4u32);
        let meta = ImageMetadata {
            size_x: w,
            size_y: h,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type: PixelType::Uint8,
            bits_per_pixel: 8,
            image_count: 1,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: false,
            is_interleaved: false,
            is_little_endian: false,
            resolution_count: 1,
            ..ImageMetadata::default()
        };
        let plane: Vec<u8> = (0..(w * h) as usize).map(|i| i as u8).collect();
        let path = std::env::temp_dir().join(format!(
            "bioformats_qtwriter_gray_{}.mov",
            std::process::id()
        ));
        let mut writer = QtWriter::new();
        writer.set_metadata(&meta).unwrap();
        writer.set_id(&path).unwrap();
        writer.save_bytes(0, &plane).unwrap();
        writer.close().unwrap();

        let bytes = std::fs::read(&path).unwrap();
        // wide + mdat container then moov.
        assert_eq!(&bytes[4..8], b"wide");
        assert_eq!(&bytes[12..16], b"mdat");
        // pad = 2, stored plane = (6+2)*4 = 32 bytes; mdat length = 32 + 8.
        let mdat_len = u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        assert_eq!(mdat_len, 32 + 8);
        // First grayscale pixel inverted: 255 - 0 = 255.
        assert_eq!(bytes[16], 255);
        // moov atom follows the 32-byte pixel payload (16 header + 32 = 48).
        assert_eq!(&bytes[48 + 4..48 + 8], b"moov");

        let _ = std::fs::remove_file(&path);
    }
}
