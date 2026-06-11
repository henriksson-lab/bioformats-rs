//! Additional FLIM, flow cytometry, and miscellaneous imaging format readers.
//!
//! Includes FlowSightReader with binary header inspection plus explicit
//! unsupported detectors and bounded native readers.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufReader, ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::common::error::{BioFormatsError, Result};
use crate::common::io::read_bytes_at;
use crate::common::metadata::{ImageMetadata, MetadataValue, ModuloAnnotation};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;
use crate::tiff::ifd::{tag, Compression, Ifd, IfdValue};
use crate::tiff::parser::TiffParser;

// ---------------------------------------------------------------------------
// Macros
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
                Err(BioFormatsError::UnsupportedFormat(
                    concat!(stringify!($name), " native payload decoding is unsupported").to_string()
                ))
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
                Err(BioFormatsError::UnsupportedFormat(
                    concat!(stringify!($name), " native payload decoding is unsupported").to_string()
                ))
            }

            fn open_bytes_region(&mut self, _plane_index: u32, _x: u32, _y: u32, _w: u32, _h: u32) -> Result<Vec<u8>> {
                Err(BioFormatsError::UnsupportedFormat(
                    concat!(stringify!($name), " native payload decoding is unsupported").to_string()
                ))
            }

            fn open_thumb_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
                Err(BioFormatsError::UnsupportedFormat(
                    concat!(stringify!($name), " native payload decoding is unsupported").to_string()
                ))
            }
        }
    };
}

#[allow(unused_macros)]
macro_rules! placeholder_reader_u16_small {
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
                Err(BioFormatsError::UnsupportedFormat(
                    concat!(stringify!($name), " native payload decoding is unsupported").to_string()
                ))
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
                Err(BioFormatsError::UnsupportedFormat(
                    concat!(stringify!($name), " native payload decoding is unsupported").to_string()
                ))
            }

            fn open_bytes_region(&mut self, _plane_index: u32, _x: u32, _y: u32, _w: u32, _h: u32) -> Result<Vec<u8>> {
                Err(BioFormatsError::UnsupportedFormat(
                    concat!(stringify!($name), " native payload decoding is unsupported").to_string()
                ))
            }

            fn open_thumb_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
                Err(BioFormatsError::UnsupportedFormat(
                    concat!(stringify!($name), " native payload decoding is unsupported").to_string()
                ))
            }
        }
    };
}

// ---------------------------------------------------------------------------
// 1. Amnis FlowSight (.cif)
// ---------------------------------------------------------------------------
/// Amnis FlowSight CIF format (`.cif`).
pub struct FlowSightReader {
    path: Option<PathBuf>,
    ifds: Vec<Ifd>,
    metas: Vec<ImageMetadata>,
    current_series: usize,
    little_endian: bool,
}

impl FlowSightReader {
    pub fn new() -> Self {
        FlowSightReader {
            path: None,
            ifds: Vec::new(),
            metas: Vec::new(),
            current_series: 0,
            little_endian: true,
        }
    }

    fn decode_series_plane(&mut self, series: usize, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self
            .metas
            .get(series)
            .ok_or(BioFormatsError::NotInitialized)?
            .clone();
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let ifd = self
            .ifds
            .get(series + 1)
            .ok_or_else(|| BioFormatsError::Format("FlowSight image IFD is missing".into()))?
            .clone();
        let full = self.decode_ifd(&ifd)?;
        let bytes_per_sample = (meta.bits_per_pixel as usize + 7) / 8;
        let total_width = ifd.image_width().ok_or_else(|| {
            BioFormatsError::Format("FlowSight image IFD missing ImageWidth".into())
        })? as usize;
        let image_height = meta.size_y as usize;
        let channel_width = meta.size_x as usize;
        let channel_offset = plane_index as usize * channel_width;
        crop_flowsight_plane(
            &full,
            total_width,
            image_height,
            bytes_per_sample,
            channel_offset,
            0,
            channel_width,
            image_height,
        )
    }

    fn decode_ifd(&mut self, ifd: &Ifd) -> Result<Vec<u8>> {
        let width = ifd.image_width().ok_or_else(|| {
            BioFormatsError::Format("FlowSight image IFD missing ImageWidth".into())
        })? as usize;
        let height = ifd.image_length().ok_or_else(|| {
            BioFormatsError::Format("FlowSight image IFD missing ImageLength".into())
        })? as usize;
        let strips = self.read_strip_data(ifd)?;
        let strip_refs: Vec<&[u8]> = strips.iter().map(Vec::as_slice).collect();
        match ifd.get_u16(tag::COMPRESSION).unwrap_or(1) {
            FLOWSIGHT_GREYSCALE_COMPRESSION => {
                decode_flowsight_greyscale_strips(&strip_refs, width, height, self.little_endian)
            }
            FLOWSIGHT_BITMASK_COMPRESSION => {
                decode_flowsight_bitmask_strips(&strip_refs, width, height)
            }
            compression => Err(BioFormatsError::UnsupportedFormat(format!(
                "Unknown FlowSight CIF compression code: {compression}"
            ))),
        }
    }

    fn read_strip_data(&mut self, ifd: &Ifd) -> Result<Vec<Vec<u8>>> {
        let offsets = ifd.get_vec_u64(tag::STRIP_OFFSETS);
        let byte_counts = ifd.get_vec_u64(tag::STRIP_BYTE_COUNTS);
        if offsets.is_empty() || byte_counts.is_empty() || offsets.len() != byte_counts.len() {
            return Err(BioFormatsError::Format(
                "FlowSight image IFD has invalid strip offsets/counts".into(),
            ));
        }
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut reader = BufReader::new(File::open(path).map_err(BioFormatsError::Io)?);
        offsets
            .iter()
            .zip(byte_counts.iter())
            .map(|(&offset, &byte_count)| {
                let len = usize::try_from(byte_count).map_err(|_| {
                    BioFormatsError::Format("FlowSight strip byte count is too large".into())
                })?;
                read_bytes_at(&mut reader, offset, len)
            })
            .collect()
    }
}

impl Default for FlowSightReader {
    fn default() -> Self {
        Self::new()
    }
}

const FLOWSIGHT_CHANNEL_COUNT_TAG: u16 = 33000;
const FLOWSIGHT_CHANNEL_NAMES_TAG: u16 = 33007;
const FLOWSIGHT_CHANNEL_DESCS_TAG: u16 = 33008;
const FLOWSIGHT_METADATA_XML_TAG: u16 = 33027;
const FLOWSIGHT_GREYSCALE_COMPRESSION: u16 = 30817;
const FLOWSIGHT_BITMASK_COMPRESSION: u16 = 30818;

fn flowsight_channel_count(ifd0: &Ifd) -> usize {
    // Match Java FlowSightReader (lines 150-200): start with the CHANNEL_COUNT_TAG
    // default, override with the channel-names count if present, then override
    // AGAIN with the XML ChannelInUseIndicators count if the XML provides it.
    // The XML count is applied LAST so it wins when sources disagree.
    let mut channel_count = ifd0
        .get_u32(FLOWSIGHT_CHANNEL_COUNT_TAG)
        .unwrap_or(1)
        .max(1) as usize;
    if let Some(names) = ifd0.get_str(FLOWSIGHT_CHANNEL_NAMES_TAG) {
        let count = split_flowsight_pipe_list(names).len();
        if count > 0 {
            channel_count = count;
        }
    }
    if let Some(xml) = ifd0.get_str(FLOWSIGHT_METADATA_XML_TAG) {
        if let Some(count) = count_flowsight_channels_in_use(xml) {
            channel_count = count.max(1);
        }
    }
    channel_count
}

fn split_flowsight_pipe_list(value: &str) -> Vec<String> {
    value
        .split('|')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

fn count_flowsight_channels_in_use(xml: &str) -> Option<usize> {
    let marker = "ChannelInUseIndicators";
    let start = xml.find(marker)?;
    let after_start = xml[start..].find('>')? + start + 1;
    let end = xml[after_start..].find('<')? + after_start;
    Some(
        xml[after_start..end]
            .split_whitespace()
            .filter(|token| *token == "1")
            .count(),
    )
}

fn build_flowsight_metadata(ifd: &Ifd, ifd0: &Ifd, channel_count: usize) -> Result<ImageMetadata> {
    let total_width = ifd
        .image_width()
        .ok_or_else(|| BioFormatsError::Format("FlowSight image IFD missing ImageWidth".into()))?;
    let size_y = ifd
        .image_length()
        .ok_or_else(|| BioFormatsError::Format("FlowSight image IFD missing ImageLength".into()))?;
    let bits = ifd.bits_per_sample().first().copied().unwrap_or(8);
    if bits != 8 && bits != 16 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "FlowSight CIF only supports 8-bit masks and 16-bit greyscale pixels, got {bits}"
        )));
    }
    if channel_count == 0 || total_width % channel_count as u32 != 0 {
        return Err(BioFormatsError::Format(format!(
            "FlowSight image width {total_width} is not divisible by channel count {channel_count}"
        )));
    }

    let mut meta = ImageMetadata {
        size_x: total_width / channel_count as u32,
        size_y,
        size_c: channel_count as u32,
        image_count: channel_count as u32,
        pixel_type: if bits == 8 {
            PixelType::Uint8
        } else {
            PixelType::Uint16
        },
        bits_per_pixel: bits as u8,
        is_little_endian: true,
        ..ImageMetadata::default()
    };
    meta.series_metadata.insert(
        "FlowSight.TotalWidth".into(),
        crate::common::metadata::MetadataValue::Int(total_width as i64),
    );
    if let Some(xml) = ifd0.get_str(FLOWSIGHT_METADATA_XML_TAG) {
        meta.series_metadata.insert(
            "FlowSight.MetadataXML".into(),
            crate::common::metadata::MetadataValue::String(xml.to_owned()),
        );
    }
    if let Some(names) = ifd0.get_str(FLOWSIGHT_CHANNEL_NAMES_TAG) {
        meta.series_metadata.insert(
            "FlowSight.ChannelNames".into(),
            crate::common::metadata::MetadataValue::String(names.to_owned()),
        );
    }
    if let Some(descs) = ifd0.get_str(FLOWSIGHT_CHANNEL_DESCS_TAG) {
        meta.series_metadata.insert(
            "FlowSight.ChannelDescriptions".into(),
            crate::common::metadata::MetadataValue::String(descs.to_owned()),
        );
    }
    Ok(meta)
}

fn crop_flowsight_plane(
    full: &[u8],
    full_width: usize,
    full_height: usize,
    bytes_per_sample: usize,
    x: usize,
    y: usize,
    w: usize,
    h: usize,
) -> Result<Vec<u8>> {
    if x.checked_add(w).is_none_or(|end| end > full_width)
        || y.checked_add(h).is_none_or(|end| end > full_height)
    {
        return Err(BioFormatsError::Format(
            "Requested FlowSight tile dimensions extend beyond the image".into(),
        ));
    }
    let row_bytes = full_width
        .checked_mul(bytes_per_sample)
        .ok_or_else(|| BioFormatsError::Format("FlowSight row byte count overflows".into()))?;
    let expected = row_bytes
        .checked_mul(full_height)
        .ok_or_else(|| BioFormatsError::Format("FlowSight plane byte count overflows".into()))?;
    if full.len() < expected {
        return Err(BioFormatsError::InvalidData(
            "FlowSight decoded plane is shorter than expected".into(),
        ));
    }
    let out_row_bytes = w.checked_mul(bytes_per_sample).ok_or_else(|| {
        BioFormatsError::Format("FlowSight output row byte count overflows".into())
    })?;
    let mut out = Vec::with_capacity(out_row_bytes * h);
    let x_bytes = x * bytes_per_sample;
    for row in y..y + h {
        let start = row * row_bytes + x_bytes;
        out.extend_from_slice(&full[start..start + out_row_bytes]);
    }
    Ok(out)
}

#[allow(dead_code)]
fn decode_flowsight_bitmask_strips(
    strips: &[&[u8]],
    image_width: usize,
    image_height: usize,
) -> Result<Vec<u8>> {
    let expected = image_width
        .checked_mul(image_height)
        .ok_or_else(|| BioFormatsError::InvalidData("FlowSight bitmask size overflows".into()))?;
    let mut out = vec![0u8; expected];
    let mut offset = 0usize;

    for strip in strips {
        let mut chunks = strip.chunks_exact(2);
        for pair in &mut chunks {
            let value = pair[0];
            let run_length = pair[1] as usize + 1;
            let end = offset.checked_add(run_length).ok_or_else(|| {
                BioFormatsError::InvalidData("FlowSight bitmask run overflows".into())
            })?;
            if end > out.len() {
                return Err(BioFormatsError::InvalidData(
                    "FlowSight bitmask run exceeds image size".into(),
                ));
            }
            out[offset..end].fill(value);
            offset = end;
        }
        if !chunks.remainder().is_empty() {
            return Err(BioFormatsError::InvalidData(
                "FlowSight bitmask strip has an odd byte count".into(),
            ));
        }
    }

    if offset != out.len() {
        return Err(BioFormatsError::InvalidData(
            "FlowSight bitmask data ended before filling the image".into(),
        ));
    }
    Ok(out)
}

#[allow(dead_code)]
fn decode_flowsight_greyscale_strips(
    strips: &[&[u8]],
    image_width: usize,
    image_height: usize,
    little_endian: bool,
) -> Result<Vec<u8>> {
    let pixels = image_width.checked_mul(image_height).ok_or_else(|| {
        BioFormatsError::InvalidData("FlowSight greyscale pixel count overflows".into())
    })?;
    let mut out = vec![
        0u8;
        pixels.checked_mul(2).ok_or_else(|| {
            BioFormatsError::InvalidData("FlowSight greyscale byte count overflows".into())
        })?
    ];
    let mut nibbles = FlowSightNibbleReader::new(strips);
    let mut last_row = vec![0i16; image_width];
    let mut this_row = vec![0i16; image_width];
    let mut byte_index = 0usize;

    for _y in 0..image_height {
        for x in 0..image_width {
            let diff = nibbles.next_diff()?;
            let value = if x == 0 {
                diff.wrapping_add(last_row[x])
            } else {
                diff.wrapping_add(last_row[x])
                    .wrapping_add(this_row[x - 1])
                    .wrapping_sub(last_row[x - 1])
            };
            this_row[x] = value;
            let bytes = if little_endian {
                value.to_le_bytes()
            } else {
                value.to_be_bytes()
            };
            out[byte_index..byte_index + 2].copy_from_slice(&bytes);
            byte_index += 2;
        }
        std::mem::swap(&mut last_row, &mut this_row);
        this_row.fill(0);
    }

    Ok(out)
}

#[allow(dead_code)]
struct FlowSightNibbleReader<'a> {
    strips: &'a [&'a [u8]],
    strip_index: usize,
    byte_index: usize,
    current_byte: u8,
    nibble_index: u8,
}

#[allow(dead_code)]
impl<'a> FlowSightNibbleReader<'a> {
    fn new(strips: &'a [&'a [u8]]) -> Self {
        Self {
            strips,
            strip_index: 0,
            byte_index: 0,
            current_byte: 0,
            nibble_index: 2,
        }
    }

    fn next_diff(&mut self) -> Result<i16> {
        let mut shift = 0u32;
        let mut value = 0i16;

        loop {
            if shift > 15 {
                return Err(BioFormatsError::InvalidData(
                    "FlowSight greyscale variable-length value is unterminated".into(),
                ));
            }
            let nibble = self.next_nibble()? as i16;
            value = value.wrapping_add((nibble & 0x7).wrapping_shl(shift));
            shift += 3;
            if (nibble & 0x8) == 0 {
                if (nibble & 0x4) != 0 {
                    // Java FlowSightReader.java:409 evaluates `1 << shift` in 32-bit
                    // int space then ORs it into the short. Doing the shift/negation
                    // in i16 overflows (panics in debug) at shift==15 and yields wrong
                    // bits at shift==18; compute in i32 and truncate to match Java.
                    value |= (-(1i32 << shift)) as i16;
                }
                return Ok(value);
            }
        }
    }

    fn next_nibble(&mut self) -> Result<u8> {
        if self.nibble_index >= 2 {
            self.current_byte = self.next_byte()?;
            self.nibble_index = 0;
        }
        let nibble = if self.nibble_index == 0 {
            self.current_byte & 0x0f
        } else {
            self.current_byte >> 4
        };
        self.nibble_index += 1;
        Ok(nibble)
    }

    fn next_byte(&mut self) -> Result<u8> {
        while self.strip_index < self.strips.len()
            && self.byte_index >= self.strips[self.strip_index].len()
        {
            self.strip_index += 1;
            self.byte_index = 0;
        }
        if self.strip_index >= self.strips.len() {
            return Err(BioFormatsError::InvalidData(
                "FlowSight greyscale data ended before filling the image".into(),
            ));
        }
        let byte = self.strips[self.strip_index][self.byte_index];
        self.byte_index += 1;
        Ok(byte)
    }
}

impl FormatReader for FlowSightReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        matches!(
            path.extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_ascii_lowercase())
                .as_deref(),
            Some("cif")
        )
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.len() >= 4
            && ((header[0..2] == [0x49, 0x49] && header[2..4] == [42, 0])
                || (header[0..2] == [0x4d, 0x4d] && header[2..4] == [0, 42]))
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let file = File::open(path).map_err(BioFormatsError::Io)?;
        let mut parser = TiffParser::new(BufReader::new(file)).map_err(|err| {
            BioFormatsError::UnsupportedFormat(format!("FlowSight CIF is not TIFF-like: {err}"))
        })?;
        if !matches!(parser.variant, crate::tiff::parser::TiffVariant::Classic) {
            return Err(BioFormatsError::UnsupportedFormat(
                "FlowSight CIF requires classic TIFF-style 32-bit offsets".into(),
            ));
        }
        let little_endian = parser.little_endian;
        let ifds = parser.read_ifds()?;
        if ifds.len() < 2 {
            return Err(BioFormatsError::Format(
                "FlowSight CIF contains no image IFDs".into(),
            ));
        }
        let ifd0 = &ifds[0];
        if ifd0.get_str(FLOWSIGHT_METADATA_XML_TAG).is_none() {
            return Err(BioFormatsError::UnsupportedFormat(
                "FlowSight CIF metadata XML tag 33027 is missing".into(),
            ));
        }
        let channel_count = flowsight_channel_count(ifd0);
        let metas = ifds[1..]
            .iter()
            .map(|ifd| build_flowsight_metadata(ifd, ifd0, channel_count))
            .collect::<Result<Vec<_>>>()?;

        self.path = Some(path.to_path_buf());
        self.ifds = ifds;
        self.metas = metas;
        self.current_series = 0;
        self.little_endian = little_endian;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.ifds.clear();
        self.metas.clear();
        self.current_series = 0;
        self.little_endian = true;
        Ok(())
    }

    fn series_count(&self) -> usize {
        self.metas.len()
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if s >= self.metas.len() {
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
        &self.metas[self.current_series]
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        self.decode_series_plane(self.current_series, plane_index)
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
        let ifd = self
            .ifds
            .get(self.current_series + 1)
            .ok_or_else(|| BioFormatsError::Format("FlowSight image IFD is missing".into()))?
            .clone();
        let full = self.decode_ifd(&ifd)?;
        let bytes_per_sample = (meta.bits_per_pixel as usize + 7) / 8;
        let total_width = ifd.image_width().ok_or_else(|| {
            BioFormatsError::Format("FlowSight image IFD missing ImageWidth".into())
        })? as usize;
        let channel_x = plane_index as usize * meta.size_x as usize + x as usize;
        crop_flowsight_plane(
            &full,
            total_width,
            meta.size_y as usize,
            bytes_per_sample,
            channel_x,
            y as usize,
            w as usize,
            h as usize,
        )
    }

    fn open_thumb_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
        let meta = self
            .metas
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(_plane_index, tx, ty, tw, th)
    }
}

// ---------------------------------------------------------------------------
// 2. Amnis/Luminex IM3
// ---------------------------------------------------------------------------
const SYNTHETIC_IM3_MAGIC: &[u8] = b"BIOFORMATS-RS-SYNTHETIC-IM3-RAW-V1\0";
const SYNTHETIC_SLIDEBOOK7_MAGIC: &[u8] = b"BIOFORMATS-RS-SYNTHETIC-SLIDEBOOK7-RAW-V1\0";
const SYNTHETIC_IVISION_MAGIC: &[u8] = b"BIOFORMATS-RS-SYNTHETIC-IVISION-IPM-RAW-V1\0";
const SYNTHETIC_RAW_TRAILER_LEN: usize = 24;
const SYNTHETIC_RAW_U8: u16 = 1;
const SYNTHETIC_RAW_U16: u16 = 2;

#[derive(Clone, Copy)]
struct SyntheticRawSpec {
    format_name: &'static str,
    unsupported_message: &'static str,
    extension: &'static str,
    magic: &'static [u8],
}

#[derive(Clone, Copy)]
struct SyntheticRawLayout {
    payload_offset: u64,
    plane_len: usize,
}

struct SyntheticRawState {
    path: PathBuf,
    meta: ImageMetadata,
    layout: SyntheticRawLayout,
}

impl SyntheticRawSpec {
    fn matches_name(self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        ext.as_deref() == Some(self.extension)
    }

    fn matches_bytes(self, header: &[u8]) -> bool {
        header.starts_with(self.magic)
    }
}

fn im3_native_cookie(header: &[u8]) -> bool {
    header
        .get(..4)
        .is_some_and(|bytes| u32::from_be_bytes(bytes.try_into().unwrap()) == 1985)
}

fn ivision_native_header(header: &[u8]) -> bool {
    ivision_structural_header(header) && header[5] <= 8
}

fn ivision_structural_header(header: &[u8]) -> bool {
    if header.len() < 6 {
        return false;
    }
    let Ok(version) = std::str::from_utf8(&header[..3]) else {
        return false;
    };
    version.parse::<f64>().is_ok()
        && version.contains('.')
        && !version.contains('-')
        && header[3].is_ascii_alphabetic()
}

fn synthetic_raw_unsupported(spec: SyntheticRawSpec) -> BioFormatsError {
    BioFormatsError::UnsupportedFormat(spec.unsupported_message.to_string())
}

fn synthetic_raw_pixel_type(spec: SyntheticRawSpec, code: u16) -> Result<(PixelType, u8)> {
    match code {
        SYNTHETIC_RAW_U8 => Ok((PixelType::Uint8, 8)),
        SYNTHETIC_RAW_U16 => Ok((PixelType::Uint16, 16)),
        other => Err(BioFormatsError::UnsupportedFormat(format!(
            "{} synthetic raw unsupported pixel type code {other}",
            spec.format_name
        ))),
    }
}

fn checked_nonzero_dimension(spec: SyntheticRawSpec, label: &str, value: u32) -> Result<u32> {
    if value == 0 {
        return Err(BioFormatsError::Format(format!(
            "{} synthetic raw {label} must be non-zero",
            spec.format_name
        )));
    }
    Ok(value)
}

fn checked_mul_usize(spec: SyntheticRawSpec, lhs: usize, rhs: usize, label: &str) -> Result<usize> {
    lhs.checked_mul(rhs).ok_or_else(|| {
        BioFormatsError::Format(format!(
            "{} synthetic raw {label} overflows",
            spec.format_name
        ))
    })
}

fn parse_synthetic_raw(path: &Path, spec: SyntheticRawSpec) -> Result<SyntheticRawState> {
    let mut file = File::open(path).map_err(BioFormatsError::Io)?;
    let mut magic = vec![0u8; spec.magic.len()];
    match file.read_exact(&mut magic) {
        Ok(()) if magic == spec.magic => {}
        Ok(()) => return Err(synthetic_raw_unsupported(spec)),
        Err(err) if err.kind() == ErrorKind::UnexpectedEof => {
            return Err(synthetic_raw_unsupported(spec));
        }
        Err(err) => return Err(BioFormatsError::Io(err)),
    }

    let mut trailer = [0u8; SYNTHETIC_RAW_TRAILER_LEN];
    match file.read_exact(&mut trailer) {
        Ok(()) => {}
        Err(err) if err.kind() == ErrorKind::UnexpectedEof => {
            return Err(BioFormatsError::Format(format!(
                "{} synthetic raw header is truncated",
                spec.format_name
            )));
        }
        Err(err) => return Err(BioFormatsError::Io(err)),
    }

    let read_u32 = |offset: usize| {
        u32::from_le_bytes([
            trailer[offset],
            trailer[offset + 1],
            trailer[offset + 2],
            trailer[offset + 3],
        ])
    };
    let size_x = checked_nonzero_dimension(spec, "width", read_u32(0))?;
    let size_y = checked_nonzero_dimension(spec, "height", read_u32(4))?;
    let size_z = checked_nonzero_dimension(spec, "Z size", read_u32(8))?;
    let size_c = checked_nonzero_dimension(spec, "channel count", read_u32(12))?;
    let size_t = checked_nonzero_dimension(spec, "timepoint count", read_u32(16))?;
    let pixel_code = u16::from_le_bytes([trailer[20], trailer[21]]);
    let reserved = u16::from_le_bytes([trailer[22], trailer[23]]);
    if reserved != 0 {
        return Err(BioFormatsError::Format(format!(
            "{} synthetic raw reserved header field must be zero",
            spec.format_name
        )));
    }
    let (pixel_type, bits_per_pixel) = synthetic_raw_pixel_type(spec, pixel_code)?;

    let image_count = size_z
        .checked_mul(size_c)
        .and_then(|v| v.checked_mul(size_t))
        .ok_or_else(|| {
            BioFormatsError::Format(format!(
                "{} synthetic raw image count overflows",
                spec.format_name
            ))
        })?;
    let samples = checked_mul_usize(spec, size_x as usize, size_y as usize, "plane sample count")?;
    let plane_len = checked_mul_usize(
        spec,
        samples,
        pixel_type.bytes_per_sample(),
        "plane byte count",
    )?;
    let expected_payload_len =
        checked_mul_usize(spec, plane_len, image_count as usize, "payload length")?;
    let payload_offset = (spec.magic.len() + SYNTHETIC_RAW_TRAILER_LEN) as u64;
    let expected_file_len = payload_offset
        .checked_add(expected_payload_len as u64)
        .ok_or_else(|| {
            BioFormatsError::Format(format!(
                "{} synthetic raw file length overflows",
                spec.format_name
            ))
        })?;
    let actual_file_len = file.metadata().map_err(BioFormatsError::Io)?.len();
    if actual_file_len != expected_file_len {
        return Err(BioFormatsError::InvalidData(format!(
            "{} synthetic raw payload length is {}, expected {expected_payload_len}",
            spec.format_name,
            actual_file_len.saturating_sub(payload_offset)
        )));
    }

    let meta = ImageMetadata {
        size_x,
        size_y,
        size_z,
        size_c,
        size_t,
        pixel_type,
        bits_per_pixel,
        image_count,
        is_little_endian: true,
        ..ImageMetadata::default()
    };
    Ok(SyntheticRawState {
        path: path.to_path_buf(),
        meta,
        layout: SyntheticRawLayout {
            payload_offset,
            plane_len,
        },
    })
}

fn synthetic_raw_open_bytes(
    state: &SyntheticRawState,
    spec: SyntheticRawSpec,
    p: u32,
) -> Result<Vec<u8>> {
    if p >= state.meta.image_count {
        return Err(BioFormatsError::PlaneOutOfRange(p));
    }
    let offset = state
        .layout
        .payload_offset
        .checked_add(
            (p as u64)
                .checked_mul(state.layout.plane_len as u64)
                .ok_or_else(|| {
                    BioFormatsError::Format(format!(
                        "{} synthetic raw plane offset overflows",
                        spec.format_name
                    ))
                })?,
        )
        .ok_or_else(|| {
            BioFormatsError::Format(format!(
                "{} synthetic raw plane offset overflows",
                spec.format_name
            ))
        })?;
    let mut reader = BufReader::new(File::open(&state.path).map_err(BioFormatsError::Io)?);
    read_bytes_at(&mut reader, offset, state.layout.plane_len)
}

fn synthetic_raw_open_bytes_region(
    state: &SyntheticRawState,
    spec: SyntheticRawSpec,
    p: u32,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
) -> Result<Vec<u8>> {
    let full = synthetic_raw_open_bytes(state, spec, p)?;
    crop_full_plane(spec.format_name, &full, &state.meta, 1, x, y, w, h)
}

#[derive(Clone)]
struct Im3Record {
    name: String,
    rec_type: u32,
    payload_offset: usize,
    payload_len: usize,
}

struct Im3NativeState {
    path: PathBuf,
    datasets: Vec<Im3NativeDataset>,
}

struct Im3NativeDataset {
    meta: ImageMetadata,
    data_offset: u64,
    interleaved_len: usize,
}

#[derive(Clone)]
struct Im3DatasetCandidate {
    container: Im3Record,
    shape: Im3Record,
    data: Im3Record,
}

enum Im3State {
    Synthetic(SyntheticRawState),
    Native(Im3NativeState),
}

fn im3_read_u32_le(bytes: &[u8], offset: usize, context: &str) -> Result<u32> {
    let raw = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| BioFormatsError::Format(format!("IM3 native {context} is truncated")))?;
    Ok(u32::from_le_bytes(raw.try_into().unwrap()))
}

fn im3_parse_record(bytes: &[u8], pos: &mut usize, end: usize) -> Result<Option<Im3Record>> {
    if *pos + 4 > end {
        return Ok(None);
    }
    let name_len = im3_read_u32_le(bytes, *pos, "record name length")? as usize;
    *pos += 4;
    let name_bytes = bytes
        .get(*pos..pos.saturating_add(name_len))
        .ok_or_else(|| BioFormatsError::Format("IM3 native record name is truncated".into()))?;
    let name = std::str::from_utf8(name_bytes)
        .map_err(|_| BioFormatsError::Format("IM3 native record name is not UTF-8".into()))?
        .to_string();
    *pos += name_len;
    let length_field = im3_read_u32_le(bytes, *pos, "record length")? as usize;
    *pos += 4;
    if length_field < 8 {
        return Err(BioFormatsError::Format(format!(
            "IM3 native record {name} has invalid length {length_field}"
        )));
    }
    let rec_type = im3_read_u32_le(bytes, *pos, "record type")?;
    *pos += 4;
    let payload_len = length_field - 8;
    let payload_offset = *pos;
    let next = payload_offset
        .checked_add(payload_len)
        .ok_or_else(|| BioFormatsError::Format("IM3 native record offset overflows".into()))?;
    if next > end || next > bytes.len() {
        return Err(BioFormatsError::Format(format!(
            "IM3 native record {name} payload is truncated"
        )));
    }
    *pos = next;
    Ok(Some(Im3Record {
        name,
        rec_type,
        payload_offset,
        payload_len,
    }))
}

fn im3_container_children(bytes: &[u8], rec: &Im3Record) -> Result<Vec<Im3Record>> {
    if rec.rec_type != 0 {
        return Ok(Vec::new());
    }
    if rec.payload_len < 8 {
        return Err(BioFormatsError::Format(format!(
            "IM3 native container {} is truncated",
            rec.name
        )));
    }
    let mut pos = rec.payload_offset + 8;
    let end = rec.payload_offset + rec.payload_len;
    let mut children = Vec::new();
    while pos < end.saturating_sub(8) {
        match im3_parse_record(bytes, &mut pos, end)? {
            Some(child) => children.push(child),
            None => break,
        }
    }
    Ok(children)
}

fn im3_int_entries(bytes: &[u8], rec: &Im3Record) -> Result<Vec<u32>> {
    if rec.rec_type != 6 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "IM3 native {} record is not an integer array",
            rec.name
        )));
    }
    if rec.payload_len < 8 {
        return Err(BioFormatsError::Format(format!(
            "IM3 native integer record {} is truncated",
            rec.name
        )));
    }
    let code = im3_read_u32_le(bytes, rec.payload_offset, "integer record code")?;
    if code == 0 {
        return Ok(vec![im3_read_u32_le(
            bytes,
            rec.payload_offset + 4,
            "integer scalar",
        )?]);
    }
    let count = im3_read_u32_le(bytes, rec.payload_offset + 4, "integer array count")? as usize;
    let values_offset = rec.payload_offset + 8;
    let byte_len = count.checked_mul(4).ok_or_else(|| {
        BioFormatsError::Format("IM3 native integer array length overflows".into())
    })?;
    if values_offset + byte_len > rec.payload_offset + rec.payload_len {
        return Err(BioFormatsError::Format(format!(
            "IM3 native integer record {} values are truncated",
            rec.name
        )));
    }
    (0..count)
        .map(|idx| im3_read_u32_le(bytes, values_offset + idx * 4, "integer array value"))
        .collect()
}

fn im3_metadata_key(name: &str) -> Option<String> {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if matches!(ch, ' ' | '_' | '-' | '.' | '/' | ':' | '#') && !out.ends_with('_') {
            out.push('_');
        }
    }
    let key = out.trim_matches('_');
    if key.is_empty() {
        None
    } else {
        Some(key.to_string())
    }
}

fn im3_string_entry(bytes: &[u8], rec: &Im3Record) -> Option<String> {
    let raw = match rec.rec_type {
        // Compatibility with earlier synthetic fixtures.
        2 if rec.payload_len > 0 && rec.payload_len <= 512 => {
            bytes.get(rec.payload_offset..rec.payload_offset + rec.payload_len)?
        }
        // Java IM3Reader StringIM3Record: skip 4 bytes, then parseString()
        // (u32 byte length followed by UTF-8 bytes).
        10 if rec.payload_len >= 8 && rec.payload_len <= 516 => {
            let len =
                im3_read_u32_le(bytes, rec.payload_offset + 4, "string length").ok()? as usize;
            if len == 0 || len > 512 || 8 + len > rec.payload_len {
                return None;
            }
            bytes.get(rec.payload_offset + 8..rec.payload_offset + 8 + len)?
        }
        _ => return None,
    };
    let end = raw.iter().position(|&byte| byte == 0).unwrap_or(raw.len());
    let text = std::str::from_utf8(&raw[..end]).ok()?.trim();
    if text.is_empty()
        || text
            .chars()
            .any(|ch| ch.is_control() && !matches!(ch, '\t' | '\n' | '\r'))
    {
        return None;
    }
    Some(text.to_string())
}

fn im3_float_entries(bytes: &[u8], rec: &Im3Record) -> Option<Vec<f64>> {
    match (rec.rec_type, rec.payload_len) {
        (3, 4) => {
            let raw = bytes.get(rec.payload_offset..rec.payload_offset + 4)?;
            let value = f32::from_le_bytes(raw.try_into().ok()?) as f64;
            value.is_finite().then_some(vec![value])
        }
        (4, 8) => {
            let raw = bytes.get(rec.payload_offset..rec.payload_offset + 8)?;
            let value = f64::from_le_bytes(raw.try_into().ok()?);
            value.is_finite().then_some(vec![value])
        }
        // Java IM3Reader FloatIM3Record: int32 code, int32 count, then f32s.
        // A zero code stores a single f32 at offset + 4.
        (7, len) if len >= 8 => {
            let code = im3_read_u32_le(bytes, rec.payload_offset, "float record code").ok()?;
            if code == 0 {
                let raw = bytes.get(rec.payload_offset + 4..rec.payload_offset + 8)?;
                let value = f32::from_le_bytes(raw.try_into().ok()?) as f64;
                return value.is_finite().then_some(vec![value]);
            }
            let count =
                im3_read_u32_le(bytes, rec.payload_offset + 4, "float array count").ok()? as usize;
            let byte_len = count.checked_mul(4)?;
            if 8 + byte_len > len || count > 4096 {
                return None;
            }
            let mut values = Vec::with_capacity(count);
            for index in 0..count {
                let start = rec.payload_offset + 8 + index * 4;
                let raw = bytes.get(start..start + 4)?;
                let value = f32::from_le_bytes(raw.try_into().ok()?) as f64;
                if !value.is_finite() {
                    return None;
                }
                values.push(value);
            }
            Some(values)
        }
        _ => None,
    }
}

fn im3_float_entry(bytes: &[u8], rec: &Im3Record) -> Option<f64> {
    let values = im3_float_entries(bytes, rec)?;
    (values.len() == 1).then_some(values[0])
}

fn im3_scalar_metadata_value(bytes: &[u8], rec: &Im3Record) -> Result<Option<MetadataValue>> {
    if rec.rec_type == 6 && rec.name != "Shape" {
        let values = im3_int_entries(bytes, rec)?;
        if values.len() == 1 {
            Ok(Some(MetadataValue::Int(values[0] as i64)))
        } else {
            Ok(Some(MetadataValue::String(
                values
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(","),
            )))
        }
    } else if let Some(value) = im3_string_entry(bytes, rec) {
        Ok(Some(MetadataValue::String(value)))
    } else if let Some(values) = im3_float_entries(bytes, rec) {
        if values.len() == 1 {
            Ok(Some(MetadataValue::Float(values[0])))
        } else {
            Ok(Some(MetadataValue::String(
                values
                    .iter()
                    .map(|value| value.to_string())
                    .collect::<Vec<_>>()
                    .join(","),
            )))
        }
    } else {
        Ok(None)
    }
}

fn im3_metadata_number(bytes: &[u8], rec: &Im3Record) -> Result<Option<f64>> {
    if rec.rec_type == 6 && rec.name != "Shape" {
        let values = im3_int_entries(bytes, rec)?;
        Ok((values.len() == 1).then_some(values[0] as f64))
    } else {
        Ok(im3_float_entry(bytes, rec))
    }
}

fn im3_channel_labels(value: &str, size_c: u32) -> Option<Vec<String>> {
    let labels = value
        .split([',', ';', '\t', '|'])
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    (labels.len() == size_c as usize).then_some(labels)
}

fn im3_insert_interpreted_metadata(
    bytes: &[u8],
    children: &[Im3Record],
    meta: &mut ImageMetadata,
) -> Result<()> {
    for child in children {
        let Some(key) = im3_metadata_key(&child.name) else {
            continue;
        };
        match key.as_str() {
            "channel_wavelengths" | "wavelengths" | "emission_wavelengths" => {
                if child.rec_type != 6 {
                    continue;
                }
                let values = im3_int_entries(bytes, child)?;
                if values.len() != meta.size_c as usize {
                    continue;
                }
                let labels = values
                    .iter()
                    .map(|value| format!("{value} nm"))
                    .collect::<Vec<_>>();
                for (index, value) in values.iter().enumerate() {
                    meta.series_metadata.insert(
                        format!("im3.channel.{index}.emission_wavelength"),
                        MetadataValue::Float(*value as f64),
                    );
                }
                if values.len() >= 2 {
                    let start = values[0] as f64;
                    let end = *values.last().unwrap() as f64;
                    let step = (end - start) / (values.len() - 1) as f64;
                    meta.modulo_c = Some(ModuloAnnotation {
                        parent_dimension: "C".into(),
                        modulo_type: "lambda".into(),
                        start,
                        step,
                        end,
                        unit: "nm".into(),
                        labels,
                    });
                }
            }
            "channel_names" | "channels" => {
                if let Some(value) = im3_string_entry(bytes, child) {
                    if let Some(labels) = im3_channel_labels(&value, meta.size_c) {
                        for (index, label) in labels.into_iter().enumerate() {
                            meta.series_metadata.insert(
                                format!("im3.channel.{index}.name"),
                                MetadataValue::String(label),
                            );
                        }
                    }
                }
            }
            "instrument_name" => {
                if let Some(value) = im3_string_entry(bytes, child) {
                    meta.series_metadata
                        .insert("im3.instrument.name".into(), MetadataValue::String(value));
                }
            }
            "exposure_time" | "exposure_seconds" | "laser_power" => {
                if let Some(value) = im3_metadata_number(bytes, child)? {
                    meta.series_metadata.insert(
                        format!("im3.acquisition.{key}"),
                        MetadataValue::Float(value),
                    );
                }
            }
            key if key.starts_with("camera_gain") => {
                if let Some(value) = im3_metadata_number(bytes, child)? {
                    meta.series_metadata.insert(
                        "im3.acquisition.camera_gain".into(),
                        MetadataValue::Float(value),
                    );
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn im3_insert_container_scalar_metadata(
    bytes: &[u8],
    container: &Im3Record,
    meta: &mut ImageMetadata,
) -> Result<()> {
    let children = im3_container_children(bytes, container)?;
    im3_insert_interpreted_metadata(bytes, &children, meta)?;
    let mut unsupported_records = Vec::new();
    for child in children {
        let Some(key) = im3_metadata_key(&child.name) else {
            continue;
        };
        let value = im3_scalar_metadata_value(bytes, &child)?;
        if let Some(value) = value {
            meta.series_metadata
                .insert(format!("im3.native.{key}"), value);
        } else if let Some(diagnostic) = im3_unsupported_metadata_record(bytes, &child)? {
            unsupported_records.push(diagnostic);
        }
    }
    if !unsupported_records.is_empty() {
        let total = unsupported_records.len();
        unsupported_records.truncate(16);
        let mut diagnostic = unsupported_records.join("; ");
        if total > unsupported_records.len() {
            diagnostic.push_str(&format!("; {} more", total - unsupported_records.len()));
        }
        meta.series_metadata.insert(
            "im3.native.unsupported_metadata_records".into(),
            MetadataValue::String(diagnostic),
        );
        meta.series_metadata.insert(
            "im3.native.unsupported_metadata_record_count".into(),
            MetadataValue::Int(total as i64),
        );
    }
    Ok(())
}

fn im3_unsupported_metadata_record(bytes: &[u8], rec: &Im3Record) -> Result<Option<String>> {
    if matches!(rec.name.as_str(), "Shape" | "Data") || rec.rec_type == 1 {
        return Ok(None);
    }
    if rec.rec_type == 0 {
        let children = im3_container_children(bytes, rec)?;
        let has_pixels = children
            .iter()
            .any(|child| matches!(child.name.as_str(), "Shape" | "Data"));
        if has_pixels {
            return Ok(None);
        }
        return Ok(Some(format!(
            "{}(type=0,len={},children={})",
            rec.name,
            rec.payload_len,
            children.len()
        )));
    }
    Ok(Some(format!(
        "{}(type={},len={})",
        rec.name, rec.rec_type, rec.payload_len
    )))
}

fn im3_collect_dataset_candidates(
    bytes: &[u8],
    rec: &Im3Record,
    out: &mut Vec<Im3DatasetCandidate>,
) -> Result<()> {
    let children = im3_container_children(bytes, rec)?;
    let shape = children
        .iter()
        .find(|child| child.name == "Shape" && child.rec_type == 6)
        .cloned();
    let data = children
        .iter()
        .find(|child| child.name == "Data" && child.rec_type == 1)
        .cloned();
    if let (Some(shape), Some(data)) = (shape, data) {
        out.push(Im3DatasetCandidate {
            container: rec.clone(),
            shape,
            data,
        });
    }
    for child in children.iter().filter(|child| child.rec_type == 0) {
        im3_collect_dataset_candidates(bytes, child, out)?;
    }
    Ok(())
}

fn parse_im3_native(path: &Path) -> Result<Im3NativeState> {
    let bytes = std::fs::read(path).map_err(BioFormatsError::Io)?;
    if !im3_native_cookie(&bytes) {
        return Err(synthetic_raw_unsupported(Im3Reader::spec()));
    }
    if bytes.len() < 4 {
        return Err(BioFormatsError::Format(
            "IM3 native header is truncated".into(),
        ));
    }

    let mut pos = 4;
    let mut top_records = Vec::new();
    while pos < bytes.len().saturating_sub(8) {
        match im3_parse_record(&bytes, &mut pos, bytes.len())? {
            Some(record) => top_records.push(record),
            None => {
                if pos > bytes.len().saturating_sub(16) {
                    break;
                }
                let chunk_end = pos.checked_add(16).ok_or_else(|| {
                    BioFormatsError::Format("IM3 native chunk offset overflows".into())
                })?;
                if chunk_end > bytes.len() {
                    break;
                }
                pos = chunk_end;
            }
        }
    }

    let mut datasets = Vec::new();
    for rec in top_records.iter().filter(|rec| rec.rec_type == 0) {
        im3_collect_dataset_candidates(&bytes, rec, &mut datasets)?;
    }
    if datasets.is_empty() {
        return Err(BioFormatsError::UnsupportedFormat(
            "IM3 native file contains no supported Shape/Data dataset".into(),
        ));
    }
    let dataset_count = datasets.len();
    let mut native_datasets = Vec::with_capacity(dataset_count);
    for (dataset_index, dataset) in datasets.iter().enumerate() {
        native_datasets.push(parse_im3_native_dataset(
            &bytes,
            dataset,
            dataset_count,
            dataset_index,
        )?);
    }

    Ok(Im3NativeState {
        path: path.to_path_buf(),
        datasets: native_datasets,
    })
}

fn parse_im3_native_dataset(
    bytes: &[u8],
    dataset: &Im3DatasetCandidate,
    dataset_count: usize,
    dataset_index: usize,
) -> Result<Im3NativeDataset> {
    let shape_rec = &dataset.shape;
    let data_rec = &dataset.data;
    let shape = im3_int_entries(bytes, shape_rec)?;
    if shape.len() < 3 {
        return Err(BioFormatsError::Format(
            "IM3 native Shape record must contain width, height, and channels".into(),
        ));
    }
    let size_x = shape[0];
    let size_y = shape[1];
    let size_c = shape[2];
    if size_x == 0 || size_y == 0 || size_c == 0 {
        return Err(BioFormatsError::Format(
            "IM3 native dimensions must be non-zero".into(),
        ));
    }
    if data_rec.payload_len < 16 {
        return Err(BioFormatsError::Format(
            "IM3 native Data record is truncated".into(),
        ));
    }
    let data_width = im3_read_u32_le(bytes, data_rec.payload_offset + 4, "Data width")?;
    let data_height = im3_read_u32_le(bytes, data_rec.payload_offset + 8, "Data height")?;
    let data_channels = im3_read_u32_le(bytes, data_rec.payload_offset + 12, "Data channels")?;
    if (data_width, data_height, data_channels) != (size_x, size_y, size_c) {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "IM3 native Shape/Data mismatch: shape {size_x}x{size_y}x{size_c}, data {data_width}x{data_height}x{data_channels}"
        )));
    }

    let samples = (size_x as usize)
        .checked_mul(size_y as usize)
        .and_then(|v| v.checked_mul(size_c as usize))
        .ok_or_else(|| BioFormatsError::Format("IM3 native sample count overflows".into()))?;
    let interleaved_len = samples
        .checked_mul(2)
        .ok_or_else(|| BioFormatsError::Format("IM3 native byte count overflows".into()))?;
    let data_offset = data_rec
        .payload_offset
        .checked_add(16)
        .ok_or_else(|| BioFormatsError::Format("IM3 native data offset overflows".into()))?;
    if data_offset + interleaved_len > data_rec.payload_offset + data_rec.payload_len
        || data_offset + interleaved_len > bytes.len()
    {
        return Err(BioFormatsError::InvalidData(format!(
            "IM3 native Data payload is {}, expected at least {interleaved_len}",
            (data_rec.payload_offset + data_rec.payload_len).saturating_sub(data_offset)
        )));
    }

    let mut meta = ImageMetadata {
        size_x,
        size_y,
        size_z: 1,
        size_c,
        size_t: 1,
        pixel_type: PixelType::Uint16,
        bits_per_pixel: 16,
        image_count: size_c,
        is_little_endian: true,
        ..ImageMetadata::default()
    };
    meta.series_metadata.insert(
        "IM3 DataSets".into(),
        crate::common::metadata::MetadataValue::Int(dataset_count as i64),
    );
    meta.series_metadata.insert(
        "IM3 DataSet Index".into(),
        crate::common::metadata::MetadataValue::Int(dataset_index as i64),
    );
    im3_insert_container_scalar_metadata(bytes, &dataset.container, &mut meta)?;

    Ok(Im3NativeDataset {
        meta,
        data_offset: data_offset as u64,
        interleaved_len,
    })
}

fn im3_native_open_bytes_region(
    state: &Im3NativeState,
    dataset: &Im3NativeDataset,
    plane_index: u32,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
) -> Result<Vec<u8>> {
    if plane_index >= dataset.meta.image_count {
        return Err(BioFormatsError::PlaneOutOfRange(plane_index));
    }
    let meta = &dataset.meta;
    if x.checked_add(w).is_none_or(|v| v > meta.size_x)
        || y.checked_add(h).is_none_or(|v| v > meta.size_y)
    {
        return Err(BioFormatsError::Format(format!(
            "IM3 region {}x{} at {},{} exceeds image {}x{}",
            w, h, x, y, meta.size_x, meta.size_y
        )));
    }
    let mut reader = BufReader::new(File::open(&state.path).map_err(BioFormatsError::Io)?);
    let interleaved = read_bytes_at(&mut reader, dataset.data_offset, dataset.interleaved_len)?;
    let channels = meta.size_c as usize;
    let width = meta.size_x as usize;
    let mut out = Vec::with_capacity((w as usize) * (h as usize) * 2);
    for row in y as usize..(y + h) as usize {
        for col in x as usize..(x + w) as usize {
            let sample = (row * width + col)
                .checked_mul(channels)
                .and_then(|v| v.checked_add(plane_index as usize))
                .and_then(|v| v.checked_mul(2))
                .ok_or_else(|| {
                    BioFormatsError::Format("IM3 native plane offset overflows".into())
                })?;
            out.extend_from_slice(interleaved.get(sample..sample + 2).ok_or_else(|| {
                BioFormatsError::InvalidData("IM3 native interleaved payload is truncated".into())
            })?);
        }
    }
    Ok(out)
}

/// Amnis/Luminex IM3 format reader (`.im3`).
pub struct Im3Reader {
    state: Option<Im3State>,
    current_series: usize,
}

impl Im3Reader {
    pub fn new() -> Self {
        Self {
            state: None,
            current_series: 0,
        }
    }

    fn spec() -> SyntheticRawSpec {
        SyntheticRawSpec {
            format_name: "IM3",
            unsupported_message: "IM3 proprietary native decoding is unsupported for this file; explicit synthetic raw fixtures are supported",
            extension: "im3",
            magic: SYNTHETIC_IM3_MAGIC,
        }
    }
}

impl Default for Im3Reader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for Im3Reader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        Self::spec().matches_name(path)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        Self::spec().matches_bytes(header) || im3_native_cookie(header)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.state = Some(match parse_synthetic_raw(path, Self::spec()) {
            Ok(state) => Im3State::Synthetic(state),
            Err(err @ BioFormatsError::UnsupportedFormat(_)) => {
                let mut magic = vec![0u8; Self::spec().magic.len()];
                if File::open(path)
                    .and_then(|mut file| file.read_exact(&mut magic))
                    .is_ok()
                    && magic == Self::spec().magic
                {
                    return Err(err);
                }
                Im3State::Native(parse_im3_native(path)?)
            }
            Err(err) => return Err(err),
        });
        self.current_series = 0;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.state = None;
        self.current_series = 0;
        Ok(())
    }

    fn series_count(&self) -> usize {
        match self.state.as_ref() {
            Some(Im3State::Synthetic(_)) => 1,
            Some(Im3State::Native(state)) => state.datasets.len(),
            None => 0,
        }
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        match self.state.as_ref() {
            Some(Im3State::Synthetic(_)) if s == 0 => {
                self.current_series = 0;
                Ok(())
            }
            Some(Im3State::Native(state)) if s < state.datasets.len() => {
                self.current_series = s;
                Ok(())
            }
            Some(_) => Err(BioFormatsError::SeriesOutOfRange(s)),
            None => Err(BioFormatsError::NotInitialized),
        }
    }

    fn series(&self) -> usize {
        self.current_series
    }

    fn metadata(&self) -> &ImageMetadata {
        self.state
            .as_ref()
            .map(|state| match state {
                Im3State::Synthetic(state) => &state.meta,
                Im3State::Native(state) => &state.datasets[self.current_series].meta,
            })
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        let state = self.state.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        match state {
            Im3State::Synthetic(state) => synthetic_raw_open_bytes(state, Self::spec(), p),
            Im3State::Native(state) => {
                let dataset = &state.datasets[self.current_series];
                im3_native_open_bytes_region(
                    state,
                    dataset,
                    p,
                    0,
                    0,
                    dataset.meta.size_x,
                    dataset.meta.size_y,
                )
            }
        }
    }

    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let state = self.state.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        match state {
            Im3State::Synthetic(state) => {
                synthetic_raw_open_bytes_region(state, Self::spec(), p, x, y, w, h)
            }
            Im3State::Native(state) => im3_native_open_bytes_region(
                state,
                &state.datasets[self.current_series],
                p,
                x,
                y,
                w,
                h,
            ),
        }
    }

    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        let meta = self.metadata();
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(p, tx, ty, tw, th)
    }

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        let state = self.state.as_ref()?;
        match state {
            Im3State::Synthetic(state) => {
                let mut ome =
                    crate::common::ome_metadata::OmeMetadata::from_image_metadata(&state.meta);
                let _ = ome.add_original_metadata_annotations(&state.meta, 0);
                Some(ome)
            }
            Im3State::Native(state) => {
                let mut ome = crate::common::ome_metadata::OmeMetadata::default();
                for (index, dataset) in state.datasets.iter().enumerate() {
                    let mut image = crate::common::ome_metadata::OmeMetadata::from_image_metadata(
                        &dataset.meta,
                    );
                    ome.images.extend(image.images.drain(..));
                    let _ = ome.add_original_metadata_annotations(&dataset.meta, index);
                }
                Some(ome)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 3. 3i SlideBook 7 — native directory subset + synthetic raw fixtures
// ---------------------------------------------------------------------------
/// 3i SlideBook 7 format reader (`.sld`, `.sldy`, `.sldyz`).
pub struct SlideBook7Reader {
    state: Option<SlideBook7State>,
    current_series: usize,
}

enum SlideBook7State {
    Synthetic(SyntheticRawState),
    Native(SlideBook7NativeState),
}

struct SlideBook7NativeState {
    series: Vec<SlideBook7Series>,
    extracted_dir: Option<PathBuf>,
}

struct SlideBook7Series {
    meta: ImageMetadata,
    files: Vec<SlideBook7PlaneFile>,
    plane_len: usize,
}

#[derive(Clone)]
struct SlideBook7PlaneFile {
    path: PathBuf,
    header_len: u64,
    z_planes: u32,
    timepoint: u32,
    channel: u32,
    compressed: bool,
}

struct SlideBook7NpyHeader {
    header_len: u64,
    pixel_type: PixelType,
    little_endian: bool,
    fortran_order: bool,
    shape: Vec<u32>,
}

struct SlideBook7NativeRoot {
    root: PathBuf,
    extracted_dir: Option<PathBuf>,
}

fn slidebook7_missing_record(path: &Path, key: &str) -> BioFormatsError {
    BioFormatsError::Format(format!(
        "SlideBook 7 ImageRecord {} is missing {key}",
        path.display()
    ))
}

fn slidebook7_safe_archive_entry(name: &str) -> Option<PathBuf> {
    let mut rel = PathBuf::new();
    for component in Path::new(name).components() {
        match component {
            std::path::Component::Normal(part) => rel.push(part),
            std::path::Component::CurDir => {}
            _ => return None,
        }
    }
    (!rel.as_os_str().is_empty()).then_some(rel)
}

fn slidebook7_group_dirs(root: &Path) -> Result<Vec<PathBuf>> {
    let mut groups = Vec::new();
    for entry in std::fs::read_dir(root).map_err(BioFormatsError::Io)? {
        let entry = entry.map_err(BioFormatsError::Io)?;
        let group_path = entry.path();
        if !group_path.is_dir() {
            continue;
        }
        let Some(name) = group_path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.ends_with(".imgdir") {
            continue;
        }
        if group_path.join("ImageRecord.yaml").is_file() {
            groups.push(group_path);
        }
    }
    groups.sort();
    Ok(groups)
}

fn slidebook7_nested_dir_roots(root: &Path) -> Result<Vec<PathBuf>> {
    let mut roots = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).map_err(BioFormatsError::Io)? {
            let entry = entry.map_err(BioFormatsError::Io)?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|name| name.ends_with(".dir"))
            {
                roots.push(path.clone());
            }
            pending.push(path);
        }
    }
    roots.sort();
    roots.dedup();
    Ok(roots)
}

fn slidebook7_yaml_u32(text: &str, keys: &[&str]) -> Option<u32> {
    for line in text.lines() {
        let trimmed = line.trim();
        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };
        let key = key.trim().trim_matches('"').trim_matches('\'');
        if !keys.iter().any(|candidate| key == *candidate) {
            continue;
        }
        let value = value
            .split('#')
            .next()
            .unwrap_or_default()
            .trim()
            .trim_matches('"')
            .trim_matches('\'');
        if let Ok(parsed) = value.parse::<u32>() {
            return Some(parsed);
        }
    }
    None
}

fn slidebook7_yaml_raw_scalar(value: &str) -> String {
    value
        .split('#')
        .next()
        .unwrap_or_default()
        .trim()
        .trim_end_matches(',')
        .trim()
        .to_string()
}

fn slidebook7_yaml_scalar(line: &str) -> Option<(usize, bool, String, String, String)> {
    let indent = line.len().saturating_sub(line.trim_start().len());
    let mut trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    let is_list_item = trimmed.starts_with('-');
    if is_list_item {
        trimmed = trimmed[1..].trim_start();
        if trimmed.is_empty() {
            return Some((indent, true, String::new(), String::new(), String::new()));
        }
    }
    let (key, value) = trimmed.split_once(':')?;
    let key = key.trim().trim_matches('"').trim_matches('\'');
    if key.is_empty() {
        return None;
    }
    let raw_value = slidebook7_yaml_raw_scalar(value);
    let value = slidebook7_clean_yaml_scalar(&raw_value).unwrap_or_default();
    Some((indent, is_list_item, key.to_string(), value, raw_value))
}

fn slidebook7_clean_yaml_scalar(value: &str) -> Option<String> {
    let value = value.trim().trim_end_matches(',');
    if value.is_empty()
        || value == "[]"
        || value == "{}"
        || value.starts_with('[')
        || value.starts_with('{')
    {
        return None;
    }
    let value = value
        .trim_matches('"')
        .trim_matches('\'')
        .trim_matches(',')
        .trim();
    if value.is_empty() || value.len() > 512 {
        return None;
    }
    Some(value.to_string())
}

fn slidebook7_split_flow_items(text: &str) -> Option<Vec<String>> {
    let mut items = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    for ch in text.chars() {
        match quote {
            Some(q) if ch == q => {
                quote = None;
                current.push(ch);
            }
            Some(_) => current.push(ch),
            None if ch == '\'' || ch == '"' => {
                quote = Some(ch);
                current.push(ch);
            }
            None if ch == ',' => {
                items.push(current.trim().to_string());
                current.clear();
            }
            None if matches!(ch, '[' | ']' | '{' | '}') => return None,
            None => current.push(ch),
        }
    }
    if quote.is_some() {
        return None;
    }
    items.push(current.trim().to_string());
    Some(items)
}

fn slidebook7_inline_scalar_map(raw_value: &str) -> Option<Vec<(String, String)>> {
    let trimmed = raw_value.trim();
    let inner = trimmed.strip_prefix('{')?.strip_suffix('}')?.trim();
    if inner.is_empty() {
        return Some(Vec::new());
    }
    let mut pairs = Vec::new();
    for item in slidebook7_split_flow_items(inner)? {
        let (key, value) = item.split_once(':')?;
        let key = key.trim().trim_matches('"').trim_matches('\'');
        if key.is_empty() {
            return None;
        }
        pairs.push((key.to_string(), slidebook7_clean_yaml_scalar(value)?));
    }
    Some(pairs)
}

fn slidebook7_inline_scalar_list(raw_value: &str) -> Option<Vec<String>> {
    let trimmed = raw_value.trim();
    let inner = trimmed.strip_prefix('[')?.strip_suffix(']')?.trim();
    if inner.is_empty() {
        return Some(Vec::new());
    }
    let mut values = Vec::new();
    for item in slidebook7_split_flow_items(inner)? {
        values.push(slidebook7_clean_yaml_scalar(&item)?);
    }
    Some(values)
}

fn slidebook7_is_channel_list_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    lower == "channels"
        || lower == "mchannels"
        || lower == "channelrecords"
        || lower == "mchannelrecords"
}

fn slidebook7_is_channel_name_key(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "name" | "mname" | "channelname" | "mchannelname"
    )
}

fn slidebook7_elapsed_key(key: &str) -> Option<&'static str> {
    let lower = key.to_ascii_lowercase();
    (lower.contains("elapsed") && lower.contains("time")).then_some("elapsed_time")
}

fn slidebook7_position_key(key: &str) -> Option<&'static str> {
    let lower = key.to_ascii_lowercase();
    if !(lower.contains("position") || lower.contains("stage")) {
        return None;
    }
    if lower.ends_with('x') || lower.contains("_x") || lower.contains(" x") {
        Some("position_x")
    } else if lower.ends_with('y') || lower.contains("_y") || lower.contains(" y") {
        Some("position_y")
    } else if lower.ends_with('z') || lower.contains("_z") || lower.contains(" z") {
        Some("position_z")
    } else {
        None
    }
}

fn slidebook7_inline_position_key(parent_key: &str, child_key: &str) -> Option<&'static str> {
    let parent = parent_key.to_ascii_lowercase();
    if !(parent.contains("position") || parent.contains("stage")) {
        return None;
    }
    match child_key.to_ascii_lowercase().as_str() {
        "x" | "mx" => Some("position_x"),
        "y" | "my" => Some("position_y"),
        "z" | "mz" => Some("position_z"),
        _ => None,
    }
}

fn slidebook7_insert_float_metadata(
    metadata: &mut HashMap<String, MetadataValue>,
    key: String,
    value: &str,
) {
    if let Ok(parsed) = value.parse::<f64>() {
        if parsed.is_finite() {
            metadata.insert(key, MetadataValue::Float(parsed));
        }
    }
}

fn slidebook7_metadata_key(key: &str) -> String {
    let mut out = String::new();
    for ch in key.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if !out.ends_with('_') {
            out.push('_');
        }
    }
    let out = out.trim_matches('_');
    if out.is_empty() {
        "value".to_string()
    } else {
        out.to_string()
    }
}

fn slidebook7_insert_scalar_metadata(
    metadata: &mut HashMap<String, MetadataValue>,
    key: String,
    value: &str,
) {
    if let Ok(parsed) = value.parse::<i64>() {
        metadata.insert(key, MetadataValue::Int(parsed));
    } else if let Ok(parsed) = value.parse::<f64>() {
        if parsed.is_finite() {
            metadata.insert(key, MetadataValue::Float(parsed));
        }
    } else {
        match value.to_ascii_lowercase().as_str() {
            "true" => {
                metadata.insert(key, MetadataValue::Bool(true));
            }
            "false" => {
                metadata.insert(key, MetadataValue::Bool(false));
            }
            _ => {
                metadata.insert(key, MetadataValue::String(value.to_string()));
            }
        };
    }
}

fn slidebook7_yaml_record_path(stack: &[(usize, String)], key: &str) -> String {
    let mut parts = stack
        .iter()
        .map(|(_, part)| part.as_str())
        .collect::<Vec<_>>();
    if !key.is_empty() {
        parts.push(key);
    }
    parts
        .into_iter()
        .filter(|part| !part.is_empty())
        .map(slidebook7_metadata_key)
        .collect::<Vec<_>>()
        .join(".")
}

fn slidebook7_image_record_metadata(text: &str) -> HashMap<String, MetadataValue> {
    let mut metadata = HashMap::new();
    let mut yaml_stack: Vec<(usize, String)> = Vec::new();
    let mut list_counts: HashMap<String, u32> = HashMap::new();
    let mut channel_list_indent: Option<usize> = None;
    let mut current_channel: Option<u32> = None;
    let mut channel_count = 0u32;

    for line in text.lines() {
        let Some((indent, is_list_item, key, value, raw_value)) = slidebook7_yaml_scalar(line)
        else {
            continue;
        };
        while yaml_stack
            .last()
            .is_some_and(|(stack_indent, _)| indent <= *stack_indent)
        {
            yaml_stack.pop();
        }
        if is_list_item {
            let parent_path = slidebook7_yaml_record_path(&yaml_stack, "");
            let item = list_counts.entry(parent_path).or_insert(0);
            yaml_stack.push((indent, item.to_string()));
            *item = item.saturating_add(1);
        }
        if !value.is_empty() {
            let record_path = slidebook7_yaml_record_path(&yaml_stack, &key);
            if !record_path.is_empty() {
                slidebook7_insert_scalar_metadata(
                    &mut metadata,
                    format!("slidebook7.record.{record_path}"),
                    &value,
                );
            }
        } else if let Some(pairs) = slidebook7_inline_scalar_map(&raw_value) {
            let record_path = slidebook7_yaml_record_path(&yaml_stack, &key);
            if !record_path.is_empty() {
                for (child_key, child_value) in pairs {
                    let child_path =
                        format!("{record_path}.{}", slidebook7_metadata_key(&child_key));
                    slidebook7_insert_scalar_metadata(
                        &mut metadata,
                        format!("slidebook7.record.{child_path}"),
                        &child_value,
                    );
                    if let Some(channel) = current_channel {
                        if let Some(field) = slidebook7_inline_position_key(&key, &child_key) {
                            slidebook7_insert_float_metadata(
                                &mut metadata,
                                format!("slidebook7.channel.{channel}.{field}"),
                                &child_value,
                            );
                        }
                    } else if let Some(field) = slidebook7_inline_position_key(&key, &child_key) {
                        slidebook7_insert_float_metadata(
                            &mut metadata,
                            format!("slidebook7.{field}"),
                            &child_value,
                        );
                    }
                }
            }
        } else if let Some(values) = slidebook7_inline_scalar_list(&raw_value) {
            let record_path = slidebook7_yaml_record_path(&yaml_stack, &key);
            if !record_path.is_empty() {
                for (index, child_value) in values.into_iter().enumerate() {
                    slidebook7_insert_scalar_metadata(
                        &mut metadata,
                        format!("slidebook7.record.{record_path}.{index}"),
                        &child_value,
                    );
                }
            }
        } else if !key.is_empty() {
            yaml_stack.push((indent, key.clone()));
        }

        if channel_list_indent.is_some_and(|list_indent| indent <= list_indent) {
            channel_list_indent = None;
            current_channel = None;
        }
        if slidebook7_is_channel_list_key(&key) {
            channel_list_indent = Some(indent);
            current_channel = None;
            continue;
        }
        if is_list_item && channel_list_indent.is_some() {
            current_channel = Some(channel_count);
            channel_count = channel_count.saturating_add(1);
        }

        if let Some(channel) = current_channel {
            let prefix = format!("slidebook7.channel.{channel}");
            if slidebook7_is_channel_name_key(&key) {
                metadata.insert(format!("{prefix}.name"), MetadataValue::String(value));
                continue;
            }
            if let Some(field) = slidebook7_elapsed_key(&key) {
                slidebook7_insert_float_metadata(
                    &mut metadata,
                    format!("{prefix}.{field}"),
                    &value,
                );
                continue;
            }
            if let Some(field) = slidebook7_position_key(&key) {
                slidebook7_insert_float_metadata(
                    &mut metadata,
                    format!("{prefix}.{field}"),
                    &value,
                );
                continue;
            }
        }

        if let Some(field) = slidebook7_elapsed_key(&key) {
            slidebook7_insert_float_metadata(&mut metadata, format!("slidebook7.{field}"), &value);
        } else if let Some(field) = slidebook7_position_key(&key) {
            slidebook7_insert_float_metadata(&mut metadata, format!("slidebook7.{field}"), &value);
        }
    }

    metadata
}

fn slidebook7_filename_number(name: &str, marker: &str) -> Option<u32> {
    let start = name.find(marker)? + marker.len();
    let rest = name.get(start..)?;
    let digits = rest
        .char_indices()
        .take_while(|(_, c)| c.is_ascii_digit())
        .last()
        .map(|(idx, c)| idx + c.len_utf8())?;
    let value = &rest[..digits];
    if value.is_empty() {
        return None;
    }
    value.parse::<u32>().ok()
}

fn slidebook7_filename_fixed_number(name: &str, marker: &str, digits: usize) -> Option<u32> {
    let start = name.find(marker)? + marker.len();
    let value = name.get(start..start + digits)?;
    if !value.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    value.parse::<u32>().ok()
}

fn slidebook7_npy_zyx_shape(shape: &[u32], _declared_z: u32) -> Result<(u32, u32, u32)> {
    match shape {
        [y, x] => Ok((1, *y, *x)),
        [z, y, x] => Ok((*z, *y, *x)),
        _ => Err(BioFormatsError::UnsupportedFormat(format!(
            "SlideBook 7 unsupported NPY shape {shape:?}; expected YX or ZYX"
        ))),
    }
}

fn slidebook7_npy_pixel_type(descr: &str) -> Result<(PixelType, bool)> {
    let (endian, dtype) = descr.split_at(1);
    let little_endian = match endian {
        "<" | "|" => true,
        ">" => false,
        other => {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "SlideBook 7 unsupported NPY byte order marker {other:?}"
            )));
        }
    };
    let pixel_type = match dtype {
        "u1" => PixelType::Uint8,
        "i1" => PixelType::Int8,
        "u2" => PixelType::Uint16,
        "i2" => PixelType::Int16,
        "u4" => PixelType::Uint32,
        "i4" => PixelType::Int32,
        "f4" => PixelType::Float32,
        "f8" => PixelType::Float64,
        other => {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "SlideBook 7 unsupported NPY pixel type {other:?}"
            )));
        }
    };
    Ok((pixel_type, little_endian))
}

fn slidebook7_header_string_value<'a>(header: &'a str, key: &str) -> Option<&'a str> {
    let key_pos = header.find(key)?;
    let after_key = &header[key_pos + key.len()..];
    let colon = after_key.find(':')?;
    let after_colon = after_key[colon + 1..].trim_start();
    let quote = after_colon.chars().next()?;
    if quote != '\'' && quote != '"' {
        return None;
    }
    let rest = &after_colon[quote.len_utf8()..];
    let end = rest.find(quote)?;
    Some(&rest[..end])
}

fn slidebook7_header_bool_value(header: &str, key: &str) -> Option<bool> {
    let key_pos = header.find(key)?;
    let after_key = &header[key_pos + key.len()..];
    let colon = after_key.find(':')?;
    let value = after_key[colon + 1..].trim_start();
    if value.starts_with("True") {
        Some(true)
    } else if value.starts_with("False") {
        Some(false)
    } else {
        None
    }
}

fn slidebook7_header_shape(header: &str) -> Result<Vec<u32>> {
    let start = header
        .find('(')
        .ok_or_else(|| BioFormatsError::Format("SlideBook 7 NPY header lacks shape".into()))?;
    let end = header[start + 1..]
        .find(')')
        .map(|idx| start + 1 + idx)
        .ok_or_else(|| BioFormatsError::Format("SlideBook 7 NPY header lacks shape".into()))?;
    let mut shape = Vec::new();
    for part in header[start + 1..end].split(',') {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            continue;
        }
        shape.push(trimmed.parse::<u32>().map_err(|_| {
            BioFormatsError::Format(format!(
                "SlideBook 7 NPY header has invalid shape value {trimmed:?}"
            ))
        })?);
    }
    if shape.len() < 2 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "SlideBook 7 unsupported NPY shape {shape:?}"
        )));
    }
    Ok(shape)
}

fn slidebook7_npyz_decode(data: &[u8], path: &Path) -> Result<Vec<u8>> {
    for kind in 0..3 {
        let mut decoded = Vec::new();
        let result = match kind {
            0 => flate2::read::GzDecoder::new(data).read_to_end(&mut decoded),
            1 => flate2::read::ZlibDecoder::new(data).read_to_end(&mut decoded),
            _ => flate2::read::DeflateDecoder::new(data).read_to_end(&mut decoded),
        };
        if result.is_ok() && decoded.starts_with(b"\x93NUMPY") {
            return Ok(decoded);
        }
    }
    Err(BioFormatsError::UnsupportedFormat(format!(
        "SlideBook 7 NPYZ image data is not a gzip/zlib/deflate-compressed NPY payload: {}",
        path.display()
    )))
}

fn slidebook7_npy_bytes(path: &Path) -> Result<(Vec<u8>, bool)> {
    let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
    if path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("npyz"))
    {
        Ok((slidebook7_npyz_decode(&data, path)?, true))
    } else {
        Ok((data, false))
    }
}

fn parse_slidebook7_npy_header_bytes(data: &[u8], path: &Path) -> Result<SlideBook7NpyHeader> {
    if data.len() < 10 || &data[..6] != b"\x93NUMPY" {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "SlideBook 7 image data is not a NPY file: {}",
            path.display()
        )));
    }
    let major = data[6];
    let minor = data[7];
    let (header_len, header_start) = match major {
        1 => (u16::from_le_bytes([data[8], data[9]]) as usize, 10usize),
        2 | 3 => {
            if data.len() < 12 {
                return Err(BioFormatsError::Format(format!(
                    "SlideBook 7 NPY header is truncated: {}",
                    path.display()
                )));
            }
            (
                u32::from_le_bytes([data[8], data[9], data[10], data[11]]) as usize,
                12usize,
            )
        }
        other => {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "SlideBook 7 unsupported NPY version {other}.{minor}: {}",
                path.display()
            )));
        }
    };
    let header_end = header_start.checked_add(header_len).ok_or_else(|| {
        BioFormatsError::Format(format!(
            "SlideBook 7 NPY header length overflows: {}",
            path.display()
        ))
    })?;
    if header_end > data.len() {
        return Err(BioFormatsError::Format(format!(
            "SlideBook 7 NPY header extends past end of file: {}",
            path.display()
        )));
    }
    let header = std::str::from_utf8(&data[header_start..header_end]).map_err(|_| {
        BioFormatsError::Format(format!(
            "SlideBook 7 NPY header is not UTF-8: {}",
            path.display()
        ))
    })?;
    let descr = slidebook7_header_string_value(header, "descr").ok_or_else(|| {
        BioFormatsError::Format(format!(
            "SlideBook 7 NPY header lacks dtype descriptor: {}",
            path.display()
        ))
    })?;
    let (pixel_type, little_endian) = slidebook7_npy_pixel_type(descr)?;
    let fortran_order = slidebook7_header_bool_value(header, "fortran_order").ok_or_else(|| {
        BioFormatsError::Format(format!(
            "SlideBook 7 NPY header lacks fortran_order: {}",
            path.display()
        ))
    })?;
    Ok(SlideBook7NpyHeader {
        header_len: header_end as u64,
        pixel_type,
        little_endian,
        fortran_order,
        shape: slidebook7_header_shape(header)?,
    })
}

fn parse_slidebook7_npy_header(path: &Path) -> Result<SlideBook7NpyHeader> {
    let (data, _) = slidebook7_npy_bytes(path)?;
    parse_slidebook7_npy_header_bytes(&data, path)
}

impl SlideBook7Reader {
    pub fn new() -> Self {
        Self {
            state: None,
            current_series: 0,
        }
    }

    fn spec() -> SyntheticRawSpec {
        SyntheticRawSpec {
            format_name: "SlideBook 7",
            unsupported_message: "SlideBook 7 native payload is not a supported uncompressed .sldy directory; explicit synthetic raw fixtures are supported",
            extension: "sld",
            magic: SYNTHETIC_SLIDEBOOK7_MAGIC,
        }
    }

    fn extract_sldyz(path: &Path) -> Result<SlideBook7NativeRoot> {
        let file = File::open(path).map_err(BioFormatsError::Io)?;
        let mut archive = zip::ZipArchive::new(file).map_err(|e| {
            BioFormatsError::Format(format!(
                "SlideBook 7 compressed .sldyz archive open error: {e}"
            ))
        })?;
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let extracted_dir = std::env::temp_dir().join(format!(
            "bioformats_slidebook7_sldyz_{}_{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&extracted_dir).map_err(BioFormatsError::Io)?;

        let result = (|| -> Result<SlideBook7NativeRoot> {
            for i in 0..archive.len() {
                let mut entry = archive.by_index(i).map_err(|e| {
                    BioFormatsError::Format(format!(
                        "SlideBook 7 compressed .sldyz archive entry error: {e}"
                    ))
                })?;
                let name = entry.name().to_string();
                let rel_path = slidebook7_safe_archive_entry(&name).ok_or_else(|| {
                    BioFormatsError::UnsupportedFormat(format!(
                        "SlideBook 7 compressed .sldyz archive has unsafe entry path: {name}"
                    ))
                })?;
                let out_path = extracted_dir.join(rel_path);
                if entry.is_dir() {
                    std::fs::create_dir_all(&out_path).map_err(BioFormatsError::Io)?;
                    continue;
                }
                if let Some(parent) = out_path.parent() {
                    std::fs::create_dir_all(parent).map_err(BioFormatsError::Io)?;
                }
                let mut buf = Vec::new();
                entry.read_to_end(&mut buf).map_err(BioFormatsError::Io)?;
                std::fs::write(&out_path, &buf).map_err(BioFormatsError::Io)?;
            }

            for root in slidebook7_nested_dir_roots(&extracted_dir)? {
                if !slidebook7_group_dirs(&root)?.is_empty() {
                    return Ok(SlideBook7NativeRoot {
                        root,
                        extracted_dir: Some(extracted_dir.clone()),
                    });
                }
            }
            if !slidebook7_group_dirs(&extracted_dir)?.is_empty() {
                return Ok(SlideBook7NativeRoot {
                    root: extracted_dir.clone(),
                    extracted_dir: Some(extracted_dir.clone()),
                });
            }
            Err(BioFormatsError::UnsupportedFormat(
                "SlideBook 7 compressed .sldyz archive has no image groups with ImageRecord.yaml"
                    .into(),
            ))
        })();

        if result.is_err() {
            let _ = std::fs::remove_dir_all(&extracted_dir);
        }
        result
    }

    fn native_root(path: &Path) -> Result<SlideBook7NativeRoot> {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        if ext.as_deref() == Some("sldyz") {
            return Self::extract_sldyz(path);
        }
        if ext.as_deref() != Some("sldy") {
            return Err(synthetic_raw_unsupported(Self::spec()));
        }
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| BioFormatsError::Format("SlideBook 7 path has no valid stem".into()))?;
        Ok(SlideBook7NativeRoot {
            root: path.with_file_name(format!("{stem}.dir")),
            extracted_dir: None,
        })
    }

    fn parse_native(path: &Path) -> Result<SlideBook7NativeState> {
        let native_root = Self::native_root(path)?;
        let root = native_root.root;
        if !root.is_dir() {
            if let Some(dir) = native_root.extracted_dir {
                let _ = std::fs::remove_dir_all(dir);
            }
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "SlideBook 7 native root directory {} is missing",
                root.display()
            )));
        }
        let groups = match slidebook7_group_dirs(&root) {
            Ok(groups) => groups,
            Err(err) => {
                if let Some(dir) = native_root.extracted_dir {
                    let _ = std::fs::remove_dir_all(dir);
                }
                return Err(err);
            }
        };
        if groups.is_empty() {
            if let Some(dir) = native_root.extracted_dir {
                let _ = std::fs::remove_dir_all(dir);
            }
            return Err(BioFormatsError::UnsupportedFormat(
                "SlideBook 7 native directory has no image groups with ImageRecord.yaml".into(),
            ));
        }

        let mut series = Vec::with_capacity(groups.len());
        for group in groups {
            match Self::parse_native_group(&group) {
                Ok(parsed) => series.push(parsed),
                Err(err) => {
                    if let Some(dir) = native_root.extracted_dir {
                        let _ = std::fs::remove_dir_all(dir);
                    }
                    return Err(err);
                }
            }
        }
        Ok(SlideBook7NativeState {
            series,
            extracted_dir: native_root.extracted_dir,
        })
    }

    fn parse_native_group(group: &Path) -> Result<SlideBook7Series> {
        let record_path = group.join("ImageRecord.yaml");
        let record = std::fs::read_to_string(&record_path).map_err(BioFormatsError::Io)?;
        let size_x = slidebook7_yaml_u32(&record, &["mWidth", "Width", "NumColumns"])
            .ok_or_else(|| slidebook7_missing_record(&record_path, "mWidth"))?;
        let size_y = slidebook7_yaml_u32(&record, &["mHeight", "Height", "NumRows"])
            .ok_or_else(|| slidebook7_missing_record(&record_path, "mHeight"))?;
        let size_z = slidebook7_yaml_u32(&record, &["mNumPlanes", "NumPlanes", "Planes"])
            .unwrap_or(1)
            .max(1);
        let declared_c = slidebook7_yaml_u32(&record, &["mNumChannels", "NumChannels", "Channels"])
            .unwrap_or(1)
            .max(1);
        let declared_t =
            slidebook7_yaml_u32(&record, &["mNumTimepoints", "NumTimepoints", "Timepoints"])
                .unwrap_or(1)
                .max(1);
        if size_x == 0 || size_y == 0 {
            return Err(BioFormatsError::Format(format!(
                "SlideBook 7 ImageRecord {} has zero dimensions",
                record_path.display()
            )));
        }

        let mut files = Vec::new();
        for entry in std::fs::read_dir(group).map_err(BioFormatsError::Io)? {
            let entry = entry.map_err(BioFormatsError::Io)?;
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let is_npy = name.ends_with(".npy");
            let is_npyz = name.ends_with(".npyz");
            if !name.starts_with("ImageData_") || (!is_npy && !is_npyz) {
                continue;
            }
            let channel = slidebook7_filename_number(name, "_Ch").ok_or_else(|| {
                BioFormatsError::Format(format!(
                    "SlideBook 7 image data filename lacks channel index: {name}"
                ))
            })?;
            let timepoint = slidebook7_filename_fixed_number(name, "_TP", 7).ok_or_else(|| {
                BioFormatsError::Format(format!(
                    "SlideBook 7 image data filename lacks timepoint index: {name}"
                ))
            })?;
            let npy = parse_slidebook7_npy_header(&path)?;
            if npy.fortran_order {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "SlideBook 7 Fortran-order NPY image data is unsupported: {}",
                    path.display()
                )));
            }
            let (npy_z, npy_y, npy_x) = slidebook7_npy_zyx_shape(&npy.shape, size_z)?;
            if npy_x != size_x || npy_y != size_y {
                return Err(BioFormatsError::Format(format!(
                    "SlideBook 7 NPY shape {:?} does not match ImageRecord {}x{} for {}",
                    npy.shape,
                    size_x,
                    size_y,
                    path.display()
                )));
            }
            files.push(SlideBook7PlaneFile {
                path,
                header_len: npy.header_len,
                z_planes: npy_z,
                timepoint,
                channel,
                compressed: is_npyz,
            });
        }
        if files.is_empty() {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "SlideBook 7 image group {} has no ImageData .npy/.npyz files",
                group.display()
            )));
        }
        files.sort_by_key(|f| (f.timepoint, f.channel, f.path.clone()));

        let first = parse_slidebook7_npy_header(&files[0].path)?;
        let pixel_type = first.pixel_type;
        let bits_per_pixel = (pixel_type.bytes_per_sample() * 8) as u8;
        let bytes_per_sample = pixel_type.bytes_per_sample();
        for file in &files[1..] {
            let npy = parse_slidebook7_npy_header(&file.path)?;
            if npy.pixel_type != pixel_type {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "SlideBook 7 mixed NPY pixel types are unsupported in {}",
                    group.display()
                )));
            }
            if npy.little_endian != first.little_endian {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "SlideBook 7 mixed NPY byte orders are unsupported in {}",
                    group.display()
                )));
            }
        }

        let max_c = files
            .iter()
            .map(|f| f.channel + 1)
            .max()
            .unwrap_or(declared_c);
        let max_t = files
            .iter()
            .map(|f| f.timepoint + 1)
            .max()
            .unwrap_or(declared_t);
        let size_c = declared_c.max(max_c);
        let mut size_t = declared_t.max(max_t);
        let single_file_timepoints = files.len() as u32 == size_c && size_z == 1;
        if single_file_timepoints {
            let max_file_z = files.iter().map(|f| f.z_planes).max().unwrap_or(1);
            size_t = size_t.max(max_file_z);
        }
        let image_count = size_z
            .checked_mul(size_c)
            .and_then(|v| v.checked_mul(size_t))
            .ok_or_else(|| BioFormatsError::Format("SlideBook 7 image count overflows".into()))?;
        let plane_len = (size_x as usize)
            .checked_mul(size_y as usize)
            .and_then(|v| v.checked_mul(bytes_per_sample))
            .ok_or_else(|| {
                BioFormatsError::Format("SlideBook 7 plane byte count overflows".into())
            })?;
        let mut meta = ImageMetadata {
            size_x,
            size_y,
            size_z,
            size_c,
            size_t,
            pixel_type,
            bits_per_pixel,
            image_count,
            is_little_endian: first.little_endian,
            is_interleaved: true,
            ..ImageMetadata::default()
        };
        meta.series_metadata = slidebook7_image_record_metadata(&record);
        Ok(SlideBook7Series {
            meta,
            files,
            plane_len,
        })
    }

    fn native_series(&self) -> Result<&SlideBook7Series> {
        match self.state.as_ref() {
            Some(SlideBook7State::Native(state)) => state
                .series
                .get(self.current_series)
                .ok_or(BioFormatsError::NotInitialized),
            _ => Err(BioFormatsError::NotInitialized),
        }
    }

    fn native_open_bytes(&self, p: u32) -> Result<Vec<u8>> {
        let series = self.native_series()?;
        if p >= series.meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(p));
        }
        let size_z = series.meta.size_z;
        let size_c = series.meta.size_c;
        let z = p % size_z;
        let c = (p / size_z) % size_c;
        let t = p / (size_z * size_c);
        let file = series
            .files
            .iter()
            .find(|f| {
                f.channel == c
                    && ((f.timepoint == t && z < f.z_planes)
                        || (series.meta.size_z == 1 && f.timepoint == 0 && t < f.z_planes))
            })
            .ok_or_else(|| {
                BioFormatsError::UnsupportedFormat(format!(
                    "SlideBook 7 missing ImageData plane for T={t} C={c} Z={z}"
                ))
            })?;
        let file_z = if series.meta.size_z == 1 && file.timepoint == 0 && t < file.z_planes {
            t
        } else {
            z
        };
        let offset = file
            .header_len
            .checked_add(
                (file_z as u64)
                    .checked_mul(series.plane_len as u64)
                    .ok_or_else(|| {
                        BioFormatsError::Format("SlideBook 7 plane offset overflows".into())
                    })?,
            )
            .ok_or_else(|| BioFormatsError::Format("SlideBook 7 plane offset overflows".into()))?;
        if file.compressed {
            let (data, _) = slidebook7_npy_bytes(&file.path)?;
            let start = offset as usize;
            let end = start.checked_add(series.plane_len).ok_or_else(|| {
                BioFormatsError::Format("SlideBook 7 plane range overflows".into())
            })?;
            return data
                .get(start..end)
                .map(|plane| plane.to_vec())
                .ok_or_else(|| {
                    BioFormatsError::InvalidData(format!(
                        "SlideBook 7 compressed ImageData plane is truncated: {}",
                        file.path.display()
                    ))
                });
        }
        let mut reader = BufReader::new(File::open(&file.path).map_err(BioFormatsError::Io)?);
        read_bytes_at(&mut reader, offset, series.plane_len)
    }
}

impl Default for SlideBook7Reader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for SlideBook7Reader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        Self::spec().matches_name(path)
            || path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| matches!(e.to_ascii_lowercase().as_str(), "sldy" | "sldyz"))
                .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        Self::spec().matches_bytes(header)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        match parse_synthetic_raw(path, Self::spec()) {
            Ok(state) => {
                self.state = Some(SlideBook7State::Synthetic(state));
                Ok(())
            }
            Err(BioFormatsError::UnsupportedFormat(_)) => {
                self.state = Some(SlideBook7State::Native(Self::parse_native(path)?));
                Ok(())
            }
            Err(err) => Err(err),
        }
    }

    fn close(&mut self) -> Result<()> {
        if let Some(SlideBook7State::Native(state)) = self.state.as_mut() {
            if let Some(dir) = state.extracted_dir.take() {
                let _ = std::fs::remove_dir_all(dir);
            }
        }
        self.state = None;
        self.current_series = 0;
        Ok(())
    }

    fn series_count(&self) -> usize {
        match &self.state {
            Some(SlideBook7State::Synthetic(_)) => 1,
            Some(SlideBook7State::Native(state)) => state.series.len(),
            None => 0,
        }
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        match &self.state {
            Some(SlideBook7State::Synthetic(_)) if s == 0 => {
                self.current_series = 0;
                Ok(())
            }
            Some(SlideBook7State::Native(state)) if s < state.series.len() => {
                self.current_series = s;
                Ok(())
            }
            Some(_) => Err(BioFormatsError::SeriesOutOfRange(s)),
            None => Err(BioFormatsError::NotInitialized),
        }
    }

    fn series(&self) -> usize {
        self.current_series
    }

    fn metadata(&self) -> &ImageMetadata {
        match &self.state {
            Some(SlideBook7State::Synthetic(state)) => &state.meta,
            Some(SlideBook7State::Native(state)) => state
                .series
                .get(self.current_series)
                .map(|s| &s.meta)
                .unwrap_or(crate::common::reader::uninitialized_metadata()),
            None => crate::common::reader::uninitialized_metadata(),
        }
    }

    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        match &self.state {
            Some(SlideBook7State::Synthetic(state)) => {
                synthetic_raw_open_bytes(state, Self::spec(), p)
            }
            Some(SlideBook7State::Native(_)) => self.native_open_bytes(p),
            None => Err(BioFormatsError::NotInitialized),
        }
    }

    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        match &self.state {
            Some(SlideBook7State::Synthetic(state)) => {
                synthetic_raw_open_bytes_region(state, Self::spec(), p, x, y, w, h)
            }
            Some(SlideBook7State::Native(_)) => {
                let full = self.native_open_bytes(p)?;
                let meta = self.metadata().clone();
                crop_full_plane("SlideBook 7", &full, &meta, 1, x, y, w, h)
            }
            None => Err(BioFormatsError::NotInitialized),
        }
    }

    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        let meta = self.metadata();
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(p, tx, ty, tw, th)
    }

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        match self.state.as_ref()? {
            SlideBook7State::Synthetic(state) => {
                let mut ome =
                    crate::common::ome_metadata::OmeMetadata::from_image_metadata(&state.meta);
                let _ = ome.add_original_metadata_annotations(&state.meta, 0);
                Some(ome)
            }
            SlideBook7State::Native(state) => {
                let mut ome = crate::common::ome_metadata::OmeMetadata::default();
                for (index, series) in state.series.iter().enumerate() {
                    let mut image =
                        crate::common::ome_metadata::OmeMetadata::from_image_metadata(&series.meta);
                    ome.images.extend(image.images.drain(..));
                    let _ = ome.add_original_metadata_annotations(&series.meta, index);
                }
                Some(ome)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 4. NDPI Set — TIFF delegate
// ---------------------------------------------------------------------------
/// NDPI Set format reader (`.ndpis`).
///
/// Ported from the Java `NDPISReader`. The `.ndpis` file is a small text index
/// listing one `.ndpi` file per channel:
///
/// ```text
/// NoImages=2
/// Image0=slide_ch0.ndpi
/// Image1=slide_ch1.ndpi
/// ```
///
/// Each `.ndpi` is a single-channel Hamamatsu TIFF. The pyramid resolutions are
/// merged so that `sizeC` equals the number of channel files; per-channel planes
/// are read from the matching delegate. Non-pyramid extra images (macro/label)
/// come from the first file only.
pub struct NdpisReader {
    /// One TiffReader delegate per channel `.ndpi` file.
    readers: Vec<crate::tiff::TiffReader>,
    ndpi_files: Vec<PathBuf>,
    /// Per-channel resolved channel name (from NDPI tag 65434), if present.
    channel_names: Vec<Option<String>>,
    metas: Vec<ImageMetadata>,
    current_series: usize,
}

const NDPI_TAG_CHANNEL: u16 = 65434;

impl NdpisReader {
    pub fn new() -> Self {
        NdpisReader {
            readers: Vec::new(),
            ndpi_files: Vec::new(),
            channel_names: Vec::new(),
            metas: Vec::new(),
            current_series: 0,
        }
    }
}

impl Default for NdpisReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for NdpisReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("ndpis"))
    }
    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }
    fn set_id(&mut self, path: &Path) -> Result<()> {
        let text = std::fs::read_to_string(path).map_err(BioFormatsError::Io)?;
        let parent = path.parent().map(Path::to_path_buf).unwrap_or_default();

        // Parse the index: NoImages=N and ImageK=relative_path lines.
        let mut files: Vec<PathBuf> = Vec::new();
        for line in text.split(['\r', '\n']) {
            let line = line.trim();
            let Some(eq) = line.find('=') else { continue };
            let key = line[..eq].trim();
            let value = line[eq + 1..].trim();
            if key == "NoImages" {
                let count = value.parse().unwrap_or(0);
                files = vec![PathBuf::new(); count];
            } else if let Some(idx) = key.strip_prefix("Image") {
                if let Ok(index) = idx.parse::<usize>() {
                    if index >= files.len() {
                        files.resize(index + 1, PathBuf::new());
                    }
                    files[index] = parent.join(value);
                }
            }
        }
        files.retain(|p| !p.as_os_str().is_empty());
        if files.is_empty() {
            return Err(BioFormatsError::Format(
                "NDPIS index references no .ndpi files".into(),
            ));
        }

        // Open each channel file as a TIFF delegate.
        let mut readers = Vec::with_capacity(files.len());
        let mut channel_names = Vec::with_capacity(files.len());
        for file in &files {
            let mut r = crate::tiff::TiffReader::new();
            r.set_id(file)?;
            // Channel name from NDPI tag 65434 on the first IFD.
            let name = r
                .ifd(0)
                .and_then(|ifd| ifd.get_str(NDPI_TAG_CHANNEL).map(str::to_owned));
            channel_names.push(name);
            readers.push(r);
        }

        // Build merged metadata from the first reader's series, setting sizeC to
        // the number of channel files and recomputing the plane count.
        let base = &readers[0];
        let mut metas: Vec<ImageMetadata> = Vec::new();
        for s in 0..base.series_count() {
            // We can't call set_series on an immutable borrow; collect by index.
            let m = base.series_list()[s].metadata.clone();
            metas.push(m);
        }
        let nchannels = files.len() as u32;
        // The pyramid resolutions are series whose dimensions shrink; the macro/
        // label images are extra. Following the Java reader, only the pyramid
        // resolutions get sizeC adjusted. We treat all base series as channel
        // stacks (sizeC == channel count) which matches single-resolution NDPI.
        for m in &mut metas {
            m.size_c = nchannels;
            m.is_rgb = false;
            m.image_count = m.size_c * m.size_z.max(1) * m.size_t.max(1);
        }

        self.readers = readers;
        self.ndpi_files = files;
        self.channel_names = channel_names;
        self.metas = metas;
        self.current_series = 0;
        Ok(())
    }
    fn close(&mut self) -> Result<()> {
        for r in &mut self.readers {
            let _ = r.close();
        }
        self.readers.clear();
        self.ndpi_files.clear();
        self.channel_names.clear();
        self.metas.clear();
        self.current_series = 0;
        Ok(())
    }
    fn series_count(&self) -> usize {
        self.metas.len()
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if s >= self.metas.len() {
            return Err(BioFormatsError::SeriesOutOfRange(s));
        }
        self.current_series = s;
        for r in &mut self.readers {
            let _ = r.set_series(s);
        }
        Ok(())
    }
    fn series(&self) -> usize {
        self.current_series
    }
    fn metadata(&self) -> &ImageMetadata {
        &self.metas[self.current_series]
    }
    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        // plane index p maps to a channel; each channel comes from one file.
        let nchannels = self.readers.len() as u32;
        let channel = (p % nchannels.max(1)) as usize;
        let inner_plane = p / nchannels.max(1);
        self.readers[channel].set_series(self.current_series)?;
        self.readers[channel].open_bytes(inner_plane)
    }
    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let nchannels = self.readers.len() as u32;
        let channel = (p % nchannels.max(1)) as usize;
        let inner_plane = p / nchannels.max(1);
        self.readers[channel].set_series(self.current_series)?;
        self.readers[channel].open_bytes_region(inner_plane, x, y, w, h)
    }
    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        let nchannels = self.readers.len() as u32;
        let channel = (p % nchannels.max(1)) as usize;
        self.readers[channel].set_series(self.current_series)?;
        self.readers[channel].open_thumb_bytes(0)
    }
    fn resolution_count(&self) -> usize {
        self.readers
            .first()
            .map(|r| r.resolution_count())
            .unwrap_or(1)
    }
    fn set_resolution(&mut self, level: usize) -> Result<()> {
        for r in &mut self.readers {
            r.set_resolution(level)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 5. iVision IPM
// ---------------------------------------------------------------------------
/// iVision format reader (`.ipm`).
pub struct IvisionReader {
    state: Option<IvisionState>,
}

struct IvisionNativeState {
    path: PathBuf,
    meta: ImageMetadata,
    ome: Option<crate::common::ome_metadata::OmeMetadata>,
    image_offset: u64,
    disk_plane_len: usize,
    output_plane_len: usize,
    has_padding_byte: bool,
    unsupported_pixel_read: Option<&'static str>,
}

enum IvisionState {
    Synthetic(SyntheticRawState),
    Native(IvisionNativeState),
}

impl IvisionReader {
    pub fn new() -> Self {
        Self { state: None }
    }

    fn spec() -> SyntheticRawSpec {
        SyntheticRawSpec {
            format_name: "iVision IPM",
            unsupported_message: "iVision IPM is a proprietary format from BioVision Technologies",
            extension: "ipm",
            magic: SYNTHETIC_IVISION_MAGIC,
        }
    }
}

impl Default for IvisionReader {
    fn default() -> Self {
        Self::new()
    }
}

fn parse_ivision_native(path: &Path) -> Result<IvisionNativeState> {
    let bytes = std::fs::read(path).map_err(BioFormatsError::Io)?;
    if !ivision_structural_header(&bytes) {
        return Err(BioFormatsError::UnsupportedFormat(
            "iVision IPM native header is not recognized".into(),
        ));
    }
    if bytes.len() < 72 {
        return Err(BioFormatsError::Format(
            "iVision IPM native header is truncated".into(),
        ));
    }

    let version = std::str::from_utf8(&bytes[..4])
        .unwrap_or("")
        .trim_end_matches('\0')
        .to_string();
    let file_format = bytes[4];
    let data_type = bytes[5];
    let size_x = u32::from_be_bytes(bytes[6..10].try_into().unwrap());
    let size_y = u32::from_be_bytes(bytes[10..14].try_into().unwrap());
    let size_z = u16::from_be_bytes(bytes[20..22].try_into().unwrap()) as u32;

    if size_x == 0 || size_y == 0 || size_z == 0 {
        return Err(BioFormatsError::Format(
            "iVision IPM native dimensions must be non-zero".into(),
        ));
    }

    let (
        pixel_type,
        size_c,
        has_padding_byte,
        disk_bytes_per_sample,
        disk_samples_per_pixel,
        unsupported_pixel_read,
        storage_layout,
    ) = match data_type {
        0 => (PixelType::Uint8, 1, false, 1, 1, None, "8-bit mono samples"),
        1 => (
            PixelType::Int16,
            1,
            false,
            2,
            1,
            None,
            "big-endian signed 16-bit mono samples",
        ),
        2 => (
            PixelType::Int32,
            1,
            false,
            4,
            1,
            None,
            "big-endian signed 32-bit mono samples",
        ),
        3 => (
            PixelType::Float32,
            1,
            false,
            4,
            1,
            None,
            "big-endian 32-bit float mono samples",
        ),
        4 => (
            PixelType::Uint8,
            3,
            false,
            2,
            1,
            Some("Packed 16-bit color iVision pixel decoding is not supported: data type 4 stores one 16-bit word per pixel, but the native header does not identify RGB555 vs RGB565 masks or channel bit order"),
            "packed 16-bit color samples with unresolved RGB555/RGB565 masks",
        ),
        5 => (
            PixelType::Uint8,
            3,
            true,
            1,
            3,
            None,
            "padded 8-bit RGB samples, one leading padding byte per pixel",
        ),
        6 => (
            PixelType::Uint16,
            1,
            false,
            2,
            1,
            None,
            "big-endian unsigned 16-bit mono samples",
        ),
        7 => (
            PixelType::Float32,
            1,
            false,
            2,
            1,
            Some("Square-root iVision pixel decoding is not supported: the Java reader declares float output but leaves the square-root transfer curve unimplemented"),
            "big-endian square-root encoded 16-bit samples with float output",
        ),
        8 => (
            PixelType::Uint16,
            3,
            false,
            2,
            3,
            None,
            "big-endian unsigned 16-bit RGB samples",
        ),
        other => {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "iVision IPM native data type {other} is unsupported"
            )));
        }
    };

    let image_offset = 72u64
        .checked_add(if size_x > 1 && size_y > 1 { 2048 } else { 0 })
        .ok_or_else(|| BioFormatsError::Format("iVision IPM image offset overflows".into()))?;
    if bytes.len() < image_offset as usize {
        return Err(BioFormatsError::Format(
            "iVision IPM native LUT/header is truncated".into(),
        ));
    }

    let output_samples = (size_x as usize)
        .checked_mul(size_y as usize)
        .and_then(|v| v.checked_mul(size_c as usize))
        .ok_or_else(|| {
            BioFormatsError::Format("iVision IPM plane sample count overflows".into())
        })?;
    let output_plane_len = output_samples
        .checked_mul(pixel_type.bytes_per_sample())
        .ok_or_else(|| BioFormatsError::Format("iVision IPM plane byte count overflows".into()))?;
    let disk_samples = (size_x as usize)
        .checked_mul(size_y as usize)
        .and_then(|v| v.checked_mul(disk_samples_per_pixel))
        .ok_or_else(|| {
            BioFormatsError::Format("iVision IPM disk plane sample count overflows".into())
        })?;
    let unpadded_disk_plane_len =
        disk_samples
            .checked_mul(disk_bytes_per_sample)
            .ok_or_else(|| {
                BioFormatsError::Format("iVision IPM disk plane byte count overflows".into())
            })?;
    let disk_plane_len = if has_padding_byte {
        unpadded_disk_plane_len
            .checked_add(
                (size_x as usize)
                    .checked_mul(size_y as usize)
                    .ok_or_else(|| {
                        BioFormatsError::Format("iVision IPM padding byte count overflows".into())
                    })?,
            )
            .ok_or_else(|| {
                BioFormatsError::Format("iVision IPM padded plane size overflows".into())
            })?
    } else {
        unpadded_disk_plane_len
    };
    let image_count = size_z;
    let expected_pixel_end = image_offset
        .checked_add(
            (disk_plane_len as u64)
                .checked_mul(image_count as u64)
                .ok_or_else(|| {
                    BioFormatsError::Format("iVision IPM payload size overflows".into())
                })?,
        )
        .ok_or_else(|| BioFormatsError::Format("iVision IPM payload end overflows".into()))?;
    if bytes.len() < expected_pixel_end as usize {
        return Err(BioFormatsError::InvalidData(format!(
            "iVision IPM native pixel payload is {}, expected at least {}",
            bytes.len().saturating_sub(image_offset as usize),
            expected_pixel_end - image_offset
        )));
    }

    let mut meta = ImageMetadata {
        size_x,
        size_y,
        size_z,
        size_c,
        size_t: 1,
        pixel_type,
        bits_per_pixel: (pixel_type.bytes_per_sample() * 8) as u8,
        image_count,
        is_little_endian: false,
        ..ImageMetadata::default()
    };
    meta.series_metadata.insert(
        "iVision Version".to_string(),
        crate::common::metadata::MetadataValue::String(version),
    );
    meta.series_metadata.insert(
        "iVision FileFormat".to_string(),
        crate::common::metadata::MetadataValue::Int(file_format as i64),
    );
    meta.series_metadata.insert(
        "iVision DataType".to_string(),
        crate::common::metadata::MetadataValue::Int(data_type as i64),
    );
    meta.series_metadata.insert(
        "iVision DataType Name".to_string(),
        crate::common::metadata::MetadataValue::String(ivision_data_type_name(data_type).into()),
    );
    meta.series_metadata.insert(
        "iVision Samples Per Pixel".to_string(),
        crate::common::metadata::MetadataValue::Int(size_c as i64),
    );
    meta.series_metadata.insert(
        "iVision Storage Layout".to_string(),
        crate::common::metadata::MetadataValue::String(storage_layout.into()),
    );
    meta.series_metadata.insert(
        "iVision Native Width".to_string(),
        crate::common::metadata::MetadataValue::Int(size_x as i64),
    );
    meta.series_metadata.insert(
        "iVision Native Height".to_string(),
        crate::common::metadata::MetadataValue::Int(size_y as i64),
    );
    meta.series_metadata.insert(
        "iVision Native Z Sections".to_string(),
        crate::common::metadata::MetadataValue::Int(size_z as i64),
    );
    meta.series_metadata.insert(
        "iVision Image Offset".to_string(),
        crate::common::metadata::MetadataValue::Int(image_offset as i64),
    );
    meta.series_metadata.insert(
        "iVision Disk Plane Bytes".to_string(),
        crate::common::metadata::MetadataValue::Int(disk_plane_len as i64),
    );
    meta.series_metadata.insert(
        "iVision Output Plane Bytes".to_string(),
        crate::common::metadata::MetadataValue::Int(output_plane_len as i64),
    );
    meta.series_metadata.insert(
        "iVision Has Padding Byte".to_string(),
        crate::common::metadata::MetadataValue::Bool(has_padding_byte),
    );

    let ome = ivision_apply_xml_metadata(path, &bytes, expected_pixel_end as usize, &mut meta);

    Ok(IvisionNativeState {
        path: path.to_path_buf(),
        meta,
        ome,
        image_offset,
        disk_plane_len,
        output_plane_len,
        has_padding_byte,
        unsupported_pixel_read,
    })
}

fn ivision_data_type_name(data_type: u8) -> &'static str {
    match data_type {
        0 => "8-bit mono",
        1 => "16-bit signed mono",
        2 => "32-bit signed mono",
        3 => "32-bit float mono",
        4 => "16-bit color",
        5 => "8-bit color with padding",
        6 => "16-bit unsigned mono",
        7 => "square-root float",
        8 => "16-bit unsigned color",
        _ => "unknown",
    }
}

fn ivision_apply_xml_metadata(
    path: &Path,
    bytes: &[u8],
    pixel_end: usize,
    meta: &mut ImageMetadata,
) -> Option<crate::common::ome_metadata::OmeMetadata> {
    let mut xml_source = None;
    let mut xml = None;

    if let Some(tail) = bytes.get(pixel_end..) {
        if let Some(found) = ivision_xml_from_bytes(tail) {
            xml_source = Some("embedded_tail");
            xml = Some(found);
        }
    }

    if xml.is_none() {
        let sidecar = path.with_extension("xml");
        if let Ok(sidecar_bytes) = std::fs::read(&sidecar) {
            if let Some(found) = ivision_xml_from_bytes(&sidecar_bytes) {
                xml_source = Some("sidecar");
                xml = Some(found);
            }
        }
    }

    let xml = xml?;

    meta.series_metadata
        .insert("iVision XML Metadata".into(), MetadataValue::Bool(true));
    if let Some(source) = xml_source {
        meta.series_metadata.insert(
            "iVision XML Source".into(),
            MetadataValue::String(source.into()),
        );
    }

    let flattened = ivision_flatten_xml_metadata(&xml, meta);
    if flattened > 0 {
        meta.series_metadata.insert(
            "iVision XML Flattened Fields".into(),
            MetadataValue::Int(flattened as i64),
        );
    }

    let mut ome = crate::common::ome_metadata::OmeMetadata::from_ome_xml(&xml);
    let _ = ome.populate_pixels(meta, 0);
    let image = ome.images.first()?;

    if let Some(name) = image.name.as_ref().filter(|v| !v.trim().is_empty()) {
        meta.series_metadata.insert(
            "iVision XML Image Name".into(),
            MetadataValue::String(name.clone()),
        );
    }
    if let Some(value) = image.physical_size_x.filter(|v| v.is_finite()) {
        meta.series_metadata.insert(
            "iVision XML PhysicalSizeX".into(),
            MetadataValue::Float(value),
        );
    }
    if let Some(value) = image.physical_size_y.filter(|v| v.is_finite()) {
        meta.series_metadata.insert(
            "iVision XML PhysicalSizeY".into(),
            MetadataValue::Float(value),
        );
    }
    if let Some(value) = image.physical_size_z.filter(|v| v.is_finite()) {
        meta.series_metadata.insert(
            "iVision XML PhysicalSizeZ".into(),
            MetadataValue::Float(value),
        );
    }
    if let Some(value) = image.time_increment.filter(|v| v.is_finite()) {
        meta.series_metadata.insert(
            "iVision XML TimeIncrement".into(),
            MetadataValue::Float(value),
        );
    }

    for (index, channel) in image.channels.iter().enumerate().take(meta.size_c as usize) {
        let prefix = format!("iVision XML Channel {index}");
        if let Some(name) = channel.name.as_ref().filter(|v| !v.trim().is_empty()) {
            meta.series_metadata.insert(
                format!("{prefix} Name"),
                MetadataValue::String(name.clone()),
            );
        }
        if let Some(value) = channel.excitation_wavelength.filter(|v| v.is_finite()) {
            meta.series_metadata.insert(
                format!("{prefix} ExcitationWavelength"),
                MetadataValue::Float(value),
            );
        }
        if let Some(value) = channel.emission_wavelength.filter(|v| v.is_finite()) {
            meta.series_metadata.insert(
                format!("{prefix} EmissionWavelength"),
                MetadataValue::Float(value),
            );
        }
        if let Some(value) = channel.color {
            meta.series_metadata
                .insert(format!("{prefix} Color"), MetadataValue::Int(value as i64));
        }
    }

    Some(ome)
}

fn ivision_flatten_xml_metadata(xml: &str, meta: &mut ImageMetadata) -> usize {
    const MAX_FIELDS: usize = 128;
    const MAX_VALUE_LEN: usize = 512;

    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut stack: Vec<String> = Vec::new();
    let mut inserted = 0usize;

    loop {
        match reader.read_event() {
            Ok(quick_xml::events::Event::Start(element)) => {
                stack.push(ivision_xml_component_name(element.name().as_ref()));
                inserted += ivision_flatten_xml_attrs(
                    &reader,
                    &stack,
                    element.attributes(),
                    meta,
                    MAX_FIELDS.saturating_sub(inserted),
                    MAX_VALUE_LEN,
                );
            }
            Ok(quick_xml::events::Event::Empty(element)) => {
                stack.push(ivision_xml_component_name(element.name().as_ref()));
                inserted += ivision_flatten_xml_attrs(
                    &reader,
                    &stack,
                    element.attributes(),
                    meta,
                    MAX_FIELDS.saturating_sub(inserted),
                    MAX_VALUE_LEN,
                );
                stack.pop();
            }
            Ok(quick_xml::events::Event::Text(text)) => {
                if inserted >= MAX_FIELDS {
                    continue;
                }
                if let Ok(value) = text.unescape() {
                    let value = value.trim();
                    if !value.is_empty() && value.len() <= MAX_VALUE_LEN {
                        let key = ivision_flatten_xml_key(&stack, None);
                        ivision_insert_flattened_xml_value(meta, key, value);
                        inserted += 1;
                    }
                }
            }
            Ok(quick_xml::events::Event::End(_)) => {
                stack.pop();
            }
            Ok(quick_xml::events::Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        if inserted >= MAX_FIELDS {
            break;
        }
    }

    inserted
}

fn ivision_flatten_xml_attrs<'a>(
    reader: &quick_xml::Reader<&[u8]>,
    stack: &[String],
    attrs: quick_xml::events::attributes::Attributes<'a>,
    meta: &mut ImageMetadata,
    remaining: usize,
    max_value_len: usize,
) -> usize {
    let mut inserted = 0usize;
    for attr in attrs.flatten() {
        if inserted >= remaining {
            break;
        }
        let name = ivision_xml_component_name(attr.key.as_ref());
        if name.is_empty() {
            continue;
        }
        let Ok(value) = attr.decode_and_unescape_value(reader.decoder()) else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() || value.len() > max_value_len {
            continue;
        }
        let key = ivision_flatten_xml_key(stack, Some(&name));
        ivision_insert_flattened_xml_value(meta, key, value);
        inserted += 1;
    }
    inserted
}

fn ivision_xml_component_name(name: &[u8]) -> String {
    let local = name.split(|byte| *byte == b':').next_back().unwrap_or(name);
    String::from_utf8_lossy(local)
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .collect()
}

fn ivision_flatten_xml_key(stack: &[String], attr: Option<&str>) -> String {
    let mut key = String::from("iVision XML");
    for part in stack.iter().filter(|part| !part.is_empty()) {
        key.push(' ');
        key.push_str(part);
    }
    if let Some(attr) = attr.filter(|value| !value.is_empty()) {
        key.push(' ');
        key.push_str(attr);
    }
    key
}

fn ivision_insert_flattened_xml_value(meta: &mut ImageMetadata, key: String, value: &str) {
    let key = ivision_unique_metadata_key(&meta.series_metadata, key);
    let value = if value.eq_ignore_ascii_case("true") {
        MetadataValue::Bool(true)
    } else if value.eq_ignore_ascii_case("false") {
        MetadataValue::Bool(false)
    } else if let Ok(parsed) = value.parse::<i64>() {
        MetadataValue::Int(parsed)
    } else if let Ok(parsed) = value.parse::<f64>() {
        if parsed.is_finite() {
            MetadataValue::Float(parsed)
        } else {
            MetadataValue::String(value.to_string())
        }
    } else {
        MetadataValue::String(value.to_string())
    };
    meta.series_metadata.insert(key, value);
}

fn ivision_unique_metadata_key(existing: &HashMap<String, MetadataValue>, key: String) -> String {
    if !existing.contains_key(&key) {
        return key;
    }
    for index in 2.. {
        let candidate = format!("{key} {index}");
        if !existing.contains_key(&candidate) {
            return candidate;
        }
    }
    unreachable!()
}

fn ivision_xml_from_bytes(bytes: &[u8]) -> Option<String> {
    let start = find_subslice(bytes, b"<?xml")
        .or_else(|| find_subslice(bytes, b"<OME"))
        .or_else(|| find_subslice(bytes, b"<Image"))?;
    let text = std::str::from_utf8(&bytes[start..])
        .ok()?
        .trim_matches('\0')
        .trim();
    if text.starts_with("<?xml") || text.contains("<Image") || text.contains("<OME") {
        Some(text.to_string())
    } else {
        None
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn ivision_native_open_bytes(state: &IvisionNativeState, plane_index: u32) -> Result<Vec<u8>> {
    if plane_index >= state.meta.image_count {
        return Err(BioFormatsError::PlaneOutOfRange(plane_index));
    }
    if let Some(message) = state.unsupported_pixel_read {
        return Err(BioFormatsError::UnsupportedFormat(message.into()));
    }
    let offset = state
        .image_offset
        .checked_add(
            (plane_index as u64)
                .checked_mul(state.disk_plane_len as u64)
                .ok_or_else(|| {
                    BioFormatsError::Format("iVision IPM plane offset overflows".into())
                })?,
        )
        .ok_or_else(|| BioFormatsError::Format("iVision IPM plane offset overflows".into()))?;
    let mut reader = BufReader::new(File::open(&state.path).map_err(BioFormatsError::Io)?);
    let disk = read_bytes_at(&mut reader, offset, state.disk_plane_len)?;
    if !state.has_padding_byte {
        return Ok(disk);
    }

    let mut out = Vec::with_capacity(state.output_plane_len);
    let channels = state.meta.size_c as usize;
    for px in disk.chunks_exact(channels + 1) {
        out.extend_from_slice(&px[1..]);
    }
    if out.len() != state.output_plane_len {
        return Err(BioFormatsError::InvalidData(
            "iVision IPM padded plane did not decode to the expected length".into(),
        ));
    }
    Ok(out)
}

impl FormatReader for IvisionReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        Self::spec().matches_name(path)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        Self::spec().matches_bytes(header) || ivision_native_header(header)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.state = Some(match parse_synthetic_raw(path, Self::spec()) {
            Ok(state) => IvisionState::Synthetic(state),
            Err(BioFormatsError::UnsupportedFormat(_)) => {
                IvisionState::Native(parse_ivision_native(path)?)
            }
            Err(err) => return Err(err),
        });
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.state = None;
        Ok(())
    }

    fn series_count(&self) -> usize {
        usize::from(self.state.is_some())
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.state.is_none() {
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
        self.state
            .as_ref()
            .map(|state| match state {
                IvisionState::Synthetic(state) => &state.meta,
                IvisionState::Native(state) => &state.meta,
            })
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let state = self.state.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        match state {
            IvisionState::Synthetic(state) => {
                synthetic_raw_open_bytes(state, Self::spec(), plane_index)
            }
            IvisionState::Native(state) => ivision_native_open_bytes(state, plane_index),
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
        let state = self.state.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        match state {
            IvisionState::Synthetic(state) => {
                synthetic_raw_open_bytes_region(state, Self::spec(), plane_index, x, y, w, h)
            }
            IvisionState::Native(state) => {
                let full = ivision_native_open_bytes(state, plane_index)?;
                crop_full_plane(
                    "iVision IPM",
                    &full,
                    &state.meta,
                    state.meta.size_c as usize,
                    x,
                    y,
                    w,
                    h,
                )
            }
        }
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.metadata();
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        match self.state.as_ref()? {
            IvisionState::Synthetic(state) => {
                Some(crate::common::ome_metadata::OmeMetadata::from_image_metadata(&state.meta))
            }
            IvisionState::Native(state) => state.ome.clone().or_else(|| {
                Some(crate::common::ome_metadata::OmeMetadata::from_image_metadata(&state.meta))
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// 6. Aperio AFI — TIFF delegate
// ---------------------------------------------------------------------------
/// Aperio AFI fluorescence format reader (`.afi`).
///
/// Ported from the Java `AFIReader`. The `.afi` file is simple XML listing one
/// `.svs` file per channel:
///
/// ```xml
/// <ImageList>
///   <Image><Path>slide_DAPI.svs</Path></Image>
///   <Image><Path>slide_FITC.svs</Path></Image>
/// </ImageList>
/// ```
///
/// Each `.svs` corresponds to a single channel. Channel names are derived from
/// the filename substring between `_` and `.` (matching Java). The channels are
/// assembled into a single multi-channel series (the trailing label/macro
/// resolutions are taken from the first file).
pub struct AfiFluorescenceReader {
    readers: Vec<crate::formats::svs::WholeSlideTiffReader>,
    channel_names: Vec<Option<String>>,
    metas: Vec<ImageMetadata>,
    current_series: usize,
}

impl AfiFluorescenceReader {
    pub fn new() -> Self {
        AfiFluorescenceReader {
            readers: Vec::new(),
            channel_names: Vec::new(),
            metas: Vec::new(),
            current_series: 0,
        }
    }

    /// Extract `<Path>...</Path>` entries from the AFI XML.
    fn parse_paths(xml: &str) -> Vec<String> {
        let mut paths = Vec::new();
        let mut rest = xml;
        while let Some(start) = rest.find("<Path") {
            let after = &rest[start..];
            let Some(gt) = after.find('>') else { break };
            let body = &after[gt + 1..];
            let Some(end) = body.find('<') else { break };
            let value = body[..end].trim();
            if !value.is_empty() {
                paths.push(value.to_string());
            }
            rest = &body[end..];
        }
        paths
    }
}

impl Default for AfiFluorescenceReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for AfiFluorescenceReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("afi"))
    }
    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }
    fn set_id(&mut self, path: &Path) -> Result<()> {
        let xml = std::fs::read_to_string(path).map_err(BioFormatsError::Io)?;
        let parent = path.parent().map(Path::to_path_buf).unwrap_or_default();
        let rel_paths = Self::parse_paths(&xml);
        if rel_paths.is_empty() {
            return Err(BioFormatsError::Format(
                "AFI file lists no <Path> channel images".into(),
            ));
        }

        let mut readers = Vec::with_capacity(rel_paths.len());
        let mut channel_names = Vec::with_capacity(rel_paths.len());
        for rel in &rel_paths {
            // Channel name = substring between '_' and '.' of the file name.
            let name = {
                let underscore = rel.find('_');
                let dot = rel.find('.');
                match (underscore, dot) {
                    (Some(u), Some(d)) if d > u => Some(rel[u + 1..d].to_string()),
                    _ => None,
                }
            };
            channel_names.push(name);

            let full = parent.join(rel);
            let mut r = crate::formats::svs::WholeSlideTiffReader::new();
            r.set_id(&full)?;
            readers.push(r);
        }

        // Build metadata: clone the first reader's per-series metadata and set
        // sizeC to the number of channels for the non-extra (pyramid) series.
        let mut metas: Vec<ImageMetadata> = Vec::new();
        for s in 0..readers[0].series_count() {
            readers[0].set_series(s)?;
            metas.push(readers[0].metadata().clone());
        }
        readers[0].set_series(0)?;

        let nchannels = readers.len() as u32;
        // EXTRA_IMAGES = 2 (label + macro) are single-channel; the rest are
        // the multi-channel pyramid resolutions.
        let total = metas.len();
        let extra = 2usize.min(total);
        for (i, m) in metas.iter_mut().enumerate() {
            if i + extra < total {
                m.size_c = nchannels;
                m.is_rgb = false;
                m.image_count = m.size_c * m.size_z.max(1) * m.size_t.max(1);
            }
        }

        self.readers = readers;
        self.channel_names = channel_names;
        self.metas = metas;
        self.current_series = 0;
        Ok(())
    }
    fn close(&mut self) -> Result<()> {
        for r in &mut self.readers {
            let _ = r.close();
        }
        self.readers.clear();
        self.channel_names.clear();
        self.metas.clear();
        self.current_series = 0;
        Ok(())
    }
    fn series_count(&self) -> usize {
        self.metas.len()
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if s >= self.metas.len() {
            return Err(BioFormatsError::SeriesOutOfRange(s));
        }
        self.current_series = s;
        for r in &mut self.readers {
            let _ = r.set_series(s);
        }
        Ok(())
    }
    fn series(&self) -> usize {
        self.current_series
    }
    fn metadata(&self) -> &ImageMetadata {
        &self.metas[self.current_series]
    }
    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        let m = &self.metas[self.current_series];
        let extra = 2usize.min(self.metas.len());
        // Extra (label/macro) series: read straight from the first file.
        if self.current_series + extra >= self.metas.len() {
            self.readers[0].set_series(self.current_series)?;
            return self.readers[0].open_bytes(p);
        }
        let nchannels = self.readers.len() as u32;
        let channel = (p % nchannels.max(1)) as usize;
        let inner_plane = p / nchannels.max(1);
        let _ = m;
        self.readers[channel].set_series(self.current_series)?;
        self.readers[channel].open_bytes(inner_plane)
    }
    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let extra = 2usize.min(self.metas.len());
        if self.current_series + extra >= self.metas.len() {
            self.readers[0].set_series(self.current_series)?;
            return self.readers[0].open_bytes_region(p, x, y, w, h);
        }
        let nchannels = self.readers.len() as u32;
        let channel = (p % nchannels.max(1)) as usize;
        let inner_plane = p / nchannels.max(1);
        self.readers[channel].set_series(self.current_series)?;
        self.readers[channel].open_bytes_region(inner_plane, x, y, w, h)
    }
    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        self.readers[0].set_series(self.current_series)?;
        self.readers[0].open_thumb_bytes(p)
    }
    fn resolution_count(&self) -> usize {
        self.readers
            .first()
            .map(|r| r.resolution_count())
            .unwrap_or(1)
    }
    fn set_resolution(&mut self, level: usize) -> Result<()> {
        for r in &mut self.readers {
            r.set_resolution(level)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 7. Imaris TIFF — TIFF delegate
// ---------------------------------------------------------------------------
/// Imaris TIFF format reader (`.ims`).
///
/// Ported from the Java `ImarisTiffReader`. Bitplane Imaris 3 TIFFs store a
/// thumbnail in the first IFD and one IFD per channel; each IFD holds a stack
/// of tiled Z planes. The first IFD's ImageDescription carries an INI-style
/// comment with `Description`, `Name` (channel names), `LSMEmissionWavelength`,
/// `LSMExcitationWavelength`, and `RecordingDate`.
///
/// We port the comment parsing and dimension assignment (`sizeC` = number of
/// IFDs). The per-IFD strip→Z-plane reshape that the Java reader performs is
/// not yet replicated; pixel reads are delegated to `TiffReader` as-is, so
/// per-channel planes are exposed at IFD granularity.
pub struct ImarisTiffReader {
    inner: crate::tiff::TiffReader,
}

impl ImarisTiffReader {
    pub fn new() -> Self {
        ImarisTiffReader {
            inner: crate::tiff::TiffReader::new(),
        }
    }

    fn enrich_metadata(&mut self) {
        use crate::common::metadata::MetadataValue;
        let comment = self.inner.ifd(0).and_then(|ifd| {
            ifd.get_str(crate::tiff::ifd::tag::IMAGE_DESCRIPTION)
                .map(str::to_owned)
        });
        let Some(comment) = comment else { return };
        if !comment.starts_with('[') {
            return;
        }

        let mut description: Option<String> = None;
        let mut creation_date: Option<String> = None;
        let mut channel_names: Vec<String> = Vec::new();
        let mut em_wave: Vec<f64> = Vec::new();
        let mut ex_wave: Vec<f64> = Vec::new();

        for line in comment.split('\n') {
            let Some(eq) = line.find('=') else { continue };
            let key = line[..eq].trim();
            let value = line[eq + 1..].trim();
            match key {
                "Description" => description = Some(value.to_string()),
                "LSMEmissionWavelength" if value != "0" => {
                    if let Ok(v) = value.parse::<f64>() {
                        em_wave.push(v);
                    }
                }
                "LSMExcitationWavelength" if value != "0" => {
                    if let Ok(v) = value.parse::<f64>() {
                        ex_wave.push(v);
                    }
                }
                "Name" => channel_names.push(value.to_string()),
                "RecordingDate" => {
                    let v = value.replace(' ', "T");
                    let trimmed = v.split('.').next().unwrap_or(&v).to_string();
                    creation_date = Some(trimmed);
                }
                _ => {}
            }
        }

        let ifd_count = self.inner.ifd_count() as u32;
        if let Some(s) = self.inner.series_list_mut().first_mut() {
            // sizeC equals the number of IFDs (channels), per Java.
            if ifd_count > 0 {
                s.metadata.size_c = ifd_count;
                s.metadata.is_rgb = false;
            }
            if let Some(d) = description {
                s.metadata
                    .series_metadata
                    .insert("imaris.description".into(), MetadataValue::String(d));
            }
            if let Some(cd) = creation_date {
                s.metadata
                    .series_metadata
                    .insert("imaris.recording_date".into(), MetadataValue::String(cd));
            }
            for (i, name) in channel_names.iter().enumerate() {
                s.metadata.series_metadata.insert(
                    format!("imaris.channel.{}.name", i),
                    MetadataValue::String(name.clone()),
                );
            }
            for (i, em) in em_wave.iter().enumerate() {
                s.metadata.series_metadata.insert(
                    format!("imaris.channel.{}.emission", i),
                    MetadataValue::Float(*em),
                );
            }
            for (i, ex) in ex_wave.iter().enumerate() {
                s.metadata.series_metadata.insert(
                    format!("imaris.channel.{}.excitation", i),
                    MetadataValue::Float(*ex),
                );
            }
        }
    }
}

impl Default for ImarisTiffReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for ImarisTiffReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("ims"))
    }
    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }
    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.inner.set_id(path)?;
        self.enrich_metadata();
        Ok(())
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
    fn set_resolution(&mut self, level: usize) -> Result<()> {
        self.inner.set_resolution(level)
    }
}

// ---------------------------------------------------------------------------
// 8. Leica XLEF — image delegate
// ---------------------------------------------------------------------------
/// Leica XLEF format reader (`.xlef`).
///
/// XLEF files are Leica XML projects. Java resolves referenced XLIF/LOF or
/// raster files through `LMSFileReader`; this bounded port follows local
/// XLEF/XLIF graph references and exposes TIFF/LOF/raster leaves as project
/// series.
pub struct XlefReader {
    delegates: Vec<XlefDelegate>,
    lms_metadata: Vec<ImageMetadata>,
    series_map: Vec<XlefSeriesRef>,
    project_metadata: Vec<ImageMetadata>,
    current_series: usize,
}

struct XlefDelegate {
    reader: Box<dyn FormatReader>,
    path: PathBuf,
}

#[derive(Clone, Copy)]
enum XlefSeriesRef {
    Delegate { delegate: usize, series: usize },
    Lms { metadata: usize },
}

impl XlefReader {
    pub fn new() -> Self {
        XlefReader {
            delegates: Vec::new(),
            lms_metadata: Vec::new(),
            series_map: Vec::new(),
            project_metadata: Vec::new(),
            current_series: 0,
        }
    }

    fn referenced_images(path: &Path) -> Result<Vec<XlefReference>> {
        let mut visited = HashSet::new();
        let mut unsupported = Vec::new();
        let refs = xlef_collect_referenced_images(path, &mut visited, &mut unsupported)?;
        if refs.is_empty() {
            if unsupported.is_empty() {
                return Err(BioFormatsError::UnsupportedFormat(
                    "Leica XLEF project contains no supported local image or LMS metadata references"
                        .into(),
                ));
            }
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "Leica XLEF project references unsupported files {}; only local TIFF, LOF, JPEG, PNG, BMP, and bounded LMS metadata leaves are currently handled",
                xlef_format_paths(&unsupported)
            )));
        }
        if !unsupported.is_empty() {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "Leica XLEF project mixes supported leaves with unsupported files {}; partial mixed-project opening is not implemented",
                xlef_format_paths(&unsupported)
            )));
        }
        Ok(refs)
    }

    fn current_delegate_mut(&mut self) -> Result<&mut (dyn FormatReader + '_)> {
        match *self
            .series_map
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?
        {
            XlefSeriesRef::Delegate { delegate, .. } => {
                if let Some(delegate) = self.delegates.get_mut(delegate) {
                    Ok(delegate.reader.as_mut())
                } else {
                    Err(BioFormatsError::NotInitialized)
                }
            }
            XlefSeriesRef::Lms { .. } => Err(BioFormatsError::UnsupportedFormat(
                "Leica XLEF LMS metadata series has no pixel delegate yet".into(),
            )),
        }
    }

    fn current_delegate(&self) -> Option<&dyn FormatReader> {
        match self.series_map.get(self.current_series)? {
            XlefSeriesRef::Delegate { delegate, .. } => self
                .delegates
                .get(*delegate)
                .map(|delegate| delegate.reader.as_ref()),
            XlefSeriesRef::Lms { .. } => None,
        }
    }

    fn current_lms_metadata(&self) -> Option<&ImageMetadata> {
        match self.series_map.get(self.current_series)? {
            XlefSeriesRef::Delegate { .. } => None,
            XlefSeriesRef::Lms { metadata } => self.lms_metadata.get(*metadata),
        }
    }

    fn add_delegate(&mut self, reference: &Path, mut reader: Box<dyn FormatReader>) -> Result<()> {
        reader.set_id(reference)?;
        self.add_initialized_delegate(reference, reader)
    }

    fn add_initialized_delegate(
        &mut self,
        reference: &Path,
        reader: Box<dyn FormatReader>,
    ) -> Result<()> {
        let delegate_index = self.delegates.len();
        let series_count = reader.series_count();
        if series_count == 0 {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "Leica XLEF referenced image {} exposes no readable series",
                reference.display()
            )));
        }
        for series in 0..series_count {
            self.series_map.push(XlefSeriesRef::Delegate {
                delegate: delegate_index,
                series,
            });
        }
        self.delegates.push(XlefDelegate {
            reader,
            path: reference.to_path_buf(),
        });
        Ok(())
    }

    fn rebuild_project_metadata(&mut self, project_path: &Path) -> Result<()> {
        let series_count = self.series_map.len();
        let mut metadata = Vec::with_capacity(series_count);

        for series_index in 0..series_count {
            let mapping = self.series_map[series_index];
            let (mut meta, source_path, source_kind) = match mapping {
                XlefSeriesRef::Delegate { delegate, series } => {
                    let delegate = self
                        .delegates
                        .get_mut(delegate)
                        .ok_or(BioFormatsError::NotInitialized)?;
                    delegate.reader.set_series(series)?;
                    (
                        delegate.reader.metadata().clone(),
                        delegate.path.display().to_string(),
                        "pixel_delegate",
                    )
                }
                XlefSeriesRef::Lms { metadata } => {
                    let meta = self
                        .lms_metadata
                        .get(metadata)
                        .ok_or(BioFormatsError::NotInitialized)?
                        .clone();
                    let source_path =
                        xlef_lms_metadata_string(&meta, "xlef.lms.path").unwrap_or_default();
                    (meta, source_path, "lms_metadata")
                }
            };

            meta.series_metadata.insert(
                "xlef.project.path".into(),
                MetadataValue::String(project_path.display().to_string()),
            );
            if let Some(name) = project_path.file_name().and_then(|name| name.to_str()) {
                meta.series_metadata.insert(
                    "xlef.project.name".into(),
                    MetadataValue::String(name.to_string()),
                );
            }
            meta.series_metadata.insert(
                "xlef.project.series_index".into(),
                MetadataValue::Int(series_index as i64),
            );
            meta.series_metadata.insert(
                "xlef.project.series_count".into(),
                MetadataValue::Int(series_count as i64),
            );
            meta.series_metadata.insert(
                "xlef.project.source_path".into(),
                MetadataValue::String(source_path),
            );
            meta.series_metadata.insert(
                "xlef.project.source_kind".into(),
                MetadataValue::String(source_kind.into()),
            );
            metadata.push(meta);
        }

        self.project_metadata = metadata;
        Ok(())
    }

    fn set_delegate_series_for_current(&mut self) -> Result<()> {
        if let Some(XlefSeriesRef::Delegate { delegate, series }) =
            self.series_map.get(self.current_series).copied()
        {
            self.delegates[delegate].reader.set_series(series)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum XlefReference {
    Image(PathBuf),
    Lms(PathBuf),
}

fn xlef_collect_referenced_images(
    path: &Path,
    visited: &mut HashSet<PathBuf>,
    unsupported: &mut Vec<PathBuf>,
) -> Result<Vec<XlefReference>> {
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if !visited.insert(canonical) {
        return Ok(Vec::new());
    }

    let xml = std::fs::read_to_string(path).map_err(BioFormatsError::Io)?;
    let mut images: Vec<XlefReference> = Vec::new();
    for reference in xlef_referenced_paths(&xml, path) {
        if xlef_is_project_reference(&reference) {
            if reference.exists() {
                for image in xlef_collect_referenced_images(&reference, visited, unsupported)? {
                    if !images.iter().any(|p| p == &image) {
                        images.push(image);
                    }
                }
            } else {
                unsupported.push(reference);
            }
        } else if xlef_is_supported_image_reference(&reference) {
            let image = XlefReference::Image(reference);
            if !images.iter().any(|p| p == &image) {
                images.push(image);
            }
        } else if xlef_is_lms_reference(&reference) {
            let lms = XlefReference::Lms(reference);
            if !images.iter().any(|p| p == &lms) {
                images.push(lms);
            }
        } else if !unsupported.iter().any(|p| p == &reference) {
            unsupported.push(reference);
        }
    }
    Ok(images)
}

fn xlef_referenced_paths(xml: &str, xlef_path: &Path) -> Vec<PathBuf> {
    let parent = xlef_path.parent().unwrap_or_else(|| Path::new(""));
    let mut refs = Vec::new();
    for (_name, attrs) in scn_scan_tags(xml) {
        for (key, value) in attrs {
            if !xlef_is_reference_attribute(&key) {
                continue;
            }
            if let Some(path) = xlef_reference_path(parent, &value) {
                if !refs.iter().any(|p| p == &path) {
                    refs.push(path);
                }
            }
        }
    }
    for token in xml.split(['"', '\'', '<', '>', '\n', '\r', '\t', ' ']) {
        let lower = token.to_ascii_lowercase();
        if !(lower.ends_with(".tif")
            || lower.ends_with(".tiff")
            || lower.ends_with(".lof")
            || lower.ends_with(".xlef")
            || lower.ends_with(".xlif")
            || lower.ends_with(".lms")
            || lower.ends_with(".bmp")
            || lower.ends_with(".jpg")
            || lower.ends_with(".jpeg")
            || lower.ends_with(".png"))
        {
            continue;
        }
        let cleaned = token.replace('\\', "/");
        let candidate = Path::new(&cleaned);
        let path = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            parent.join(candidate)
        };
        if !refs.iter().any(|p| p == &path) {
            refs.push(path);
        }
    }
    refs
}

fn xlef_is_reference_attribute(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "file" | "filename" | "filepath" | "path" | "relativepath" | "href" | "url" | "source"
    )
}

fn xlef_reference_path(parent: &Path, value: &str) -> Option<PathBuf> {
    let cleaned = value.trim().replace('\\', "/");
    if cleaned.is_empty() || cleaned.starts_with('#') {
        return None;
    }
    let lower = cleaned.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return Some(PathBuf::from(cleaned));
    }
    let candidate = Path::new(&cleaned);
    if candidate.extension().is_none() {
        return None;
    }
    Some(if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        parent.join(candidate)
    })
}

fn xlef_is_project_reference(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some("xlef") | Some("xlif")
    )
}

fn xlef_is_supported_image_reference(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some("tif")
            | Some("tiff")
            | Some("lof")
            | Some("jpg")
            | Some("jpeg")
            | Some("png")
            | Some("bmp")
    )
}

fn xlef_is_lms_reference(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some("lms")
    )
}

fn xlef_delegate_for_reference(reference: &Path) -> Box<dyn FormatReader> {
    match reference
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("lof") => Box::new(crate::formats::extended::LeicaLofReader::new()),
        Some("jpg") | Some("jpeg") => Box::new(crate::formats::jpeg::JpegReader::new()),
        Some("png") => Box::new(crate::formats::png::PngReader::new()),
        Some("bmp") => Box::new(crate::formats::bmp::BmpReader::new()),
        _ => Box::new(crate::tiff::TiffReader::new()),
    }
}

fn xlef_lms_delegate_for_reference(reference: &Path) -> Result<Option<Box<dyn FormatReader>>> {
    let mut reader: Box<dyn FormatReader> = Box::new(crate::formats::sem::ZeissLmsReader::new());
    match reader.set_id(reference) {
        Ok(()) => Ok(Some(reader)),
        Err(BioFormatsError::Io(err)) => Err(BioFormatsError::Io(err)),
        Err(_) => Ok(None),
    }
}

fn xlef_lms_metadata_for_reference(reference: &Path) -> Result<ImageMetadata> {
    let data = std::fs::read(reference).map_err(BioFormatsError::Io)?;
    let xml = xlef_decode_lms_text(&data)?;
    let tags = scn_scan_tags(&xml);
    let mut meta = ImageMetadata::default();
    meta.dimension_order = crate::common::metadata::DimensionOrder::XYZCT;
    let mut channel_count_from_descriptions = 0u32;
    let mut size_c_from_dimension = false;
    meta.series_metadata.insert(
        "xlef.lms.path".into(),
        crate::common::metadata::MetadataValue::String(reference.display().to_string()),
    );
    meta.series_metadata.insert(
        "xlef.lms.pixel_payload".into(),
        crate::common::metadata::MetadataValue::String("unsupported".into()),
    );

    for (name, attrs) in &tags {
        if name.eq_ignore_ascii_case("Element") {
            if let Some(value) = attrs.get("Name").filter(|v| !v.is_empty()) {
                meta.series_metadata.insert(
                    "xlef.lms.element.name".into(),
                    crate::common::metadata::MetadataValue::String(value.clone()),
                );
            }
            if let Some(value) = xlef_lms_description_attr(attrs) {
                meta.series_metadata
                    .entry("xlef.lms.description".into())
                    .or_insert_with(|| crate::common::metadata::MetadataValue::String(value));
            }
        } else if name.eq_ignore_ascii_case("Image") {
            for key in ["Name", "File", "ID", "UUID"] {
                if let Some(value) = attrs.get(key).filter(|v| !v.is_empty()) {
                    meta.series_metadata.insert(
                        format!("xlef.lms.image.{}", key.to_ascii_lowercase()),
                        crate::common::metadata::MetadataValue::String(value.clone()),
                    );
                }
            }
            if let Some(value) = xlef_lms_description_attr(attrs) {
                meta.series_metadata.insert(
                    "xlef.lms.description".into(),
                    crate::common::metadata::MetadataValue::String(value),
                );
            }
        } else if name.eq_ignore_ascii_case("ImageDescription") {
            if let Some(value) = xlef_lms_description_attr(attrs) {
                meta.series_metadata
                    .entry("xlef.lms.description".into())
                    .or_insert_with(|| crate::common::metadata::MetadataValue::String(value));
            }
        } else if name.eq_ignore_ascii_case("ChannelDescription") {
            let channel_index = channel_count_from_descriptions;
            channel_count_from_descriptions = channel_count_from_descriptions.saturating_add(1);
            xlef_lms_insert_channel_metadata(&mut meta, channel_index, attrs);
            if let Some(bits) = attrs.get("Resolution").and_then(|v| v.parse::<u8>().ok()) {
                meta.bits_per_pixel = bits;
                meta.pixel_type = if bits <= 8 {
                    PixelType::Uint8
                } else if bits <= 16 {
                    PixelType::Uint16
                } else {
                    PixelType::Float32
                };
            }
        } else if name.eq_ignore_ascii_case("DimensionDescription") {
            let Some(dim_id) = attrs.get("DimID").and_then(|v| v.parse::<u32>().ok()) else {
                continue;
            };
            let Some(elements) = attrs
                .get("NumberOfElements")
                .and_then(|v| v.parse::<u32>().ok())
            else {
                continue;
            };
            meta.series_metadata.insert(
                format!("xlef.lms.dimension.{dim_id}.elements"),
                crate::common::metadata::MetadataValue::Int(elements as i64),
            );
            if let Some(unit) = attrs.get("Unit").filter(|v| !v.is_empty()) {
                meta.series_metadata.insert(
                    format!("xlef.lms.dimension.{dim_id}.unit"),
                    crate::common::metadata::MetadataValue::String(unit.clone()),
                );
            }
            if let Some(length) = attrs.get("Length").and_then(|v| xlef_parse_f64(v)) {
                meta.series_metadata.insert(
                    format!("xlef.lms.dimension.{dim_id}.length"),
                    crate::common::metadata::MetadataValue::Float(length),
                );
            }
            if let Some(physical_size_um) = xlef_lms_physical_size_um(attrs, elements) {
                meta.series_metadata.insert(
                    format!("xlef.lms.dimension.{dim_id}.physical_size_um"),
                    crate::common::metadata::MetadataValue::Float(physical_size_um),
                );
                match dim_id {
                    1 => {
                        meta.series_metadata.insert(
                            "xlef.lms.physical_size_x".into(),
                            crate::common::metadata::MetadataValue::Float(physical_size_um),
                        );
                    }
                    2 => {
                        meta.series_metadata.insert(
                            "xlef.lms.physical_size_y".into(),
                            crate::common::metadata::MetadataValue::Float(physical_size_um),
                        );
                    }
                    3 => {
                        meta.series_metadata.insert(
                            "xlef.lms.physical_size_z".into(),
                            crate::common::metadata::MetadataValue::Float(physical_size_um),
                        );
                    }
                    _ => {}
                }
            }
            match dim_id {
                1 if meta.size_x == 0 => meta.size_x = elements,
                2 if meta.size_y == 0 => meta.size_y = elements,
                3 => meta.size_z = elements.max(1),
                4 => meta.size_t = elements.max(1),
                5 => {
                    meta.size_c = elements.max(1);
                    size_c_from_dimension = true;
                }
                10 => {
                    meta.series_metadata.insert(
                        "xlef.lms.tile_count".into(),
                        crate::common::metadata::MetadataValue::Int(elements as i64),
                    );
                }
                _ => {}
            }
        }
    }

    xlef_lms_capture_graph_metadata(&mut meta, &tags);

    if let Some(value) = xlef_lms_description_text(&xml) {
        meta.series_metadata
            .entry("xlef.lms.description".into())
            .or_insert_with(|| crate::common::metadata::MetadataValue::String(value));
    }

    if !size_c_from_dimension && channel_count_from_descriptions > 0 {
        meta.size_c = channel_count_from_descriptions;
    }
    if meta.size_x == 0 || meta.size_y == 0 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "Leica XLEF LMS metadata leaf {} does not declare bounded X/Y dimensions",
            reference.display()
        )));
    }
    meta.image_count = meta
        .size_z
        .saturating_mul(meta.size_c)
        .saturating_mul(meta.size_t)
        .max(1);
    Ok(meta)
}

const XLEF_LMS_GRAPH_CAPTURE_LIMIT: usize = 16;

fn xlef_lms_capture_graph_metadata(
    meta: &mut ImageMetadata,
    tags: &[(String, std::collections::HashMap<String, String>)],
) {
    let mut counts: HashMap<&'static str, usize> = HashMap::new();
    meta.series_metadata.insert(
        "xlef.lms.graph.tag_count".into(),
        crate::common::metadata::MetadataValue::Int(tags.len() as i64),
    );

    for (name, attrs) in tags {
        let Some(kind) = xlef_lms_graph_kind(name) else {
            continue;
        };
        let index = counts.entry(kind).or_insert(0);
        if *index < XLEF_LMS_GRAPH_CAPTURE_LIMIT {
            xlef_lms_capture_graph_attrs(meta, kind, *index, attrs);
        }
        *index += 1;
    }

    for (kind, count) in counts {
        meta.series_metadata.insert(
            format!("xlef.lms.graph.{kind}_count"),
            crate::common::metadata::MetadataValue::Int(count as i64),
        );
        if count > XLEF_LMS_GRAPH_CAPTURE_LIMIT {
            meta.series_metadata.insert(
                format!("xlef.lms.graph.{kind}_captured"),
                crate::common::metadata::MetadataValue::Int(XLEF_LMS_GRAPH_CAPTURE_LIMIT as i64),
            );
        }
    }
}

fn xlef_lms_graph_kind(name: &str) -> Option<&'static str> {
    let local = name.rsplit(':').next().unwrap_or(name).to_ascii_lowercase();
    match local.as_str() {
        "detectordescription" | "detector" => Some("detector"),
        "laserdescription" | "laser" | "lasersource" => Some("laser"),
        "objectivedescription" | "objective" => Some("objective"),
        "roi" | "roidescription" | "regionofinterest" => Some("roi"),
        "stageposition" | "position" | "positiondescription" => Some("position"),
        "timestamp" | "timestampdescription" | "timestamplist" => Some("timestamp"),
        _ => None,
    }
}

fn xlef_lms_capture_graph_attrs(
    meta: &mut ImageMetadata,
    kind: &str,
    index: usize,
    attrs: &std::collections::HashMap<String, String>,
) {
    for key in [
        "ID",
        "Id",
        "UUID",
        "Name",
        "Type",
        "Model",
        "Manufacturer",
        "Serial",
        "SerialNumber",
        "ClassName",
        "Wavelength",
        "Power",
        "Magnification",
        "NumericalAperture",
        "NA",
        "Immersion",
        "Correction",
        "Medium",
        "X",
        "Y",
        "Z",
        "PositionX",
        "PositionY",
        "PositionZ",
        "Time",
        "TimeStamp",
    ] {
        let Some(raw) = attrs.get(key) else {
            continue;
        };
        let Some(value) = xlef_lms_clean_bounded_text(raw) else {
            continue;
        };
        let metadata_value = if xlef_lms_graph_float_attr(key) {
            let Some(float_value) = xlef_parse_f64(&value) else {
                continue;
            };
            crate::common::metadata::MetadataValue::Float(float_value)
        } else if let Ok(int_value) = value.parse::<i64>() {
            crate::common::metadata::MetadataValue::Int(int_value)
        } else if let Some(float_value) = xlef_parse_f64(&value) {
            crate::common::metadata::MetadataValue::Float(float_value)
        } else {
            crate::common::metadata::MetadataValue::String(value)
        };
        meta.series_metadata.insert(
            format!("xlef.lms.{kind}.{index}.{}", xlef_lms_key_name(key)),
            metadata_value,
        );
    }
}

fn xlef_lms_graph_float_attr(key: &str) -> bool {
    matches!(
        key,
        "Wavelength"
            | "Power"
            | "Magnification"
            | "NumericalAperture"
            | "NA"
            | "X"
            | "Y"
            | "Z"
            | "PositionX"
            | "PositionY"
            | "PositionZ"
            | "Time"
            | "TimeStamp"
    )
}

fn xlef_lms_description_attr(attrs: &std::collections::HashMap<String, String>) -> Option<String> {
    for key in ["Description", "Comment", "UserComment", "Notes"] {
        if let Some(value) = attrs.get(key).and_then(|v| xlef_lms_clean_text(v)) {
            return Some(value);
        }
    }
    None
}

fn xlef_lms_description_text(xml: &str) -> Option<String> {
    for tag in ["Description", "Comment", "UserComment", "Notes"] {
        if let Some(value) = scn_element_text(xml, tag).and_then(|v| xlef_lms_clean_text(&v)) {
            return Some(value);
        }
    }
    None
}

fn xlef_lms_clean_text(value: &str) -> Option<String> {
    xlef_lms_clean_text_with_limit(value, usize::MAX)
}

fn xlef_lms_clean_bounded_text(value: &str) -> Option<String> {
    xlef_lms_clean_text_with_limit(value, 256)
}

fn xlef_lms_clean_text_with_limit(value: &str, max_chars: usize) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.chars().take(max_chars).collect())
    }
}

fn xlef_lms_insert_channel_metadata(
    meta: &mut ImageMetadata,
    channel_index: u32,
    attrs: &std::collections::HashMap<String, String>,
) {
    let prefix = format!("xlef.lms.channel.{channel_index}");
    for key in ["Name", "DyeName", "Dye", "LUTName"] {
        if let Some(value) = attrs.get(key).filter(|v| !v.trim().is_empty()) {
            meta.series_metadata.insert(
                format!("{prefix}.{}", xlef_lms_key_name(key)),
                crate::common::metadata::MetadataValue::String(value.trim().to_string()),
            );
        }
    }
    for key in [
        "ExcitationWavelength",
        "EmissionWavelength",
        "Pinhole",
        "PinholeAiry",
        "PinholeSize",
        "BytesInc",
    ] {
        if let Some(value) = attrs.get(key).and_then(|v| xlef_parse_f64(v)) {
            meta.series_metadata.insert(
                format!("{prefix}.{}", xlef_lms_key_name(key)),
                crate::common::metadata::MetadataValue::Float(value),
            );
        }
    }
    if let Some(bits) = attrs.get("Resolution").and_then(|v| v.parse::<i64>().ok()) {
        meta.series_metadata.insert(
            format!("{prefix}.resolution"),
            crate::common::metadata::MetadataValue::Int(bits),
        );
    }
}

fn xlef_lms_key_name(key: &str) -> String {
    if key.chars().all(|ch| !ch.is_ascii_lowercase()) {
        return key.to_ascii_lowercase();
    }
    let mut out = String::new();
    for (i, ch) in key.chars().enumerate() {
        if ch.is_ascii_uppercase() && i > 0 {
            out.push('_');
        }
        out.push(ch.to_ascii_lowercase());
    }
    out
}

fn xlef_parse_f64(value: &str) -> Option<f64> {
    let parsed = value.trim().parse::<f64>().ok()?;
    parsed.is_finite().then_some(parsed)
}

fn xlef_lms_physical_size_um(
    attrs: &std::collections::HashMap<String, String>,
    elements: u32,
) -> Option<f64> {
    if elements <= 1 {
        return None;
    }
    let length = attrs.get("Length").and_then(|v| xlef_parse_f64(v))?;
    let mut value = length / (elements as f64 - 1.0);
    match attrs.get("Unit").map(|v| v.as_str()) {
        Some("m") => value *= 1_000_000.0,
        Some("mm") => value *= 1_000.0,
        Some("nm") => value /= 1_000.0,
        Some("Ks") => value /= 1_000.0,
        _ => {}
    }
    value.is_finite().then_some(value.abs())
}

fn xlef_lms_metadata_float(meta: &ImageMetadata, key: &str) -> Option<f64> {
    match meta.series_metadata.get(key) {
        Some(crate::common::metadata::MetadataValue::Float(value)) if value.is_finite() => {
            Some(*value)
        }
        Some(crate::common::metadata::MetadataValue::Int(value)) => Some(*value as f64),
        Some(crate::common::metadata::MetadataValue::String(value)) => xlef_parse_f64(value),
        _ => None,
    }
}

fn xlef_lms_metadata_string(meta: &ImageMetadata, key: &str) -> Option<String> {
    match meta.series_metadata.get(key) {
        Some(crate::common::metadata::MetadataValue::String(value)) if !value.is_empty() => {
            Some(value.clone())
        }
        _ => None,
    }
}

fn xlef_lms_ome_metadata(meta: &ImageMetadata) -> crate::common::ome_metadata::OmeMetadata {
    let mut ome = crate::common::ome_metadata::OmeMetadata::from_image_metadata(meta);
    if let Some(image) = ome.images.get_mut(0) {
        image.name = xlef_lms_metadata_string(meta, "xlef.lms.image.name")
            .or_else(|| xlef_lms_metadata_string(meta, "xlef.lms.element.name"));
        image.description = xlef_lms_metadata_string(meta, "xlef.lms.description");
        image.physical_size_x = xlef_lms_metadata_float(meta, "xlef.lms.physical_size_x");
        image.physical_size_y = xlef_lms_metadata_float(meta, "xlef.lms.physical_size_y");
        image.physical_size_z = xlef_lms_metadata_float(meta, "xlef.lms.physical_size_z");

        let channel_count = if meta.is_rgb {
            1
        } else {
            meta.size_c.max(1) as usize
        };
        if image.channels.len() < channel_count {
            image.channels.resize_with(
                channel_count,
                crate::common::ome_metadata::OmeChannel::default,
            );
        }
        for (channel_index, channel) in image.channels.iter_mut().enumerate() {
            let prefix = format!("xlef.lms.channel.{channel_index}");
            channel.name = xlef_lms_metadata_string(meta, &format!("{prefix}.name"))
                .or_else(|| xlef_lms_metadata_string(meta, &format!("{prefix}.dye_name")))
                .or_else(|| xlef_lms_metadata_string(meta, &format!("{prefix}.dye")));
            channel.excitation_wavelength =
                xlef_lms_metadata_float(meta, &format!("{prefix}.excitation_wavelength"))
                    .filter(|v| *v > 0.0);
            channel.emission_wavelength =
                xlef_lms_metadata_float(meta, &format!("{prefix}.emission_wavelength"))
                    .filter(|v| *v > 0.0);
        }
    }
    let _ = ome.add_original_metadata_annotations(meta, 0);
    ome
}

fn xlef_decode_lms_text(data: &[u8]) -> Result<String> {
    if data.starts_with(&[0xff, 0xfe]) {
        let units: Vec<u16> = data[2..]
            .chunks_exact(2)
            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
            .collect();
        return String::from_utf16(&units).map_err(|_| {
            BioFormatsError::UnsupportedFormat(
                "Leica XLEF LMS metadata is not valid UTF-16LE".into(),
            )
        });
    }
    if data.len() >= 4 && data[1] == 0 && data[3] == 0 {
        let units: Vec<u16> = data
            .chunks_exact(2)
            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
            .collect();
        return String::from_utf16(&units).map_err(|_| {
            BioFormatsError::UnsupportedFormat(
                "Leica XLEF LMS metadata is not valid UTF-16LE".into(),
            )
        });
    }
    String::from_utf8(data.to_vec()).map_err(|_| {
        BioFormatsError::UnsupportedFormat("Leica XLEF LMS metadata is not valid UTF-8".into())
    })
}

fn xlef_format_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

impl Default for XlefReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for XlefReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("xlef"))
    }
    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }
    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let references = Self::referenced_images(path)?;
        for reference in references {
            match reference {
                XlefReference::Image(reference) => {
                    self.add_delegate(&reference, xlef_delegate_for_reference(&reference))?;
                }
                XlefReference::Lms(reference) => {
                    if let Some(reader) = xlef_lms_delegate_for_reference(&reference)? {
                        self.add_initialized_delegate(&reference, reader)?;
                    } else {
                        let metadata = xlef_lms_metadata_for_reference(&reference)?;
                        let metadata_index = self.lms_metadata.len();
                        self.lms_metadata.push(metadata);
                        self.series_map.push(XlefSeriesRef::Lms {
                            metadata: metadata_index,
                        });
                    }
                }
            }
        }
        self.current_series = 0;
        self.rebuild_project_metadata(path)?;
        self.set_delegate_series_for_current()?;
        Ok(())
    }
    fn close(&mut self) -> Result<()> {
        for delegate in &mut self.delegates {
            delegate.reader.close()?;
        }
        self.delegates.clear();
        self.lms_metadata.clear();
        self.series_map.clear();
        self.project_metadata.clear();
        self.current_series = 0;
        Ok(())
    }
    fn series_count(&self) -> usize {
        self.series_map.len()
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        let mapping = *self
            .series_map
            .get(s)
            .ok_or(BioFormatsError::SeriesOutOfRange(s))?;
        if let XlefSeriesRef::Delegate { delegate, series } = mapping {
            self.delegates[delegate].reader.set_series(series)?;
        }
        self.current_series = s;
        Ok(())
    }
    fn series(&self) -> usize {
        self.current_series
    }
    fn metadata(&self) -> &ImageMetadata {
        self.project_metadata
            .get(self.current_series)
            .or_else(|| self.current_delegate().map(|reader| reader.metadata()))
            .or_else(|| self.current_lms_metadata())
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }
    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        self.current_delegate_mut()?.open_bytes(p)
    }
    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        self.current_delegate_mut()?
            .open_bytes_region(p, x, y, w, h)
    }
    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        self.current_delegate_mut()?.open_thumb_bytes(p)
    }
    fn resolution_count(&self) -> usize {
        self.current_delegate()
            .map(|reader| reader.resolution_count())
            .unwrap_or(1)
    }
    fn set_resolution(&mut self, level: usize) -> Result<()> {
        self.current_delegate_mut()?.set_resolution(level)
    }
    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        match self.series_map.get(self.current_series)? {
            XlefSeriesRef::Delegate { delegate, .. } => {
                self.delegates.get(*delegate)?.reader.ome_metadata()
            }
            XlefSeriesRef::Lms { metadata } => {
                self.lms_metadata.get(*metadata).map(xlef_lms_ome_metadata)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 9. Olympus OIR
// ---------------------------------------------------------------------------
//
// Port of the Java `OIRReader`. Native `.oir` files begin with the 16-byte
// identifier `OLYMPUSRAWFORMAT`; pixel data is stored as a sequence of raw
// "pixel blocks", each preceded by a UID encoding the plane/block index
// (e.g. `z001t001_<channel-id>_0`). Blocks of XML are interspersed and define
// the acquisition parameters (dimensions, channels, LUTs). Large acquisitions
// (>1 GB) spill into companion files named `<base>_00001`, `<base>_00002`, ...
//
// In addition to the native path, this reader includes a TIFF-delegate
// fallback: some Olympus exports (e.g. maximum-intensity-projection snapshots)
// are saved with a `.oir` extension but actually contain a plain (often
// ImageJ-flavoured) TIFF. Java has no such fallback and simply fails on those
// files; we delegate to the internal `TiffReader` so they still open with the
// correct dimensions and pixels.

/// 16-byte magic identifier for native Olympus OIR files.
const OIR_IDENTIFIER: &[u8] = b"OLYMPUSRAWFORMAT";

/// A single raw pixel block within an OIR (companion) file.
#[derive(Clone)]
struct OirPixelBlock {
    /// File that physically contains the block (main or companion).
    file: PathBuf,
    /// Absolute offset of the raw pixel bytes (header already skipped).
    data_offset: u64,
    /// Number of raw pixel bytes in this block.
    length: usize,
    /// First image row (inclusive) covered by the block within its plane.
    y_start: usize,
    /// One past the last image row covered by the block.
    y_end: usize,
}

/// Resolved native-OIR state produced by `parse_oir_native`.
struct OirNative {
    meta: ImageMetadata,
    /// (c, z, t) -> blocks for that plane, indexed by block number.
    czt_blocks: std::collections::HashMap<(i32, i32, i32), Vec<Option<OirPixelBlock>>>,
}

/// Internal state of an initialized [`OirReader`].
enum OirState {
    /// Native `OLYMPUSRAWFORMAT` container.
    Native(Box<OirNative>),
    /// `.oir`-named file that is actually a TIFF; delegated to `TiffReader`.
    /// Carries an overridden metadata copy (e.g. ImageJ channel count).
    Tiff(Box<crate::tiff::TiffReader>, ImageMetadata),
}

/// Olympus OIR format reader (`.oir`).
pub struct OirReader {
    state: Option<OirState>,
}

impl OirReader {
    pub fn new() -> Self {
        OirReader { state: None }
    }
}

impl Default for OirReader {
    fn default() -> Self {
        Self::new()
    }
}

/// Minimal forward cursor over an in-memory file buffer with little-endian
/// readers, mirroring the subset of `RandomAccessInputStream` the Java reader
/// uses. All reads are bounds-checked and return `None` past EOF (the Java code
/// relies on `EOFException` to terminate some scan loops).
struct OirCursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> OirCursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        OirCursor { data, pos: 0 }
    }
    fn len(&self) -> usize {
        self.data.len()
    }
    fn tell(&self) -> usize {
        self.pos
    }
    fn seek(&mut self, p: usize) {
        self.pos = p;
    }
    fn skip(&mut self, n: usize) {
        self.pos = self.pos.saturating_add(n);
    }
    fn read_u32(&mut self) -> Option<u32> {
        if self.pos + 4 > self.data.len() {
            self.pos = self.data.len();
            return None;
        }
        let v = u32::from_le_bytes(self.data[self.pos..self.pos + 4].try_into().unwrap());
        self.pos += 4;
        Some(v)
    }
    /// Read `n` bytes as a UTF-8 (lossy) string, clamped to EOF.
    fn read_string(&mut self, n: usize) -> String {
        let end = (self.pos + n).min(self.data.len());
        let s = String::from_utf8_lossy(&self.data[self.pos..end]).into_owned();
        self.pos = end;
        s
    }
    /// Seek forward to the next `<?xml` marker (ASCII), positioning just after
    /// the matched bytes (mirrors `findString("<?xml")`).
    fn find_xml(&mut self) -> bool {
        let needle = b"<?xml";
        if self.pos >= self.data.len() {
            return false;
        }
        if let Some(rel) = self.data[self.pos..]
            .windows(needle.len())
            .position(|w| w == needle)
        {
            self.pos += rel + needle.len();
            true
        } else {
            self.pos = self.data.len();
            false
        }
    }
}

/// Parse pixel blocks (and collect XML strings) from one OIR file buffer,
/// porting `OIRReader.readPixelsFile` / `skipPixelBlock`.
fn oir_scan_file(
    file: &Path,
    data: &[u8],
    is_current: bool,
    pixel_blocks: &mut Vec<(String, OirPixelBlock)>,
    xml_blocks: &mut Vec<String>,
    blocks_per_plane: &mut i32,
) {
    let mut s = OirCursor::new(data);
    if data.len() < 20 {
        return;
    }
    // Seek past the leading 16-byte identifier and the framing that follows,
    // up to the 0xffffffff terminator.
    s.seek(16);
    loop {
        match s.read_u32() {
            Some(0xffff_ffff) => break,
            Some(_) => {}
            None => return,
        }
    }
    s.skip(4);

    let pixel_start = s.tell();
    // Skip reference image blocks (not stored).
    while oir_skip_pixel_block(file, &mut s, false, pixel_blocks, blocks_per_plane) {}

    if s.tell() == pixel_start && !is_current {
        loop {
            match s.read_u32() {
                Some(0xffff_ffff) => break,
                Some(_) => {}
                None => return,
            }
        }
        s.skip(4);
    }

    oir_read_xml_block(&mut s, is_current, xml_blocks);

    while oir_skip_pixel_block(file, &mut s, true, pixel_blocks, blocks_per_plane) {}

    oir_read_xml_block(&mut s, is_current, xml_blocks);

    while s.tell() + 16 < s.len() {
        if !s.find_xml() {
            break;
        }
        // back up to the 4-byte length that precedes "<?xml" (5 bytes) by 9
        let mark = s.tell();
        if mark < 9 {
            break;
        }
        s.seek(mark - 9);
        let length = match s.read_u32() {
            Some(v) => v as i64,
            None => break,
        };
        if length <= 0 || (length as usize) + s.tell() > s.len() {
            break;
        }
        let fp = s.tell();
        let xml = s.read_string(length as usize);
        if !xml.starts_with("<?xml") {
            // resync: step back two bytes and keep scanning
            s.seek(fp.saturating_sub(2));
            continue;
        }
        let xml = xml.trim().to_string();
        let expect_pixel_block = xml.trim_end().ends_with(":frameProperties>");
        if is_current {
            xml_blocks.push(xml);
        }
        if expect_pixel_block {
            while oir_skip_pixel_block(file, &mut s, true, pixel_blocks, blocks_per_plane) {}
        }
    }
}

/// Port of `OIRReader.skipPixelBlock`. Returns `true` if a block (real or
/// reference) was consumed and scanning should continue.
fn oir_skip_pixel_block(
    file: &Path,
    s: &mut OirCursor,
    store: bool,
    pixel_blocks: &mut Vec<(String, OirPixelBlock)>,
    blocks_per_plane: &mut i32,
) -> bool {
    let offset = s.tell();
    if offset + 8 >= s.len() {
        return false;
    }
    let check_length = match s.read_u32() {
        Some(v) => v,
        None => return false,
    };
    let check = match s.read_u32() {
        Some(v) => v,
        None => return false,
    };
    if check != 3 {
        s.seek(offset);
        if check == 2 {
            s.seek(offset + check_length as usize + 8);
            return true;
        }
        return false;
    }

    s.skip(8);
    let uid_length = match s.read_u32() {
        Some(v) => v,
        None => return false,
    };
    if check_length != uid_length.wrapping_add(12) {
        s.seek(offset);
        return false;
    }
    let uid = s.read_string(uid_length as usize);
    if s.tell() + 4 >= s.len() {
        return false;
    }
    let pixel_bytes = match s.read_u32() {
        Some(v) => v,
        None => return false,
    };
    s.skip(4);
    let data_offset = s.tell() as u64;

    if store && pixel_bytes > 0 {
        if let Some(block_index) = uid.rsplit('_').next().and_then(|t| t.parse::<i32>().ok()) {
            if block_index >= *blocks_per_plane {
                *blocks_per_plane = block_index + 1;
            }
        }
        pixel_blocks.push((
            uid,
            OirPixelBlock {
                file: file.to_path_buf(),
                data_offset,
                length: pixel_bytes as usize,
                y_start: 0,
                y_end: 0,
            },
        ));
    } else if pixel_bytes == 0 {
        return false;
    }
    s.skip(pixel_bytes as usize);
    true
}

/// Port of `OIRReader.readXMLBlock`: a length-prefixed container holding one or
/// more XML strings. Extracted XML is appended to `xml_blocks` when current.
fn oir_read_xml_block(s: &mut OirCursor, is_current: bool, xml_blocks: &mut Vec<String>) {
    let offset = s.tell();
    if offset + 8 >= s.len() {
        return;
    }
    let total_block_length = match s.read_u32() {
        Some(v) => v as usize,
        None => return,
    };
    if total_block_length < 4 {
        s.seek(offset);
        return;
    }
    let end = s.tell() + total_block_length - 4;
    s.skip(4);

    let default_skip = 36usize;
    while s.tell() < end {
        s.skip(default_skip);
        let mut xml_length = match s.read_u32() {
            Some(v) => v as i64,
            None => return,
        };
        if xml_length <= 32 {
            // small value: skip an embedded UID then read the real length
            let n = match s.read_u32() {
                Some(v) => v as usize,
                None => return,
            };
            let _uid = s.read_string(n);
            xml_length = match s.read_u32() {
                Some(v) => v as i64,
                None => return,
            };
        }
        if xml_length <= 32 || s.tell() + xml_length as usize > end + 8 {
            break;
        }
        let xml = s.read_string(xml_length as usize);
        let xml = xml.trim().to_string();
        if !xml.starts_with("<?xml") {
            break;
        }
        if is_current || xml.contains("lut:LUT") {
            xml_blocks.push(xml);
        }
    }
}

/// Extract the trimmed text content of the first element whose (possibly
/// namespaced) name ends with `local`. Returns `None` if absent.
fn oir_xml_text(xml: &str, local: &str) -> Option<String> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut capture = false;
    let mut depth_match = 0usize;
    let mut text = String::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let name = e.name();
                if local_name_matches(name.as_ref(), local) {
                    capture = true;
                    depth_match += 1;
                    text.clear();
                }
            }
            Ok(Event::Text(t)) if capture && depth_match > 0 => {
                if let Ok(s) = t.unescape() {
                    text.push_str(&s);
                }
            }
            Ok(Event::End(e)) => {
                if local_name_matches(e.name().as_ref(), local) && capture {
                    return Some(text.trim().to_string());
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }
    None
}

fn local_name_matches(qname: &[u8], local: &str) -> bool {
    let after_colon = qname
        .iter()
        .rposition(|&b| b == b':')
        .map(|i| &qname[i + 1..])
        .unwrap_or(qname);
    after_colon == local.as_bytes()
}

/// Parse a native `OLYMPUSRAWFORMAT` OIR file (and any companion files) into
/// resolved [`OirNative`] state. This ports the metadata/dimension/pixel-block
/// portions of `OIRReader.initFile`; per-channel laser/detector/objective
/// enrichment present in Java is intentionally omitted.
fn parse_oir_native(path: &Path) -> Result<OirNative> {
    // Resolve companion files: <base>_00001, <base>_00002, ... in the same dir.
    let parent = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();
    let mut files: Vec<PathBuf> = vec![path.to_path_buf()];
    if let Ok(entries) = std::fs::read_dir(&parent) {
        let mut companions: Vec<(u32, PathBuf)> = Vec::new();
        let prefix = format!("{stem}_");
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(rest) = name.strip_prefix(&prefix) {
                if rest.len() == 5 {
                    if let Ok(idx) = rest.parse::<u32>() {
                        companions.push((idx, entry.path()));
                    }
                }
            }
        }
        companions.sort_by_key(|(idx, _)| *idx);
        files.extend(companions.into_iter().map(|(_, p)| p));
    }

    let mut meta = ImageMetadata {
        size_z: 1,
        size_c: 1,
        size_t: 1,
        is_little_endian: true,
        ..ImageMetadata::default()
    };

    let mut pixel_blocks: Vec<(String, OirPixelBlock)> = Vec::new();
    let mut xml_blocks: Vec<String> = Vec::new();
    let mut blocks_per_plane: i32 = 0;
    let mut channel_ids: Vec<String> = Vec::new();

    for (i, file) in files.iter().enumerate() {
        let data = std::fs::read(file).map_err(BioFormatsError::Io)?;
        if i == 0 && !data.starts_with(OIR_IDENTIFIER) {
            return Err(BioFormatsError::UnsupportedFormat(
                "Not an OLYMPUSRAWFORMAT Olympus OIR file".into(),
            ));
        }
        oir_scan_file(
            file,
            &data,
            i == 0,
            &mut pixel_blocks,
            &mut xml_blocks,
            &mut blocks_per_plane,
        );
    }

    // Parse XML metadata for dimensions and channels.
    oir_apply_xml(&xml_blocks, &mut meta, &mut channel_ids);

    if meta.size_x == 0 || meta.size_y == 0 {
        return Err(BioFormatsError::Format(
            "Olympus OIR XML metadata did not define image dimensions".into(),
        ));
    }
    if channel_ids.is_empty() {
        // Fall back to channel ids discovered in the pixel block UIDs.
        channel_ids = oir_channel_ids_from_uids(&pixel_blocks);
    }
    let channel_count = channel_ids.len().max(1) as u32;

    // sizeC starts at 1 (or LAMBDA size) and is multiplied by channel count,
    // mirroring `m.sizeC *= channels.size()`.
    meta.size_c = meta.size_c.max(1) * channel_count;

    // Determine min/max Z and T across stored blocks when blocks are missing.
    let mut min_z = i32::MAX;
    let mut min_t = i32::MAX;
    let image_count_full = meta.size_c * meta.size_z * meta.size_t;
    if blocks_per_plane > 0
        && (blocks_per_plane as usize) * (image_count_full as usize) != pixel_blocks.len()
    {
        let mut max_z = 0;
        let mut max_t = 0;
        for (uid, _) in &pixel_blocks {
            if oir_get_block(uid) == blocks_per_plane - 1 {
                let z = oir_get_z(uid);
                let t = oir_get_t(uid);
                max_z = max_z.max(z);
                max_t = max_t.max(t);
                min_z = min_z.min(z);
                min_t = min_t.min(t);
            }
        }
        if min_z != i32::MAX {
            meta.size_z = ((max_z - min_z) + 1) as u32;
            meta.size_t = ((max_t - min_t) + 1) as u32;
        }
    }
    if min_z == i32::MAX {
        min_z = 0;
    }
    if min_t == i32::MAX {
        min_t = 0;
    }
    meta.image_count = meta.size_c * meta.size_z * meta.size_t;

    // Dimension order: Java emits "XYC" + a Z/T ordering; our enum's closest
    // match is XYCZT, which is correct whenever Z or T is singleton (the common
    // case) and a reasonable default otherwise.
    meta.dimension_order = crate::common::metadata::DimensionOrder::XYCZT;

    // Group blocks by (c,z,t) into per-plane block arrays.
    let max_blocks = pixel_blocks
        .iter()
        .map(|(uid, _)| oir_get_block(uid) + 1)
        .max()
        .unwrap_or(1)
        .max(1) as usize;

    let mut czt_blocks: std::collections::HashMap<(i32, i32, i32), Vec<Option<OirPixelBlock>>> =
        std::collections::HashMap::new();
    for (uid, block) in &pixel_blocks {
        let z = oir_get_z(uid) - min_z;
        let t = oir_get_t(uid) - min_t;
        let c = oir_get_c(uid, &channel_ids) + oir_get_l(uid);
        let b = oir_get_block(uid) as usize;
        let entry = czt_blocks
            .entry((c, z, t))
            .or_insert_with(|| vec![None; max_blocks]);
        if b < entry.len() {
            entry[b] = Some(block.clone());
        }
    }

    // Compute per-block Y extents within each plane.
    let bpp = meta.pixel_type.bytes_per_sample();
    let bytes_per_line = (meta.size_x as usize).max(1) * bpp;
    for blocks in czt_blocks.values_mut() {
        let mut y_start = 0usize;
        for block in blocks.iter_mut().flatten() {
            block.y_start = y_start;
            let n_lines = if bytes_per_line > 0 {
                block.length / bytes_per_line
            } else {
                0
            };
            y_start += n_lines;
            block.y_end = y_start;
        }
    }

    Ok(OirNative { meta, czt_blocks })
}

/// Apply parsed OIR XML blocks to metadata (dimensions, pixel type, channels).
fn oir_apply_xml(xml_blocks: &[String], meta: &mut ImageMetadata, channel_ids: &mut Vec<String>) {
    for xml in xml_blocks {
        // frame:frameProperties -> imageDefinition width/height/depth/bitCounts
        if xml.contains("frameProperties") {
            let rgb = oir_xml_text(xml, "colorType")
                .map(|c| c.trim().eq_ignore_ascii_case("RGB"))
                .unwrap_or(false);
            if let Some(w) = oir_xml_text(xml, "width").and_then(|v| v.trim().parse::<u32>().ok()) {
                if meta.size_x == 0 {
                    meta.size_x = w;
                }
            }
            if let Some(h) = oir_xml_text(xml, "height").and_then(|v| v.trim().parse::<u32>().ok())
            {
                if meta.size_y == 0 {
                    meta.size_y = h;
                }
            }
            if let Some(mut depth) =
                oir_xml_text(xml, "depth").and_then(|v| v.trim().parse::<u32>().ok())
            {
                if rgb {
                    depth /= 3;
                }
                let (pt, bits) = oir_pixel_type_from_bytes(depth);
                meta.pixel_type = pt;
                if meta.bits_per_pixel == 0 || meta.bits_per_pixel == 8 {
                    meta.bits_per_pixel = bits;
                }
            }
            if let Some(mut bits) =
                oir_xml_text(xml, "bitCounts").and_then(|v| v.trim().parse::<u32>().ok())
            {
                if rgb {
                    bits /= 3;
                }
                meta.bits_per_pixel = bits as u8;
            }
        }

        // image:imageProperties -> imageInfo width/height, axes, channels
        if xml.contains("imageProperties") || xml.contains("imageInfo") {
            if meta.size_x == 0 {
                if let Some(w) =
                    oir_xml_text(xml, "width").and_then(|v| v.trim().parse::<u32>().ok())
                {
                    meta.size_x = w;
                }
            }
            if meta.size_y == 0 {
                if let Some(h) =
                    oir_xml_text(xml, "height").and_then(|v| v.trim().parse::<u32>().ok())
                {
                    meta.size_y = h;
                }
            }
            oir_apply_axes(xml, meta);
            oir_apply_channels(xml, channel_ids);
        }
    }
}

/// Parse `commonparam:axis` / `commonimage:axis` entries (ZSTACK/TIMELAPSE/
/// LAMBDA) and update Z/T/C sizes, mirroring `OIRReader.parseAxis`.
fn oir_apply_axes(xml: &str, meta: &mut ImageMetadata) {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(xml);
    // Collect axis blocks as (axisName, maxSize) by scanning for <...:axis>
    // wrappers that contain a nested <...:axis> name and <...:maxSize> value.
    let mut cur_axis_name: Option<String> = None;
    let mut cur_max_size: Option<u32> = None;
    let mut in_axis_wrapper = 0i32;
    let mut pending_text_for: Option<&'static str> = None;
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let name = e.name();
                let local = local_of(name.as_ref());
                if local == "axis" {
                    // could be the wrapper (dimensionAxis) or the inner name node
                    in_axis_wrapper += 1;
                    pending_text_for = Some("axisname");
                } else if local == "maxSize" {
                    pending_text_for = Some("maxsize");
                } else {
                    pending_text_for = None;
                }
            }
            Ok(Event::Text(t)) => {
                if let Some(kind) = pending_text_for {
                    if let Ok(s) = t.unescape() {
                        let s = s.trim().to_string();
                        if kind == "axisname" && !s.is_empty() {
                            cur_axis_name = Some(s);
                        } else if kind == "maxsize" {
                            cur_max_size = s.parse::<u32>().ok();
                        }
                    }
                }
                pending_text_for = None;
            }
            Ok(Event::End(e)) => {
                let name = e.name();
                let local = local_of(name.as_ref());
                if local == "axis" {
                    in_axis_wrapper -= 1;
                    if in_axis_wrapper <= 0 {
                        if let (Some(name), Some(size)) =
                            (cur_axis_name.take(), cur_max_size.take())
                        {
                            oir_apply_one_axis(&name, size, meta);
                        }
                        in_axis_wrapper = 0;
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }
}

fn oir_apply_one_axis(name: &str, size: u32, meta: &mut ImageMetadata) {
    match name {
        "ZSTACK" => {
            if meta.size_z <= 1 {
                meta.size_z = size.max(1);
            }
        }
        "TIMELAPSE" => {
            if meta.size_t <= 1 {
                meta.size_t = size.max(1);
            }
        }
        "LAMBDA" => {
            meta.size_c = size.max(1);
        }
        _ => {}
    }
}

fn local_of(qname: &[u8]) -> &str {
    let after = qname
        .iter()
        .rposition(|&b| b == b':')
        .map(|i| &qname[i + 1..])
        .unwrap_or(qname);
    std::str::from_utf8(after).unwrap_or("")
}

/// Collect channel ids from `commonphase:channel` / `commonphase:elementChannel`
/// nodes (the `id` attribute), preserving document order.
fn oir_apply_channels(xml: &str, channel_ids: &mut Vec<String>) {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(xml);
    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                let name = e.name();
                let local = local_of(name.as_ref());
                if local == "channel" || local == "elementChannel" {
                    if let Some(id) = e.attributes().flatten().find_map(|a| {
                        if a.key.as_ref() == b"id" {
                            a.unescape_value().ok().map(|v| v.into_owned())
                        } else {
                            None
                        }
                    }) {
                        if !id.is_empty() && !channel_ids.contains(&id) {
                            channel_ids.push(id);
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }
}

/// Derive channel ids from the channel signature embedded in pixel block UIDs.
fn oir_channel_ids_from_uids(pixel_blocks: &[(String, OirPixelBlock)]) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    for (uid, _) in pixel_blocks {
        // uid = ...<channel-id>_<block>; channel signature is the token before
        // the final '_'.
        if let Some(idx) = uid.rfind('_') {
            let before = &uid[..idx];
            if let Some(cidx) = before.rfind('_') {
                let sig = before[cidx + 1..].to_string();
                if !sig.is_empty() && !ids.contains(&sig) {
                    ids.push(sig);
                }
            }
        }
    }
    ids
}

fn oir_pixel_type_from_bytes(bytes: u32) -> (PixelType, u8) {
    match bytes {
        1 => (PixelType::Uint8, 8),
        2 => (PixelType::Uint16, 16),
        4 => (PixelType::Float32, 32),
        _ => (PixelType::Uint16, 16),
    }
}

fn oir_get_z(uid: &str) -> i32 {
    if let Some(idx) = uid.find('z') {
        uid.get(idx + 1..idx + 4)
            .and_then(|s| s.parse::<i32>().ok())
            .map(|v| v - 1)
            .unwrap_or(0)
    } else {
        0
    }
}

fn oir_get_t(uid: &str) -> i32 {
    if let Some(idx) = uid.find('t') {
        let sub = &uid[idx + 1..];
        if let Some(end) = sub.find('_') {
            return sub[..end].parse::<i32>().map(|v| v - 1).unwrap_or(0);
        }
    }
    0
}

fn oir_get_c(uid: &str, channel_ids: &[String]) -> i32 {
    if let Some(idx) = uid.rfind('_') {
        let before = &uid[..idx];
        if let Some(cidx) = before.rfind('_') {
            let sig = &before[cidx + 1..];
            for (i, id) in channel_ids.iter().enumerate() {
                if id == sig {
                    return i as i32;
                }
            }
        }
    }
    0
}

fn oir_get_block(uid: &str) -> i32 {
    if let Some(idx) = uid.rfind('_') {
        uid[idx + 1..].parse::<i32>().unwrap_or(0)
    } else {
        0
    }
}

fn oir_get_l(uid: &str) -> i32 {
    if !uid.starts_with('l') {
        return 0;
    }
    uid.get(1..4)
        .and_then(|s| s.parse::<i32>().ok())
        .map(|v| v - 1)
        .unwrap_or(0)
}

/// Detect whether a `.oir`-named file is actually a TIFF (II*/MM* magic).
fn oir_looks_like_tiff(header: &[u8]) -> bool {
    header.len() >= 4
        && ((header[0..2] == [0x49, 0x49] && header[2..4] == [42, 0])
            || (header[0..2] == [0x4d, 0x4d] && header[2..4] == [0, 42]))
}

/// Build overridden metadata for a TIFF-delegated `.oir` file, applying ImageJ
/// `channels=`/`images=` hints from the ImageDescription when present.
fn oir_tiff_meta(reader: &crate::tiff::TiffReader) -> ImageMetadata {
    let mut meta = reader.series_list()[0].metadata.clone();
    // ImageJ stores hyperstack layout in the ImageDescription of IFD 0.
    if let Some(desc) = reader.ifd(0).and_then(|ifd| {
        ifd.get_str(crate::tiff::ifd::tag::IMAGE_DESCRIPTION)
            .map(str::to_owned)
    }) {
        if desc.contains("ImageJ=") {
            let get = |key: &str| -> Option<u32> {
                desc.lines()
                    .find_map(|l| l.strip_prefix(key))
                    .and_then(|v| v.trim().parse::<u32>().ok())
            };
            let channels = get("channels=");
            let slices = get("slices=");
            let frames = get("frames=");
            if let Some(c) = channels {
                if c > 0 {
                    meta.size_c = c;
                    meta.size_z = slices.unwrap_or(1).max(1);
                    meta.size_t = frames.unwrap_or(1).max(1);
                    meta.is_rgb = false;
                }
            }
        }
    }
    meta
}

impl FormatReader for OirReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("oir"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // Only claim genuine native OIR files by magic; TIFF-flavoured `.oir`
        // exports are handled by extension fallback so we do not hijack the
        // generic TIFF magic in the reader registry.
        header.len() >= OIR_IDENTIFIER.len() && header.starts_with(OIR_IDENTIFIER)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.state = None;
        let header = crate::common::io::peek_header(path, 16)?;

        if header.starts_with(OIR_IDENTIFIER) {
            let native = parse_oir_native(path)?;
            self.state = Some(OirState::Native(Box::new(native)));
            return Ok(());
        }

        if oir_looks_like_tiff(&header) {
            // Non-Java extension: Olympus MIP/snapshot exports saved as `.oir`
            // are plain TIFFs. Delegate to the internal TIFF reader.
            let mut tiff = crate::tiff::TiffReader::new();
            tiff.set_id(path)?;
            let meta = oir_tiff_meta(&tiff);
            self.state = Some(OirState::Tiff(Box::new(tiff), meta));
            return Ok(());
        }

        Err(BioFormatsError::UnsupportedFormat(
            "Olympus OIR file is neither OLYMPUSRAWFORMAT nor a TIFF export".into(),
        ))
    }

    fn close(&mut self) -> Result<()> {
        if let Some(OirState::Tiff(tiff, _)) = &mut self.state {
            let _ = tiff.close();
        }
        self.state = None;
        Ok(())
    }

    fn series_count(&self) -> usize {
        usize::from(self.state.is_some())
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.state.is_none() {
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
        match &self.state {
            Some(OirState::Native(n)) => &n.meta,
            Some(OirState::Tiff(_, meta)) => meta,
            None => crate::common::reader::uninitialized_metadata(),
        }
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        match &mut self.state {
            Some(OirState::Native(n)) => oir_open_plane(n, plane_index),
            Some(OirState::Tiff(tiff, _)) => {
                tiff.set_series(0)?;
                tiff.open_bytes(plane_index)
            }
            None => Err(BioFormatsError::NotInitialized),
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
        match &mut self.state {
            Some(OirState::Native(n)) => {
                let full = oir_open_plane(n, plane_index)?;
                crop_full_plane("Olympus OIR", &full, &n.meta, 1, x, y, w, h)
            }
            Some(OirState::Tiff(tiff, _)) => {
                tiff.set_series(0)?;
                tiff.open_bytes_region(plane_index, x, y, w, h)
            }
            None => Err(BioFormatsError::NotInitialized),
        }
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let (sx, sy) = {
            let meta = self.metadata();
            (meta.size_x, meta.size_y)
        };
        if sx == 0 || sy == 0 {
            return Err(BioFormatsError::NotInitialized);
        }
        let tw = sx.min(256);
        let th = sy.min(256);
        let tx = (sx - tw) / 2;
        let ty = (sy - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}

/// Assemble a full native-OIR plane by concatenating its pixel blocks in order,
/// porting the full-plane case of `OIRReader.openBytes`.
fn oir_open_plane(n: &OirNative, plane_index: u32) -> Result<Vec<u8>> {
    if plane_index >= n.meta.image_count {
        return Err(BioFormatsError::PlaneOutOfRange(plane_index));
    }
    let bpp = n.meta.pixel_type.bytes_per_sample();
    let bytes_per_line = (n.meta.size_x as usize) * bpp;
    let plane_len = bytes_per_line * (n.meta.size_y as usize);
    let mut out = vec![0u8; plane_len];

    let (z, c, t) = oir_zct_coords(&n.meta, plane_index);
    let key = (c as i32, z as i32, t as i32);
    let blocks = match n.czt_blocks.get(&key) {
        Some(b) => b,
        None => return Ok(out), // missing plane: zero-filled (matches fill color)
    };

    let mut y_off_bytes = 0usize;
    for block in blocks.iter().flatten() {
        if y_off_bytes >= out.len() {
            break;
        }
        let mut reader = BufReader::new(File::open(&block.file).map_err(BioFormatsError::Io)?);
        let data = read_bytes_at(&mut reader, block.data_offset, block.length)?;
        let end = (y_off_bytes + data.len()).min(out.len());
        let take = end - y_off_bytes;
        out[y_off_bytes..end].copy_from_slice(&data[..take]);
        y_off_bytes = end;
    }
    Ok(out)
}

/// Convert a plane index to (z, c, t) using the metadata dimension order.
fn oir_zct_coords(meta: &ImageMetadata, no: u32) -> (u32, u32, u32) {
    use crate::common::metadata::DimensionOrder;
    let z = meta.size_z.max(1);
    let c = meta.size_c.max(1);
    let t = meta.size_t.max(1);
    let dims: &[(char, u32)] = match meta.dimension_order {
        DimensionOrder::XYCTZ => &[('C', c), ('T', t), ('Z', z)],
        DimensionOrder::XYCZT => &[('C', c), ('Z', z), ('T', t)],
        DimensionOrder::XYTCZ => &[('T', t), ('C', c), ('Z', z)],
        DimensionOrder::XYTZC => &[('T', t), ('Z', z), ('C', c)],
        DimensionOrder::XYZCT => &[('Z', z), ('C', c), ('T', t)],
        DimensionOrder::XYZTC => &[('Z', z), ('T', t), ('C', c)],
    };
    let mut remaining = no;
    let (mut zz, mut cc, mut tt) = (0u32, 0u32, 0u32);
    for (dim, len) in dims {
        let len = (*len).max(1);
        let value = remaining % len;
        remaining /= len;
        match dim {
            'Z' => zz = value,
            'C' => cc = value,
            'T' => tt = value,
            _ => {}
        }
    }
    (zz, cc, tt)
}

// ---------------------------------------------------------------------------
// 10. Olympus cellSens VSI — TIFF-based delegate
// ---------------------------------------------------------------------------
/// Olympus cellSens VSI format reader (`.vsi`).
///
/// Ported (partially) from the Java `CellSensReader`. The base `.vsi` is a
/// TIFF-like container (parsed by the inner `TiffReader`). High-resolution
/// pyramid pixels live in companion `.ets` files inside `_<name>_/<stack>/`
/// subdirectories. This reader ports the ETS tile-index parsing: it locates the
/// `frame_*.ets` files and reads each ETS `SIS`/`ETS` binary header to recover
/// the tile geometry (tileX/tileY), channel count, compression type, pixel
/// type, and the per-chunk tile-coordinate → file-offset map.
///
/// Full ETS pyramid assembly is implemented here: each `.ets` volume is exposed
/// as an additional series after the inner TIFF's series. For every volume the
/// reader reconstructs the resolution levels (the last tile coordinate when
/// `usePyramid` is set), computes per-level tile grids and plane sizes following
/// the Java halving rules, and assembles tiles into a full plane on
/// `open_bytes`. Tiles are decoded according to the ETS compression code: RAW,
/// JPEG, JPEG-2000, JPEG-lossless, PNG and BMP reuse codec.rs decoders. Tag
/// 700-style metadata and label/overview images continue to be served by the
/// inner TIFF.
pub struct CellSensReader {
    inner: crate::tiff::TiffReader,
    ets: Vec<EtsVolume>,
    /// Number of series owned by the inner TIFF reader.
    tiff_series: usize,
    /// Current target: TIFF series, or ETS volume + resolution level.
    target: CellSensTarget,
    /// Metadata describing the current ETS resolution (when an ETS target is
    /// active). Held so `metadata()` can return a borrow.
    ets_meta: Option<ImageMetadata>,
    /// Flattened series ordering (mirrors Java with flattened resolutions): the
    /// ETS pyramid resolution levels come first (one logical series each), then a
    /// single embedded TIFF image (the overview). Built in `enrich_metadata`.
    series_map: Vec<CellSensTarget>,
    /// Image name per logical series, for OME (CellSensReader.java:994-1031).
    series_names: Vec<String>,
    /// (physicalSizeX, physicalSizeY) per logical series, for OME.
    series_phys: Vec<Option<(f64, f64)>>,
    /// Currently selected logical series index into `series_map`.
    current: usize,
    /// Path to the base `.vsi` file (needed to read embedded-TIFF JPEG strips
    /// directly when the inner reader cannot merge the JPEGTables tag).
    vsi_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CellSensTarget {
    /// Inner TIFF series `usize`.
    Tiff(usize),
    /// ETS volume index + resolution level.
    Ets { volume: usize, resolution: usize },
}

/// One resolution level of an ETS pyramid volume.
#[derive(Debug, Clone, Default)]
struct EtsLevel {
    size_x: u32,
    size_y: u32,
    size_z: u32,
    size_c: u32,
    size_t: u32,
    /// tile grid dimensions for this level
    rows: u32,
    cols: u32,
}

/// Parsed header + tile index for one `.ets` file.
#[derive(Debug, Clone, Default)]
struct EtsVolume {
    path: PathBuf,
    n_dimensions: u32,
    size_c: u32,
    compression: i32,
    tile_x: u32,
    tile_y: u32,
    pixel_type_code: i32,
    /// component order == 1 (BGR) and RAW compression -> swap channels.
    bgr: bool,
    use_pyramid: bool,
    /// background fill color (per-channel sample bytes), if present.
    background: Vec<u8>,
    /// dimension ordering: index in the coordinate vector (already +0, the
    /// Java code adds 2). Maps logical Z/C/T to a coordinate slot index.
    dim_z: Option<usize>,
    dim_c: Option<usize>,
    dim_t: Option<usize>,
    /// (coordinate vector, file offset, byte count) for each used chunk.
    tiles: Vec<(Vec<i32>, u64, u32)>,
    /// per-resolution geometry (index 0 = full resolution).
    levels: Vec<EtsLevel>,
    /// Exact full-resolution width/height parsed from the VSI `Pyramid` tag-tree
    /// (IMAGE_BOUNDARY tag). `None` falls back to the tile-grid extent.
    pyramid_width: Option<u32>,
    pyramid_height: Option<u32>,
    /// Tile origin crop offsets from the VSI tag-tree (TILE_ORIGIN tag). When set,
    /// stored tile pixels are cropped to the declared image size via these
    /// offsets (CellSensReader.java:556-560).
    tile_origin_x: Option<i32>,
    tile_origin_y: Option<i32>,
    /// Canonical dimension ordering from the VSI tag-tree: logical dim -> the
    /// coordinate-vector tag (Java stores tag, used as `tag + 2` for the slot).
    dim_order: VsiDimOrder,
    /// Non-geometry acquisition metadata from the matched `Pyramid` block.
    meta: VsiPyramidMeta,
    /// Physical pixel size (micrometres) from the matched `Pyramid` block.
    physical_size_x: Option<f64>,
    physical_size_y: Option<f64>,
}

/// Canonical dimension ordering parsed from the VSI `Pyramid` tag-tree. Each
/// value is the raw dimension tag; the coordinate-vector slot is `tag + 2`
/// (CellSensReader.java:1122-1123, 1377-1379).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct VsiDimOrder {
    z: Option<i32>,
    t: Option<i32>,
    c: Option<i32>,
    l: Option<i32>,
}

/// One `Pyramid` metadata block parsed from the VSI tag-tree. Holds only the
/// fields the ETS pixel pipeline needs for correct geometry.
#[derive(Debug, Clone, Default)]
struct VsiPyramid {
    width: Option<u32>,
    height: Option<u32>,
    tile_origin_x: Option<i32>,
    tile_origin_y: Option<i32>,
    /// Physical pixel size in micrometres, from RWC_FRAME_SCALE
    /// (CellSensReader.java:1853-1858).
    physical_size_x: Option<f64>,
    physical_size_y: Option<f64>,
    dim_order: VsiDimOrder,
    /// Non-geometry acquisition metadata, captured from the tag-tree
    /// (CellSensReader.java:1881-1979). Lists preserve the order Java appends in.
    meta: VsiPyramidMeta,
}

/// Optional device/objective/exposure/gain metadata for one pyramid block.
/// Mirrors the corresponding `Pyramid` fields in Java (CellSensReader.java:2696-2740).
#[derive(Debug, Clone, Default)]
struct VsiPyramidMeta {
    device_names: Vec<String>,
    device_ids: Vec<String>,
    device_subtypes: Vec<String>,
    device_manufacturers: Vec<String>,
    objective_names: Vec<String>,
    objective_types: Vec<i64>,
    exposure_times: Vec<i64>,
    magnification: Option<f64>,
    numerical_aperture: Option<f64>,
    working_distance: Option<f64>,
    refractive_index: Option<f64>,
    bit_depth: Option<i64>,
    binning_x: Option<i64>,
    binning_y: Option<i64>,
    gain: Option<f64>,
    offset: Option<f64>,
    red_gain: Option<f64>,
    green_gain: Option<f64>,
    blue_gain: Option<f64>,
    red_offset: Option<f64>,
    green_offset: Option<f64>,
    blue_offset: Option<f64>,
    stack_type: Option<String>,
    acquisition_time: Option<i64>,
    /// Prefix-gated VALUE metadata (CellSensReader.java:1960-1979).
    channel_wavelengths: Vec<f64>,
    z_start: Option<f64>,
    z_increment: Option<f64>,
    z_values: Vec<f64>,
    t_values: Vec<f64>,
    /// Per-channel names and stack name from TCHAR leaves
    /// (CellSensReader.java:1769-1778).
    channel_names: Vec<String>,
    name: Option<String>,
    /// EXPOSURE_TIME split by tag prefix (CellSensReader.java:1899-1905).
    /// `exposure_times` (already present) collects top-level (empty-prefix)
    /// exposures; the prefixed ones land here.
    default_exposure_time: Option<i64>,
    other_exposure_times: Vec<i64>,
}

const ETS_RAW: i32 = 0;
const ETS_JPEG: i32 = 2;
const ETS_JPEG_2000: i32 = 3;
const ETS_JPEG_LOSSLESS: i32 = 5;
const ETS_PNG: i32 = 8;
const ETS_BMP: i32 = 9;

// ETS pixel type codes (CellSensReader.java:80-90 / convertPixelType).
const ETS_PT_CHAR: i32 = 1;
const ETS_PT_UCHAR: i32 = 2;
const ETS_PT_SHORT: i32 = 3;
const ETS_PT_USHORT: i32 = 4;
const ETS_PT_INT: i32 = 5;
const ETS_PT_UINT: i32 = 6;
const ETS_PT_FLOAT: i32 = 9;
const ETS_PT_DOUBLE: i32 = 10;

/// Map an ETS pixel type code to a [`PixelType`]. Mirrors Java
/// `CellSensReader.convertPixelType` (CellSensReader.java:1562-1586).
fn convert_ets_pixel_type(code: i32) -> Result<PixelType> {
    Ok(match code {
        ETS_PT_CHAR => PixelType::Int8,
        ETS_PT_UCHAR => PixelType::Uint8,
        ETS_PT_SHORT => PixelType::Int16,
        ETS_PT_USHORT => PixelType::Uint16,
        ETS_PT_INT => PixelType::Int32,
        ETS_PT_UINT => PixelType::Uint32,
        ETS_PT_FLOAT => PixelType::Float32,
        ETS_PT_DOUBLE => PixelType::Float64,
        other => {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "cellSens ETS: unsupported pixel type code {other}"
            )))
        }
    })
}

impl EtsVolume {
    /// Reconstruct resolution-level geometry and the C/Z/T dimension slots from
    /// the tile coordinates. Mirrors the geometry math in Java `parseETSFile`
    /// (CellSensReader.java:1302-1558). The VSI proprietary `Pyramid` metadata
    /// (which Java consults for the exact full-res width/height and the canonical
    /// dimension ordering) is not parsed here; instead the full-res plane size is
    /// derived from the tile-grid extent, and subsequent levels follow the Java
    /// halving rule.
    fn compute_levels(&mut self) {
        let ndim = self.n_dimensions as usize;
        let max_resolution = if self.use_pyramid {
            (self
                .tiles
                .iter()
                .filter_map(|(c, _, _)| c.last().copied())
                .max()
                .unwrap_or(0)
                + 1)
            .max(1) as usize
        } else {
            1
        };

        // Determine C/Z/T coordinate slots, porting the full Java collision-shift
        // heuristics (CellSensReader.java:1370-1444). The base slot for a logical
        // dimension is `tag + 2`; from there Java applies a series of fixups that
        // shift indices when they collide with the resolution slot or fall outside
        // the coordinate length. All tiles in one ETS volume share the same
        // coordinate length (== ndim) and the same `usePyramid`, so Java's per-tile
        // recomputation converges to a single result; we compute it once here.
        //
        // `tv`/`zv`/`cv` track the raw dimension tag (Java's dimOrder values), and
        // `t_index`/`z_index`/`c_index` are the corresponding coordinate slots
        // (can go negative, exactly as Java allows). `None` for the *v variables
        // mirrors Java's `null`.
        let len = ndim as i32;
        let mut tv: Option<i32> = self.dim_order.t;
        let mut zv: Option<i32> = self.dim_order.z;
        let cv: Option<i32> = self.dim_order.c;

        let mut t_index: i32 = tv.map_or(-1, |v| v + 2);
        let mut z_index: i32 = zv.map_or(-1, |v| v + 2);
        let mut c_index: i32 = cv.map_or(-1, |v| v + 2);

        // Slots that collide with the resolution slot (last) are not real axes
        // (CellSensReader.java:1381-1388). Only T and Z get this treatment.
        if self.use_pyramid && t_index == len - 1 {
            tv = None;
            t_index = -1;
        }
        if self.use_pyramid && z_index == len - 1 {
            zv = None;
            z_index = -1;
        }

        let upper_limit = if self.use_pyramid { len - 1 } else { len };
        // All three indices outside the valid range: shift them down by one and
        // push the shifted tag back into the ordering (CellSensReader.java:1391-1407).
        if (t_index < 0 || t_index >= upper_limit)
            && (z_index < 0 || z_index >= upper_limit)
            && (c_index < 0 || c_index >= upper_limit)
        {
            t_index -= 1;
            z_index -= 1;
            c_index -= 1;
            if self.dim_order.t.is_some() {
                self.dim_order.t = Some(t_index - 2);
            }
            if self.dim_order.z.is_some() {
                self.dim_order.z = Some(z_index - 2);
            }
            if self.dim_order.c.is_some() {
                self.dim_order.c = Some(c_index - 2);
            }
        }

        // No T and no Z ordering: infer C/T/Z slots from the coordinate length
        // (CellSensReader.java:1409-1444).
        if tv.is_none() && zv.is_none() {
            if len > 4 && cv.is_none() {
                c_index = 2;
                self.dim_order.c = Some(c_index - 2);
            }

            if len > 4 {
                if cv.is_none() {
                    t_index = 3;
                } else {
                    t_index = c_index + 2;
                }
                if t_index < len {
                    self.dim_order.t = Some(t_index - 2);
                } else {
                    t_index = -1;
                }
            }

            if len > 5 {
                if cv.is_none() {
                    z_index = 4;
                } else {
                    z_index = c_index + 1;
                }
                if z_index < len {
                    self.dim_order.z = Some(z_index - 2);
                } else {
                    z_index = -1;
                }
            }
        }

        // Translate final indices to optional slots; negative/out-of-range -> None.
        let to_slot = |i: i32| -> Option<usize> {
            if i >= 0 && i < len {
                Some(i as usize)
            } else {
                None
            }
        };
        self.dim_t = to_slot(t_index);
        self.dim_z = to_slot(z_index);
        self.dim_c = to_slot(c_index);

        let mut max_x = vec![0i32; max_resolution];
        let mut max_y = vec![0i32; max_resolution];
        let mut max_z = vec![0i32; max_resolution];
        let mut max_c = vec![0i32; max_resolution];
        let mut max_t = vec![0i32; max_resolution];

        for (coord, _, _) in &self.tiles {
            let res = if self.use_pyramid {
                coord.last().copied().unwrap_or(0).max(0) as usize
            } else {
                0
            };
            if res >= max_resolution {
                continue;
            }
            if coord[0] > max_x[res] {
                max_x[res] = coord[0];
            }
            if coord[1] > max_y[res] {
                max_y[res] = coord[1];
            }
            if let Some(ci) = self.dim_c {
                if ci < coord.len() && coord[ci] > max_c[res] {
                    max_c[res] = coord[ci];
                }
            }
            if let Some(ti) = self.dim_t {
                if ti < coord.len() && coord[ti] > max_t[res] {
                    max_t[res] = coord[ti];
                }
            }
            if let Some(zi) = self.dim_z {
                if zi < coord.len() && coord[zi] > max_z[res] {
                    max_z[res] = coord[zi];
                }
            }
        }

        // Level 0 (full resolution): exact size from the VSI `Pyramid` block when
        // available (CellSensReader.java:1463-1464: ms.sizeX = pyramid.width),
        // else the tile-grid extent.
        let mut levels = Vec::with_capacity(max_resolution);
        let cols0 = if max_x[0] >= 1 {
            (max_x[0] + 1) as u32
        } else {
            1
        };
        let rows0 = if max_y[0] >= 1 {
            (max_y[0] + 1) as u32
        } else {
            1
        };
        let base_c = self.size_c
            * if max_c[0] > 0 {
                (max_c[0] + 1) as u32
            } else {
                1
            };
        let size_x0 = self.pyramid_width.unwrap_or(cols0 * self.tile_x);
        let size_y0 = self.pyramid_height.unwrap_or(rows0 * self.tile_y);
        levels.push(EtsLevel {
            size_x: size_x0,
            size_y: size_y0,
            size_z: (max_z[0].max(0) + 1) as u32,
            size_c: base_c.max(1),
            size_t: (max_t[0].max(0) + 1) as u32,
            rows: rows0,
            cols: cols0,
        });

        for i in 1..max_resolution {
            let prev = levels[i - 1].clone();
            let cols = if max_x[i] >= 1 {
                (max_x[i] + 1) as u32
            } else {
                1
            };
            let rows = if max_y[i] >= 1 {
                (max_y[i] + 1) as u32
            } else {
                1
            };
            let max_size_x = self.tile_x * cols;
            let max_size_y = self.tile_y * rows;
            // Java halving rule (CellSensReader.java:1510-1523).
            let mut sx = prev.size_x / 2;
            if prev.size_x % 2 == 1 && sx < max_size_x {
                sx += 1;
            } else if sx > max_size_x {
                sx = max_size_x;
            }
            let mut sy = prev.size_y / 2;
            if prev.size_y % 2 == 1 && sy < max_size_y {
                sy += 1;
            } else if sy > max_size_y {
                sy = max_size_y;
            }
            let sc = self.size_c
                * if max_c[i] > 0 {
                    (max_c[i] + 1) as u32
                } else {
                    1
                };
            levels.push(EtsLevel {
                size_x: sx,
                size_y: sy,
                size_z: (max_z[i].max(0) + 1) as u32,
                size_c: sc.max(1),
                size_t: (max_t[i].max(0) + 1) as u32,
                rows,
                cols,
            });
        }
        self.levels = levels;
    }

    /// Maximum stored pixel extent at resolution 0, used for orphan-ETS matching
    /// (CellSensReader.java:1330-1339). Returns `(maxPixelWidth, maxPixelHeight)`,
    /// i.e. the tile-grid extent of the full-resolution level in pixels.
    fn max_pixel_extent(&self) -> (i64, i64) {
        let mut max_x = 0i32;
        let mut max_y = 0i32;
        for (coord, _, _) in &self.tiles {
            let at_res0 = !self.use_pyramid || coord.last().copied() == Some(0);
            if at_res0 {
                if coord.first().copied().unwrap_or(0) > max_x {
                    max_x = coord[0];
                }
                if coord.get(1).copied().unwrap_or(0) > max_y {
                    max_y = coord[1];
                }
            }
        }
        let w = (max_x as i64 + 1) * self.tile_x as i64;
        let h = (max_y as i64 + 1) * self.tile_y as i64;
        (w, h)
    }

    fn pixel_type(&self) -> Result<PixelType> {
        convert_ets_pixel_type(self.pixel_type_code)
    }

    /// RGB channel count: ETS stores all channels in one tile when sizeC > 1.
    fn rgb_channels(&self) -> u32 {
        self.size_c.max(1)
    }

    /// Byte length of one decoded tile.
    fn tile_size(&self) -> Result<usize> {
        let bpp = self.pixel_type()?.bytes_per_sample();
        bpp.checked_mul(self.rgb_channels() as usize)
            .and_then(|v| v.checked_mul(self.tile_x as usize))
            .and_then(|v| v.checked_mul(self.tile_y as usize))
            .ok_or_else(|| BioFormatsError::Format("cellSens ETS tile byte count overflows".into()))
    }

    /// Build the tile coordinate for (resolution, row, col, z, c, t) and look up
    /// its index in the tile map. Mirrors `decodeTile` coordinate construction
    /// (CellSensReader.java:1114-1141).
    fn find_tile(
        &self,
        resolution: usize,
        row: i32,
        col: i32,
        z: i32,
        c: i32,
        t: i32,
    ) -> Option<usize> {
        let ndim = self.n_dimensions as usize;
        let mut coord = vec![0i32; ndim];
        if ndim >= 1 {
            coord[0] = col;
        }
        if ndim >= 2 {
            coord[1] = row;
        }
        if let Some(ci) = self.dim_c {
            if ci < ndim {
                coord[ci] = c;
            }
        }
        if let Some(ti) = self.dim_t {
            if ti < ndim {
                coord[ti] = t;
            }
        }
        if let Some(zi) = self.dim_z {
            if zi < ndim {
                coord[zi] = z;
            }
        }
        if self.use_pyramid && ndim >= 1 {
            coord[ndim - 1] = resolution as i32;
        }
        self.tiles.iter().position(|(co, _, _)| co == &coord)
    }

    /// Decode one tile at (resolution,row,col,z,c,t), returning exactly
    /// `tile_size()` bytes. Missing tiles are filled with the background color
    /// (CellSensReader.java:1142-1155). Mirrors the codec dispatch in
    /// `decodeTile` (CellSensReader.java:1182-1212).
    fn decode_tile(
        &self,
        resolution: usize,
        row: i32,
        col: i32,
        z: i32,
        c: i32,
        t: i32,
    ) -> Result<Vec<u8>> {
        let tile_size = self.tile_size()?;
        let Some(index) = self.find_tile(resolution, row, col, z, c, t) else {
            // Fill with background color, like Java.
            let mut tile = vec![0u8; tile_size];
            if !self.background.is_empty() {
                let cl = self.background.len();
                let mut q = 0;
                while q + cl <= tile.len() {
                    tile[q..q + cl].copy_from_slice(&self.background);
                    q += cl;
                }
            }
            return Ok(tile);
        };

        let (_, offset, n_bytes) = self.tiles[index];
        // ETS chunk table byte counts define the exact stored tile payload.
        // RAW counts are validated during parsing; compressed tiles are decoded
        // from their declared codestream length.
        let read_len = n_bytes as usize;
        let mut reader = BufReader::new(File::open(&self.path).map_err(BioFormatsError::Io)?);
        let raw = read_bytes_at(&mut reader, offset, read_len)?;

        let mut buf = match self.compression {
            ETS_RAW => raw,
            ETS_JPEG => crate::common::codec::decompress_jpeg(&raw)?,
            ETS_JPEG_2000 => crate::common::codec::decompress_jpeg2000(&raw)?,
            ETS_JPEG_LOSSLESS => crate::common::codec::decompress_jpeg(&raw)?,
            // PNG/BMP tiles store a full image payload; decode in memory via the
            // codec helpers (CellSensReader.java:1198-1210, APNGReader/BMPReader).
            ETS_PNG => crate::common::codec::decompress_png(&raw)?,
            ETS_BMP => crate::common::codec::decompress_bmp(&raw)?,
            other => {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "cellSens ETS tile codec {other} is not supported"
                )))
            }
        };

        if buf.len() != tile_size {
            return Err(BioFormatsError::InvalidData(format!(
                "cellSens ETS tile decoded to {} bytes, expected {tile_size}",
                buf.len()
            )));
        }

        // BGR -> RGB swap for RAW component-order-1 multichannel tiles.
        if self.bgr && self.rgb_channels() >= 3 {
            let bpp = self.pixel_type()?.bytes_per_sample();
            let channels = self.rgb_channels() as usize;
            let pixel = bpp * channels;
            for px in buf.chunks_mut(pixel) {
                if px.len() == pixel {
                    for b in 0..bpp {
                        px.swap(b, 2 * bpp + b);
                    }
                }
            }
        }
        Ok(buf)
    }

    /// Assemble a full plane for the given resolution level and z/c/t plane
    /// coordinates by tiling. Mirrors `openBytes` tile-stitching
    /// (CellSensReader.java:533-598).
    fn assemble_plane(&self, resolution: usize, z: i32, c: i32, t: i32) -> Result<Vec<u8>> {
        let level = self
            .levels
            .get(resolution)
            .ok_or(BioFormatsError::PlaneOutOfRange(0))?;
        let bpp = self.pixel_type()?.bytes_per_sample();
        let channels = self.rgb_channels() as usize;
        let pixel = bpp * channels;
        let out_w = level.size_x as usize;
        let out_h = level.size_y as usize;
        let out_row_len = out_w * pixel;
        let mut out = vec![0u8; out_row_len * out_h];

        let width = self.tile_x as i64;
        let height = self.tile_y as i64;

        // Image region is the full plane [0,0,W,H]. Java shifts each tile by
        // tileOrigin / 2^resolution, intersects with the image rect, and copies
        // the intersecting rows into the output buffer in a compacting fashion
        // (CellSensReader.java:537-592). The row-band / column accumulation
        // (outputRow/outputCol) reproduces Java's `System.arraycopy` exactly.
        let img = (0i64, 0i64, out_w as i64, out_h as i64);
        let res_scale = 1i64 << resolution;
        let origin_x = self.tile_origin_x.map_or(0, |v| v as i64) / res_scale;
        let origin_y = self.tile_origin_y.map_or(0, |v| v as i64) / res_scale;

        let mut output_row: usize = 0;
        let mut output_col: usize = 0;
        for row in 0..level.rows {
            let mut last_height: Option<i64> = None;
            for col in 0..level.cols {
                // Tile placement in image coordinates, after the origin shift.
                let tx = col as i64 * width + origin_x;
                let ty = row as i64 * height + origin_y;
                // Intersection of [tx,tx+width) x [ty,ty+height) with the image.
                let ix0 = tx.max(img.0);
                let iy0 = ty.max(img.1);
                let ix1 = (tx + width).min(img.0 + img.2);
                let iy1 = (ty + height).min(img.1 + img.3);
                if ix1 <= ix0 || iy1 <= iy0 {
                    continue;
                }
                let inter_w = ix1 - ix0;
                let inter_h = iy1 - iy0;
                let intersection_x = if tx < img.0 { (img.0 - tx) as usize } else { 0 };

                let tile = self.decode_tile(resolution, row as i32, col as i32, z, c, t)?;
                let row_len = pixel * inter_w.min(width) as usize;

                let mut output_offset = output_row * out_row_len + output_col;
                for trow in 0..inter_h {
                    let real_row = (trow + iy0 - ty) as usize;
                    let input_offset = pixel * (real_row * width as usize + intersection_x);
                    if input_offset + row_len <= tile.len() && output_offset + row_len <= out.len()
                    {
                        out[output_offset..output_offset + row_len]
                            .copy_from_slice(&tile[input_offset..input_offset + row_len]);
                    }
                    output_offset += out_row_len;
                }
                output_col += row_len;
                last_height = Some(inter_h);
            }
            if let Some(h) = last_height {
                output_row += h as usize;
                output_col = 0;
            }
        }
        Ok(out)
    }

    /// Per-level image metadata.
    fn level_metadata(&self, resolution: usize) -> Result<ImageMetadata> {
        let level = self
            .levels
            .get(resolution)
            .ok_or(BioFormatsError::PlaneOutOfRange(resolution as u32))?;
        let pt = self.pixel_type()?;
        let channels = self.rgb_channels();
        let image_count = level.size_z * level.size_t * (level.size_c / channels.max(1)).max(1);
        let mut meta = ImageMetadata {
            size_x: level.size_x,
            size_y: level.size_y,
            size_z: level.size_z.max(1),
            size_c: level.size_c.max(1),
            size_t: level.size_t.max(1),
            pixel_type: pt,
            bits_per_pixel: (pt.bytes_per_sample() * 8) as u8,
            image_count: image_count.max(1),
            dimension_order: crate::common::metadata::DimensionOrder::XYCZT,
            is_rgb: channels > 1,
            is_interleaved: channels > 1,
            is_indexed: false,
            // Java: ms.littleEndian = compressionType.get(index) == RAW
            // (CellSensReader.java:800). Compressed tiles (JPEG/JPEG2000/etc.)
            // report littleEndian = false.
            is_little_endian: self.compression == ETS_RAW,
            resolution_count: 1,
            ..ImageMetadata::default()
        };
        insert_cellsens_acquisition_metadata(&mut meta.series_metadata, "cellsens.ets", &self.meta);
        Ok(meta)
    }
}

fn insert_cellsens_acquisition_metadata(
    sm: &mut HashMap<String, MetadataValue>,
    prefix: &str,
    m: &VsiPyramidMeta,
) {
    let strs: [(&str, Option<&String>); 6] = [
        ("device_name", m.device_names.first()),
        ("device_id", m.device_ids.first()),
        ("device_subtype", m.device_subtypes.first()),
        ("device_manufacturer", m.device_manufacturers.first()),
        ("objective_name", m.objective_names.first()),
        ("stack_type", m.stack_type.as_ref()),
    ];
    for (key, val) in strs {
        if let Some(val) = val {
            if !val.is_empty() {
                sm.insert(
                    format!("{prefix}.{key}"),
                    MetadataValue::String(val.clone()),
                );
            }
        }
    }
    let floats: [(&str, Option<f64>); 12] = [
        ("objective_magnification", m.magnification),
        ("numerical_aperture", m.numerical_aperture),
        ("working_distance", m.working_distance),
        ("refractive_index", m.refractive_index),
        ("camera_gain", m.gain),
        ("camera_offset", m.offset),
        ("red_gain", m.red_gain),
        ("green_gain", m.green_gain),
        ("blue_gain", m.blue_gain),
        ("red_offset", m.red_offset),
        ("green_offset", m.green_offset),
        ("blue_offset", m.blue_offset),
    ];
    for (key, val) in floats {
        if let Some(x) = val {
            sm.insert(format!("{prefix}.{key}"), MetadataValue::Float(x));
        }
    }
    let ints: [(&str, Option<i64>); 5] = [
        ("bit_depth", m.bit_depth),
        ("binning_x", m.binning_x),
        ("binning_y", m.binning_y),
        ("acquisition_time", m.acquisition_time),
        ("exposure_time", m.exposure_times.first().copied()),
    ];
    for (key, val) in ints {
        if let Some(x) = val {
            sm.insert(format!("{prefix}.{key}"), MetadataValue::Int(x));
        }
    }

    let float_lists: [(&str, &Vec<f64>); 3] = [
        ("channel_wavelength", &m.channel_wavelengths),
        ("z_value", &m.z_values),
        ("timestamp", &m.t_values),
    ];
    for (key, list) in float_lists {
        for (idx, x) in list.iter().enumerate() {
            sm.insert(format!("{prefix}.{key}.{idx}"), MetadataValue::Float(*x));
        }
    }
    if let Some(x) = m.z_start {
        sm.insert(format!("{prefix}.z_start"), MetadataValue::Float(x));
    }
    if let Some(x) = m.z_increment {
        sm.insert(format!("{prefix}.z_increment"), MetadataValue::Float(x));
    }
    for (idx, name) in m.channel_names.iter().enumerate() {
        if !name.is_empty() {
            sm.insert(
                format!("{prefix}.channel_name.{idx}"),
                MetadataValue::String(name.clone()),
            );
        }
    }
    if let Some(name) = &m.name {
        sm.insert(
            format!("{prefix}.stack_name"),
            MetadataValue::String(name.clone()),
        );
    }
    if let Some(x) = m.default_exposure_time {
        sm.insert(
            format!("{prefix}.default_exposure_time"),
            MetadataValue::Int(x),
        );
    }
    for (idx, x) in m.other_exposure_times.iter().enumerate() {
        sm.insert(
            format!("{prefix}.other_exposure_time.{idx}"),
            MetadataValue::Int(*x),
        );
    }
    for (idx, x) in m.objective_types.iter().enumerate() {
        sm.insert(
            format!("{prefix}.objective_type.{idx}"),
            MetadataValue::Int(*x),
        );
    }
}

// ---- VSI proprietary tag-tree parser (CellSensReader.java:1589-2079) --------
//
// The base `.vsi` is a TIFF whose first IFD also points (at byte offset 8) to a
// proprietary tag-tree describing each `Pyramid` (image) block: its exact
// full-resolution width/height (IMAGE_BOUNDARY), the tile-origin crop
// (TILE_ORIGIN) and the canonical dimension ordering. This is a focused port of
// the tree walk that collects only those geometry fields; the large body of
// per-device acquisition metadata tags is captured below into `VsiPyramidMeta`
// and mirrored into both overview summary metadata and ETS logical series.

// Real field types (CellSensReader.java:80-126).
const VSI_CHAR: i32 = 1;
const VSI_UCHAR: i32 = 2;
const VSI_SHORT: i32 = 3;
const VSI_USHORT: i32 = 4;
const VSI_INT: i32 = 5;
const VSI_UINT: i32 = 6;
const VSI_LONG: i32 = 7;
const VSI_ULONG: i32 = 8;
const VSI_FLOAT: i32 = 9;
const VSI_DOUBLE: i32 = 10;
const VSI_BOOLEAN: i32 = 12;
const VSI_TCHAR: i32 = 13;
const VSI_DWORD: i32 = 14;
const VSI_TIMESTAMP: i32 = 17;
const VSI_DATE: i32 = 18;
const VSI_FIELD_TYPE: i32 = 271;
const VSI_MEM_MODEL: i32 = 272;
const VSI_COLOR_SPACE: i32 = 273;
const VSI_UNICODE_TCHAR: i32 = 8192;
const VSI_RGB: i32 = 269;
const VSI_BGR: i32 = 270;

// Volume / structural field types (CellSensReader.java:129-132).
const VSI_NEW_VOLUME_HEADER: i32 = 0;
const VSI_PROPERTY_SET_VOLUME: i32 = 1;
const VSI_NEW_MDIM_VOLUME_HEADER: i32 = 2;

// Tags (CellSensReader.java:139-303).
const VSI_IMAGE_FRAME_VOLUME: i32 = 2002;
const VSI_DIMENSION_DESCRIPTION_VOLUME: i32 = 2007;
const VSI_CHANNEL_PROPERTIES: i32 = 2008;
const VSI_EXTERNAL_FILE_PROPERTIES: i32 = 2018;
const VSI_DOCUMENT_PROPERTIES: i32 = 2109;
const VSI_SLIDE_PROPERTIES: i32 = 2452;
const VSI_IMAGE_BOUNDARY: i32 = 2053;
const VSI_TILE_ORIGIN: i32 = 2410;
// RWC_FRAME_SCALE: physical pixel size (doubleValues[0]/[1]) in micrometres
// (CellSensReader.java:300, 1853-1858).
const VSI_RWC_FRAME_SCALE: i32 = 2019;
const VSI_HAS_EXTERNAL_FILE: i32 = 20005;
const VSI_Z_START: i32 = 2012;
const VSI_TIME_START: i32 = 2100;
const VSI_DIMENSION_VALUE_ID: i32 = 2027;
const VSI_LAMBDA_START: i32 = 2039;
const VSI_DIMENSION_MEANING: i32 = 2023;

// Non-geometry metadata tags (CellSensReader.java:139-376). Captured into the
// pyramid's metadata for inclusion in series_metadata (CellSensReader.java:1881-1989).
const VSI_EXPOSURE_TIME: i32 = 100002;
const VSI_CAMERA_GAIN: i32 = 100003;
const VSI_CAMERA_OFFSET: i32 = 100004;
const VSI_RED_GAIN: i32 = 100007;
const VSI_GREEN_GAIN: i32 = 100008;
const VSI_BLUE_GAIN: i32 = 100009;
const VSI_RED_OFFSET: i32 = 100010;
const VSI_GREEN_OFFSET: i32 = 100011;
const VSI_BLUE_OFFSET: i32 = 100012;
const VSI_X_BINNING: i32 = 100015;
const VSI_Y_BINNING: i32 = 100016;
const VSI_BIT_DEPTH: i32 = 100049;
const VSI_STACK_TYPE: i32 = 2074;
// Prefix-gated VALUE metadata and the volume tags that build the tag-name prefix
// (CellSensReader.java:1899-1979, 2081-2108).
const VSI_VALUE: i32 = 268435458;
const VSI_Z_INCREMENT: i32 = 2013;
const VSI_Z_VALUE: i32 = 2014;
const VSI_TIME_VALUE: i32 = 2017;
const VSI_CHANNEL_NAME: i32 = 2419;
const VSI_STACK_NAME: i32 = 2030;
const VSI_OPTICAL_PATH: i32 = 2043;
const VSI_CALIBRATION: i32 = 20051;
// Volume tags whose getVolumeName(tag) yields the empty (structural) prefix
// (CellSensReader.java:2083-2094).
const VSI_COLLECTION_VOLUME: i32 = 2000;
const VSI_MULTIDIM_IMAGE_VOLUME: i32 = 2001;
const VSI_DIMENSION_SIZE: i32 = 2003;
const VSI_IMAGE_COLLECTION_PROPERTIES: i32 = 2004;
const VSI_MULTIDIM_STACK_PROPERTIES: i32 = 2005;
const VSI_FRAME_PROPERTIES: i32 = 2006;
const VSI_DISPLAY_MAPPING_VOLUME: i32 = 2011;
const VSI_LAYER_INFO_PROPERTIES: i32 = 2012;
// Volume tag 2417 maps to the "Channel Wavelength " prefix (CellSensReader.java:2097).
const VSI_CHANNEL_WAVELENGTH_VOLUME: i32 = 2417;
const VSI_OBJECTIVE_MAG: i32 = 120060;
const VSI_NUMERICAL_APERTURE: i32 = 120061;
const VSI_WORKING_DISTANCE: i32 = 120062;
const VSI_OBJECTIVE_NAME: i32 = 120063;
const VSI_OBJECTIVE_TYPE: i32 = 120064;
const VSI_REFRACTIVE_INDEX: i32 = 120079;
const VSI_DEVICE_NAME: i32 = 120116;
const VSI_DEVICE_ID: i32 = 120129;
const VSI_DEVICE_SUBTYPE: i32 = 120130;
const VSI_DEVICE_MANUFACTURER: i32 = 120133;
const VSI_CREATION_TIME: i32 = 2015;

// DIMENSION_MEANING enum values (CellSensReader.java:285-290).
const VSI_DIM_Z: i64 = 1;
const VSI_DIM_T: i64 = 2;
const VSI_DIM_LAMBDA: i64 = 3;
const VSI_DIM_C: i64 = 4;

/// Stateful walk over the VSI metadata tag-tree. Ported (focused) from
/// `CellSensReader.readTags` (CellSensReader.java:1589-2079).
struct VsiTagParser<'a> {
    data: &'a [u8],
    pyramids: Vec<VsiPyramid>,
    metadata_index: i32,
    previous_tag: i32,
    in_dimension_properties: bool,
    dimension_tag: i32,
    found_channel_tag: bool,
    /// last stored leaf value as a string (for DIMENSION_MEANING parsing).
    stored_value: Option<String>,
    expect_ets: bool,
    /// recursion guard.
    depth: u32,
}

impl<'a> VsiTagParser<'a> {
    fn new(data: &'a [u8]) -> Self {
        VsiTagParser {
            data,
            pyramids: Vec::new(),
            metadata_index: -1,
            previous_tag: 0,
            in_dimension_properties: false,
            dimension_tag: 0,
            found_channel_tag: false,
            stored_value: None,
            expect_ets: false,
            depth: 0,
        }
    }

    fn len(&self) -> i64 {
        self.data.len() as i64
    }
    fn rd(&self, off: i64, n: usize) -> Option<&[u8]> {
        if off < 0 {
            return None;
        }
        self.data.get(off as usize..off as usize + n)
    }
    fn i16(&self, off: i64) -> i16 {
        self.rd(off, 2)
            .map_or(0, |b| i16::from_le_bytes([b[0], b[1]]))
    }
    fn i32(&self, off: i64) -> i32 {
        self.rd(off, 4)
            .map_or(0, |b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn i64(&self, off: i64) -> i64 {
        self.rd(off, 8).map_or(0, |b| {
            i64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
        })
    }

    /// Walk one tag container starting at byte offset `fp`. Mirrors `readTags`.
    /// Returns the file pointer reached, so a parent NEW_VOLUME_HEADER loop can
    /// advance through successive child containers (CellSensReader.java:1685-1695).
    fn read_tags(&mut self, fp: i64, populate: bool, tag_prefix: &str) -> i64 {
        if self.depth > 64 {
            return fp;
        }
        self.depth += 1;
        let end = self.read_tags_inner(fp, populate, tag_prefix);
        self.depth -= 1;
        end
    }

    /// Map a volume tag to the tag-name prefix it pushes onto descendants.
    /// Ported from `getVolumeName` (CellSensReader.java:2081-2108).
    fn volume_name(tag: i32) -> &'static str {
        match tag {
            VSI_COLLECTION_VOLUME
            | VSI_MULTIDIM_IMAGE_VOLUME
            | VSI_IMAGE_FRAME_VOLUME
            | VSI_DIMENSION_SIZE
            | VSI_IMAGE_COLLECTION_PROPERTIES
            | VSI_MULTIDIM_STACK_PROPERTIES
            | VSI_FRAME_PROPERTIES
            | VSI_DIMENSION_DESCRIPTION_VOLUME
            | VSI_CHANNEL_PROPERTIES
            | VSI_DISPLAY_MAPPING_VOLUME
            | VSI_LAYER_INFO_PROPERTIES => "",
            VSI_OPTICAL_PATH => "Microscope ",
            VSI_CHANNEL_WAVELENGTH_VOLUME => "Channel Wavelength ",
            VSI_WORKING_DISTANCE => "Objective Working Distance ",
            VSI_TIME_VALUE => "Timestamp ",
            VSI_CALIBRATION => "Calibration Function ",
            _ => "",
        }
    }

    fn read_tags_inner(&mut self, container_fp: i64, _populate: bool, tag_prefix: &str) -> i64 {
        if container_fp + 24 >= self.len() {
            return container_fp;
        }
        // 24-byte container header.
        let _header_size = self.i16(container_fp) as i32;
        let _version = self.i16(container_fp + 2) as i32;
        let _volume_version = self.i32(container_fp + 4);
        let data_field_offset = self.i64(container_fp + 8);
        let flags = self.i32(container_fp + 16);
        // container_fp + 20 .. 24: skipped reserved bytes
        let tag_count = (flags & 0x0fff_ffff) as i64;
        if container_fp + data_field_offset < 0 {
            return container_fp;
        }
        let mut fp = container_fp + data_field_offset;
        if fp >= self.len() || tag_count > self.len() {
            return fp;
        }

        for _ in 0..tag_count {
            if fp + 16 >= self.len() {
                break;
            }
            let field_type = self.i32(fp);
            let tag = self.i32(fp + 4);
            let next_field = (self.i32(fp + 8) as u32) as i64;
            let data_size = self.i32(fp + 12);
            // After the 16-byte field record (+optional secondTag).
            let mut cur = fp + 16;

            let extra_tag = ((field_type & 0x0800_0000) >> 27) == 1;
            let extended_field = ((field_type & 0x1000_0000) >> 28) == 1;
            let inline_data = ((field_type & 0x4000_0000) >> 30) == 1;
            let real_type = field_type & 0x00ff_ffff;

            let mut second_tag = -1;
            if extra_tag {
                second_tag = self.i32(cur);
                cur += 4;
            }

            if tag < 0 {
                return fp;
            }

            if tag == VSI_EXTERNAL_FILE_PROPERTIES && self.previous_tag == VSI_IMAGE_FRAME_VOLUME {
                self.metadata_index += 1;
            } else if tag == VSI_DOCUMENT_PROPERTIES || tag == VSI_SLIDE_PROPERTIES {
                self.metadata_index = -1;
            }
            self.previous_tag = tag;

            while self.metadata_index >= self.pyramids.len() as i32 {
                self.pyramids.push(VsiPyramid::default());
            }

            if extended_field && real_type == VSI_NEW_VOLUME_HEADER {
                if tag == VSI_DIMENSION_DESCRIPTION_VOLUME {
                    self.dimension_tag = second_tag;
                    self.in_dimension_properties = true;
                }
                // Child prefix is getVolumeName(tag) (CellSensReader.java:1690).
                let child_prefix = Self::volume_name(tag);
                let end_pointer = cur + data_size as i64;
                let mut child = cur;
                while child < end_pointer && child < self.len() {
                    let start = child;
                    let end = self.read_tags(child, true, child_prefix);
                    // Mirror Java's start >= end guard (CellSensReader.java:1692).
                    if end <= start {
                        break;
                    }
                    child = end;
                }
                if tag == VSI_DIMENSION_DESCRIPTION_VOLUME {
                    self.in_dimension_properties = false;
                    self.found_channel_tag = false;
                }
            } else if extended_field
                && (real_type == VSI_PROPERTY_SET_VOLUME || real_type == VSI_NEW_MDIM_VOLUME_HEADER)
            {
                // Child prefix: getVolumeName(tag) for NEW_MDIM, else inherit the
                // current tagPrefix (CellSensReader.java:1704-1720). When the MDIM
                // volume yields an empty name, the Z_* tags get a literal fallback.
                let mut child_prefix: String = if real_type == VSI_NEW_MDIM_VOLUME_HEADER {
                    Self::volume_name(tag).to_string()
                } else {
                    tag_prefix.to_string()
                };
                if child_prefix.is_empty() && real_type == VSI_NEW_MDIM_VOLUME_HEADER {
                    match tag {
                        VSI_Z_START => child_prefix = "Z start position".to_string(),
                        VSI_Z_INCREMENT => child_prefix = "Z increment".to_string(),
                        VSI_Z_VALUE => child_prefix = "Z value".to_string(),
                        _ => {}
                    }
                }
                self.read_tags(cur, tag != 2037, &child_prefix);
            } else {
                // Leaf field: read the value for the types we care about.
                let mut value: Option<String> = None;
                if !inline_data && data_size > 0 {
                    value = self.read_leaf_value(real_type, cur, data_size, tag);
                }
                if let Some(v) = &value {
                    self.stored_value = Some(v.clone());
                }
                if tag == VSI_HAS_EXTERNAL_FILE {
                    if let Some(v) = &value {
                        if v.trim() == "1" {
                            self.expect_ets = true;
                        }
                    }
                }
                // Non-geometry acquisition metadata (CellSensReader.java:1881-1979).
                if self.metadata_index >= 0 {
                    if let Some(v) = &value {
                        self.capture_metadata(tag, v, tag_prefix);
                    }
                }
            }

            // Dimension ordering (CellSensReader.java:2013-2061).
            if self.in_dimension_properties && self.metadata_index >= 0 {
                let dtag = self.dimension_tag;
                let idx = self.metadata_index as usize;
                let p = &mut self.pyramids[idx];
                if tag == VSI_Z_START && !p.dim_order.contains_value(dtag) {
                    p.dim_order.z = Some(dtag);
                } else if (tag == VSI_TIME_START || tag == VSI_DIMENSION_VALUE_ID)
                    && !p.dim_order.contains_value(dtag)
                {
                    p.dim_order.t = Some(dtag);
                } else if tag == VSI_LAMBDA_START && !p.dim_order.contains_value(dtag) {
                    p.dim_order.l = Some(dtag);
                } else if tag == VSI_CHANNEL_PROPERTIES
                    && self.found_channel_tag
                    && !p.dim_order.contains_value(dtag)
                {
                    p.dim_order.c = Some(dtag);
                } else if tag == VSI_CHANNEL_PROPERTIES {
                    self.found_channel_tag = true;
                } else if tag == VSI_DIMENSION_MEANING {
                    if let Some(sv) = &self.stored_value {
                        if let Ok(dim) = sv.trim().parse::<i64>() {
                            match dim {
                                VSI_DIM_Z => p.dim_order.z = Some(dtag),
                                VSI_DIM_T => p.dim_order.t = Some(dtag),
                                VSI_DIM_LAMBDA => p.dim_order.l = Some(dtag),
                                VSI_DIM_C => p.dim_order.c = Some(dtag),
                                _ => {}
                            }
                        }
                    }
                }
            }

            // Navigation (CellSensReader.java:2063-2073). Both the sibling jump
            // and the terminating resume are RELATIVE TO THE CONTAINER BASE
            // `container_fp` (Java keeps `fp` constant at the container header and
            // re-seeks to `fp + nextField`), not relative to the current field.
            if next_field == 0 || tag == -494804095 {
                // Java: if (fp + dataSize + 32 < length && fp + dataSize >= 0)
                //         seek(fp + dataSize + 32);
                let resume = container_fp + data_size as i64 + 32;
                if resume < self.len() && container_fp + data_size as i64 >= 0 {
                    return resume;
                }
                return fp + 16;
            }
            if container_fp + next_field < self.len() && container_fp + next_field >= 0 {
                fp = container_fp + next_field;
            } else {
                break;
            }
        }
        fp
    }

    /// Capture non-geometry acquisition metadata for the current pyramid.
    /// Mirrors the metadata dispatch in `readTags` (CellSensReader.java:1881-1979).
    ///
    /// `tag_prefix` is the recursive tag-name prefix accumulated while descending
    /// volumes (CellSensReader.java:getVolumeName/tagPrefix); it gates the
    /// EXPOSURE_TIME split and the generic VALUE tag (channel wavelengths, Z
    /// start/increment/value, timestamps, working distance).
    fn capture_metadata(&mut self, tag: i32, value: &str, tag_prefix: &str) {
        let idx = self.metadata_index as usize;
        if idx >= self.pyramids.len() {
            return;
        }
        let m = &mut self.pyramids[idx].meta;
        let v = value.trim();
        let as_i64 = || v.parse::<i64>().ok();
        let as_f64 = || v.parse::<f64>().ok();
        match tag {
            VSI_DEVICE_NAME => m.device_names.push(v.to_string()),
            VSI_DEVICE_ID => m.device_ids.push(v.to_string()),
            VSI_DEVICE_SUBTYPE => m.device_subtypes.push(v.to_string()),
            VSI_DEVICE_MANUFACTURER => m.device_manufacturers.push(v.to_string()),
            VSI_OBJECTIVE_NAME => m.objective_names.push(v.to_string()),
            VSI_OBJECTIVE_TYPE => {
                if let Some(n) = as_i64() {
                    m.objective_types.push(n);
                }
            }
            // EXPOSURE_TIME split by prefix (CellSensReader.java:1899-1905):
            // empty prefix -> exposureTimes; otherwise defaultExposureTime +
            // otherExposureTimes.
            VSI_EXPOSURE_TIME => {
                if let Some(n) = as_i64() {
                    if tag_prefix.is_empty() {
                        m.exposure_times.push(n);
                    } else {
                        m.default_exposure_time = Some(n);
                        m.other_exposure_times.push(n);
                    }
                }
            }
            // Generic VALUE tag, disambiguated entirely by the tag prefix
            // (CellSensReader.java:1960-1979).
            VSI_VALUE => {
                if tag_prefix == "Channel Wavelength " {
                    if let Some(x) = as_f64() {
                        m.channel_wavelengths.push(x);
                    }
                } else if tag_prefix.starts_with("Objective Working Distance") {
                    m.working_distance = as_f64();
                } else if tag_prefix == "Z start position" {
                    m.z_start = as_f64();
                } else if tag_prefix == "Z increment" {
                    m.z_increment = as_f64();
                } else if tag_prefix == "Z value" {
                    if let Some(x) = as_f64() {
                        m.z_values.push(x);
                    }
                } else if tag_prefix == "Timestamp " {
                    if let Some(x) = as_f64() {
                        m.t_values.push(x);
                    }
                }
            }
            VSI_OBJECTIVE_MAG => m.magnification = as_f64(),
            VSI_NUMERICAL_APERTURE => m.numerical_aperture = as_f64(),
            VSI_WORKING_DISTANCE => m.working_distance = as_f64(),
            VSI_REFRACTIVE_INDEX => m.refractive_index = as_f64(),
            VSI_BIT_DEPTH => m.bit_depth = as_i64(),
            VSI_X_BINNING => m.binning_x = as_i64(),
            VSI_Y_BINNING => m.binning_y = as_i64(),
            VSI_CAMERA_GAIN => m.gain = as_f64(),
            VSI_CAMERA_OFFSET => m.offset = as_f64(),
            VSI_RED_GAIN => m.red_gain = as_f64(),
            VSI_GREEN_GAIN => m.green_gain = as_f64(),
            VSI_BLUE_GAIN => m.blue_gain = as_f64(),
            VSI_RED_OFFSET => m.red_offset = as_f64(),
            VSI_GREEN_OFFSET => m.green_offset = as_f64(),
            VSI_BLUE_OFFSET => m.blue_offset = as_f64(),
            VSI_STACK_TYPE => m.stack_type = Some(v.to_string()),
            VSI_CREATION_TIME => {
                if m.acquisition_time.is_none() {
                    m.acquisition_time = as_i64();
                }
            }
            _ => {}
        }
    }

    /// Read a leaf value, filling pyramid geometry for the tags of interest and
    /// returning a string form for the value (used by DIMENSION_MEANING).
    fn read_leaf_value(
        &mut self,
        real_type: i32,
        off: i64,
        data_size: i32,
        tag: i32,
    ) -> Option<String> {
        match real_type {
            VSI_CHAR | VSI_UCHAR => Some((self.rd(off, 1).map(|b| b[0]).unwrap_or(0)).to_string()),
            VSI_SHORT | VSI_USHORT => Some(self.i16(off).to_string()),
            VSI_INT | VSI_UINT | VSI_DWORD | VSI_FIELD_TYPE | VSI_MEM_MODEL | VSI_COLOR_SPACE => {
                Some(self.i32(off).to_string())
            }
            VSI_LONG | VSI_ULONG | VSI_TIMESTAMP => Some(self.i64(off).to_string()),
            VSI_FLOAT => {
                let b = self.rd(off, 4)?;
                Some(f32::from_le_bytes([b[0], b[1], b[2], b[3]]).to_string())
            }
            VSI_DOUBLE | VSI_DATE => {
                let b = self.rd(off, 8)?;
                Some(
                    f64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
                        .to_string(),
                )
            }
            VSI_BOOLEAN => Some((self.rd(off, 1).map(|b| b[0]).unwrap_or(0) != 0).to_string()),
            VSI_TCHAR | VSI_UNICODE_TCHAR => {
                let n = data_size.max(0) as usize;
                let bytes = self.rd(off, n)?;
                let s = String::from_utf8_lossy(bytes)
                    .replace('\0', "")
                    .trim()
                    .to_string();
                // CHANNEL_NAME / STACK_NAME are captured straight from the string
                // leaf (CellSensReader.java:1769-1778).
                if self.metadata_index >= 0 {
                    let m = &mut self.pyramids[self.metadata_index as usize].meta;
                    if tag == VSI_CHANNEL_NAME {
                        m.channel_names.push(s.clone());
                    } else if tag == VSI_STACK_NAME && s != "0" && m.name.is_none() {
                        m.name = Some(s.clone());
                    }
                }
                Some(s)
            }
            VSI_RGB | VSI_BGR => None,
            // INT array family (256..=277, 8195/8199/8200, 8470). These carry
            // IMAGE_BOUNDARY (width/height) and TILE_ORIGIN.
            256..=259 | 267 | 274..=277 | 8195 | 8199 | 8200 | 8470 => {
                let n_values = (data_size / 4).max(0) as usize;
                let mut vals = Vec::with_capacity(n_values);
                for v in 0..n_values {
                    vals.push(self.i32(off + (v * 4) as i64));
                }
                if tag == VSI_IMAGE_BOUNDARY && vals.len() >= 4 && self.metadata_index >= 0 {
                    let p = &mut self.pyramids[self.metadata_index as usize];
                    if p.width.is_none() {
                        // intValues[2]/[3] (CellSensReader.java:1812-1814).
                        if vals[2] > 0 {
                            p.width = Some(vals[2] as u32);
                        }
                        if vals[3] > 0 {
                            p.height = Some(vals[3] as u32);
                        }
                    }
                } else if tag == VSI_TILE_ORIGIN && vals.len() >= 2 && self.metadata_index >= 0 {
                    let p = &mut self.pyramids[self.metadata_index as usize];
                    p.tile_origin_x = Some(vals[0]);
                    p.tile_origin_y = Some(vals[1]);
                }
                Some(format!("{vals:?}"))
            }
            // DOUBLE array family (260..=266, 268, 279, 280).
            260..=266 | 268 | 279 | 280 => {
                let n_values = (data_size / 8).max(0) as usize;
                let mut vals = Vec::with_capacity(n_values);
                for v in 0..n_values {
                    let b = self.rd(off + (v * 8) as i64, 8)?;
                    vals.push(f64::from_le_bytes([
                        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
                    ]));
                }
                // RWC_FRAME_SCALE carries the physical pixel size
                // (CellSensReader.java:1853-1858).
                if tag == VSI_RWC_FRAME_SCALE && vals.len() >= 2 && self.metadata_index >= 0 {
                    let p = &mut self.pyramids[self.metadata_index as usize];
                    if p.physical_size_x.is_none() {
                        p.physical_size_x = Some(vals[0]);
                        p.physical_size_y = Some(vals[1]);
                    }
                }
                Some(format!("{vals:?}"))
            }
            _ => None,
        }
    }
}

impl VsiDimOrder {
    fn contains_value(&self, tag: i32) -> bool {
        self.z == Some(tag) || self.t == Some(tag) || self.c == Some(tag) || self.l == Some(tag)
    }
}

impl CellSensReader {
    pub fn new() -> Self {
        CellSensReader {
            inner: crate::tiff::TiffReader::new(),
            ets: Vec::new(),
            tiff_series: 0,
            target: CellSensTarget::Tiff(0),
            ets_meta: None,
            series_map: Vec::new(),
            series_names: Vec::new(),
            series_phys: Vec::new(),
            current: 0,
            vsi_path: None,
        }
    }

    /// Parse the proprietary VSI tag-tree (from byte offset 8) and return the
    /// ordered `Pyramid` blocks. Mirrors `initFile`'s `readTags(vsi, false, "")`
    /// call (CellSensReader.java:684-685).
    fn parse_vsi_pyramids(vsi_path: &Path) -> Vec<VsiPyramid> {
        let Ok(bytes) = std::fs::read(vsi_path) else {
            return Vec::new();
        };
        let mut parser = VsiTagParser::new(&bytes);
        // initFile calls readTags(vsi, false, "") (CellSensReader.java:684-685).
        parser.read_tags(8, false, "");
        parser.pyramids
    }

    /// Locate `frame_*.ets` files in the `_<name>_/<stack>/` pixel directories
    /// next to the `.vsi`. Mirrors the directory walk in `initFile`.
    fn find_ets_files(vsi_path: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let Some(dir) = vsi_path.parent() else {
            return out;
        };
        let stem = vsi_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        let pixels_dir = dir.join(format!("_{}_", stem));
        let Ok(stacks) = std::fs::read_dir(&pixels_dir) else {
            return out;
        };
        let mut stack_dirs: Vec<PathBuf> = stacks
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.is_dir())
            .collect();
        stack_dirs.sort();
        for stack in stack_dirs {
            if let Ok(files) = std::fs::read_dir(&stack) {
                let mut paths: Vec<PathBuf> =
                    files.filter_map(|e| e.ok().map(|e| e.path())).collect();
                paths.sort();
                for p in paths {
                    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
                    if name.starts_with("frame_") && name.to_ascii_lowercase().ends_with(".ets") {
                        out.push(p);
                    }
                }
            }
        }
        out
    }

    /// Parse one ETS file's volume header and tile index. Mirrors `parseETSFile`.
    /// ETS is always little-endian.
    fn parse_ets(path: &Path) -> Result<EtsVolume> {
        let bytes = std::fs::read(path).map_err(BioFormatsError::Io)?;
        let rd = |off: usize, n: usize| -> Result<&[u8]> {
            let end = off.checked_add(n).ok_or_else(|| {
                BioFormatsError::Format(format!("ETS file {:?}: header offset overflows", path))
            })?;
            bytes.get(off..end).ok_or_else(|| {
                BioFormatsError::Format(format!(
                    "ETS file {:?}: truncated header/table at offset {off}",
                    path
                ))
            })
        };
        let u32_at = |off: usize| -> Result<u32> {
            rd(off, 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        };
        let i32_at = |off: usize| -> Result<i32> { Ok(u32_at(off)? as i32) };
        let u64_at = |off: usize| -> Result<u64> {
            rd(off, 8).map(|b| u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
        };

        // Volume header (offset 0): "SIS\0" magic, then ints/longs. The 4-byte
        // tag is NUL-padded ("SIS\0"); strip trailing NULs as well as whitespace.
        let magic = String::from_utf8_lossy(rd(0, 4)?)
            .trim_matches(|c: char| c.is_whitespace() || c == '\0')
            .to_string();
        if magic != "SIS" {
            return Err(BioFormatsError::Format(format!(
                "ETS file {:?}: unexpected magic {:?}",
                path, magic
            )));
        }
        // headerSize(4) version(8) nDimensions(12) addHeaderOffset(16, long)
        // addHeaderSize(24) reserved(28) usedChunkOffset(32, long) nUsedChunks(40)
        let n_dimensions = u32_at(12)?;
        if !(2..=16).contains(&n_dimensions) {
            return Err(BioFormatsError::Format(format!(
                "ETS file {:?}: unsupported dimension count {n_dimensions}",
                path
            )));
        }
        let additional_header_offset = usize::try_from(u64_at(16)?).map_err(|_| {
            BioFormatsError::Format(format!(
                "ETS file {:?}: additional header offset overflows",
                path
            ))
        })?;
        let used_chunk_offset = usize::try_from(u64_at(32)?).map_err(|_| {
            BioFormatsError::Format(format!("ETS file {:?}: used chunk offset overflows", path))
        })?;
        let n_used_chunks = u32_at(40)? as usize;
        if n_used_chunks == 0 {
            return Err(BioFormatsError::Format(format!(
                "ETS file {:?}: chunk table must contain at least one tile",
                path
            )));
        }

        // Additional header (additionalHeaderOffset): "ETS\0" magic (NUL-padded).
        let more_magic = String::from_utf8_lossy(rd(additional_header_offset, 4)?)
            .trim_matches(|c: char| c.is_whitespace() || c == '\0')
            .to_string();
        if more_magic != "ETS" {
            return Err(BioFormatsError::Format(format!(
                "ETS file {:?}: unexpected secondary magic {:?}",
                path, more_magic
            )));
        }
        // skip 4 (extra version), then pixelType(int), sizeC(int), colorspace(int),
        // compression(int), quality(int), tileX(int), tileY(int), tileZ(int),
        // skip 4*17 (pixel info hints), color[sizeC*bpp], skip(40-color),
        // componentOrder(int), usePyramid(int).
        let base = additional_header_offset + 8;
        let pixel_type_code = i32_at(base)?;
        let size_c = u32_at(base + 4)?;
        let compression = i32_at(base + 12)?;
        let tile_x = u32_at(base + 20)?;
        let tile_y = u32_at(base + 24)?;
        if size_c == 0 || tile_x == 0 || tile_y == 0 {
            return Err(BioFormatsError::Format(format!(
                "ETS file {:?}: sizeC and tile dimensions must be non-zero",
                path
            )));
        }
        let pixel_type = convert_ets_pixel_type(pixel_type_code)?;
        let bpp = pixel_type.bytes_per_sample();
        let expected_tile_size = bpp
            .checked_mul(size_c as usize)
            .and_then(|v| v.checked_mul(tile_x as usize))
            .and_then(|v| v.checked_mul(tile_y as usize))
            .ok_or_else(|| {
                BioFormatsError::Format(format!("ETS file {:?}: tile byte count overflows", path))
            })?;
        // color region begins at base + 32 + 68 = base + 100, always 40 bytes.
        let color_start = base + 32 + 4 * 17;
        let color_len = (size_c as usize).saturating_mul(bpp).min(40);
        let background = rd(color_start, color_len)?.to_vec();
        let component_order = i32_at(color_start + 40)?;
        let use_pyramid = i32_at(color_start + 44)? != 0;
        let bgr = component_order == 1 && compression == ETS_RAW;

        // Used-chunk table at usedChunkOffset. Each entry:
        //   skip 4; nDimensions * int coordinate; long offset; int nBytes; skip 4.
        let entry_len = 4usize
            .checked_add(n_dimensions as usize * 4)
            .and_then(|v| v.checked_add(8 + 4 + 4))
            .ok_or_else(|| {
                BioFormatsError::Format(format!(
                    "ETS file {:?}: chunk table length overflows",
                    path
                ))
            })?;
        let table_len = entry_len.checked_mul(n_used_chunks).ok_or_else(|| {
            BioFormatsError::Format(format!("ETS file {:?}: chunk table length overflows", path))
        })?;
        rd(used_chunk_offset, table_len)?;
        let mut tiles = Vec::with_capacity(n_used_chunks);
        let mut off = used_chunk_offset;
        for _ in 0..n_used_chunks {
            off += 4;
            let mut coord = Vec::with_capacity(n_dimensions as usize);
            for _ in 0..n_dimensions {
                coord.push(i32_at(off)?);
                off += 4;
            }
            let tile_offset = u64_at(off)?;
            off += 8;
            let n_bytes = u32_at(off)?;
            off += 4;
            off += 4; // reserved
            if n_bytes == 0 {
                return Err(BioFormatsError::Format(format!(
                    "ETS file {:?}: tile byte count must be non-zero",
                    path
                )));
            }
            let tile_end = tile_offset.checked_add(n_bytes as u64).ok_or_else(|| {
                BioFormatsError::Format(format!(
                    "ETS file {:?}: tile payload offset overflows",
                    path
                ))
            })?;
            if tile_end > bytes.len() as u64 {
                return Err(BioFormatsError::InvalidData(format!(
                    "ETS file {:?}: tile payload extends past end of file",
                    path
                )));
            }
            if compression == ETS_RAW && n_bytes as usize != expected_tile_size {
                return Err(BioFormatsError::InvalidData(format!(
                    "ETS file {:?}: RAW tile byte count is {}, expected {expected_tile_size}",
                    path, n_bytes
                )));
            }
            tiles.push((coord, tile_offset, n_bytes));
        }

        let mut vol = EtsVolume {
            path: path.to_path_buf(),
            n_dimensions,
            size_c,
            compression,
            tile_x,
            tile_y,
            pixel_type_code,
            bgr,
            use_pyramid,
            background,
            dim_z: None,
            dim_c: None,
            dim_t: None,
            tiles,
            levels: Vec::new(),
            pyramid_width: None,
            pyramid_height: None,
            tile_origin_x: None,
            tile_origin_y: None,
            dim_order: VsiDimOrder::default(),
            meta: VsiPyramidMeta::default(),
            physical_size_x: None,
            physical_size_y: None,
        };
        vol.compute_levels();
        Ok(vol)
    }

    /// Read one RAW tile by its (col,row[,...]) coordinate from an ETS volume.
    #[allow(dead_code)]
    fn read_raw_tile(vol: &EtsVolume, col: i32, row: i32) -> Result<Vec<u8>> {
        if vol.compression != ETS_RAW {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "cellSens ETS tile uses compression code {} (only RAW is decodable here)",
                vol.compression
            )));
        }
        let tile = vol
            .tiles
            .iter()
            .find(|(c, _, _)| c.first() == Some(&col) && c.get(1) == Some(&row));
        let Some((_, offset, n_bytes)) = tile else {
            return Err(BioFormatsError::PlaneOutOfRange(0));
        };
        let mut reader = BufReader::new(File::open(&vol.path).map_err(BioFormatsError::Io)?);
        read_bytes_at(&mut reader, *offset, *n_bytes as usize)
    }

    fn enrich_metadata(&mut self, vsi_path: &Path) {
        use crate::common::metadata::MetadataValue;
        let ets_files = Self::find_ets_files(vsi_path);
        let mut volumes = Vec::new();
        for f in &ets_files {
            if let Ok(v) = Self::parse_ets(f) {
                volumes.push(v);
            }
        }
        self.tiff_series = self.inner.series_count();
        if volumes.is_empty() {
            return;
        }

        // Apply VSI tag-tree geometry to each ETS volume.
        //
        // In the common (non-orphan) case the parsed `Pyramid` blocks correspond
        // 1:1, in order, to the ETS volumes (CellSensReader.java:1366).
        //
        // When there are more `frame_*.ets` files than pyramid blocks, some ETS
        // files are orphans. Java sets `hasOrphanEtsFiles = pyramids.size() <
        // (files.size() - 1)` (the `- 1` discounts the `.vsi` itself, leaving the
        // ETS count) and matches each ETS volume to an as-yet-unclaimed pyramid by
        // width/height range, dropping any ETS that finds no match
        // (CellSensReader.java:782, 1329-1364).
        let pyramids = Self::parse_vsi_pyramids(vsi_path);
        // If the VSI tag-tree yielded no `Pyramid` blocks at all, there is nothing
        // to match against and nothing to crop to: keep every ETS volume and
        // derive geometry purely from the tile grid (compute_levels falls back to
        // the stored tile extent when pyramid_width/height are absent). Without
        // this, a `.vsi` whose tag-tree we can't fully parse would expose only the
        // tiny embedded TIFF overview images and never the real ETS pixels.
        let has_orphan_ets = !pyramids.is_empty() && pyramids.len() < volumes.len();
        if pyramids.is_empty() {
            for vol in volumes.iter_mut() {
                vol.compute_levels();
            }
        } else if has_orphan_ets {
            // Track which pyramids have already been claimed by an ETS volume
            // (Java's `Pyramid.HasAssociatedEtsFile`).
            let mut claimed = vec![false; pyramids.len()];
            let mut matched: Vec<EtsVolume> = Vec::with_capacity(pyramids.len());
            for mut vol in volumes.into_iter() {
                let (max_w, max_h) = vol.max_pixel_extent();
                let tx = vol.tile_x as i64;
                let ty = vol.tile_y as i64;
                // Find an unclaimed pyramid whose declared size falls within one
                // tile of this volume's stored extent (CellSensReader.java:1340-1349).
                let found = pyramids.iter().enumerate().position(|(i, p)| {
                    if claimed[i] {
                        return false;
                    }
                    let pw = p.width.map(|w| w as i64);
                    let ph = p.height.map(|h| h as i64);
                    match (pw, ph) {
                        (Some(pw), Some(ph)) => {
                            pw <= max_w && pw >= max_w - tx && ph <= max_h && ph >= max_h - ty
                        }
                        _ => false,
                    }
                });
                match found {
                    Some(i) => {
                        claimed[i] = true;
                        let p = &pyramids[i];
                        vol.pyramid_width = p.width;
                        vol.pyramid_height = p.height;
                        vol.tile_origin_x = p.tile_origin_x;
                        vol.tile_origin_y = p.tile_origin_y;
                        vol.dim_order = p.dim_order;
                        vol.meta = p.meta.clone();
                        vol.physical_size_x = p.physical_size_x;
                        vol.physical_size_y = p.physical_size_y;
                        vol.compute_levels();
                        matched.push(vol);
                    }
                    // No matching metadata block: this is an orphan ETS file. Drop
                    // it entirely (CellSensReader.java:1350-1363).
                    None => {}
                }
            }
            volumes = matched;
        } else {
            // Non-orphan case: pyramids correspond 1:1, in order, to ETS volumes
            // (CellSensReader.java:1366). Zip handles the common equal-length case;
            // any extra volumes keep their tile-grid geometry.
            for (vol, p) in volumes.iter_mut().zip(pyramids.iter()) {
                vol.pyramid_width = p.width;
                vol.pyramid_height = p.height;
                vol.tile_origin_x = p.tile_origin_x;
                vol.tile_origin_y = p.tile_origin_y;
                vol.dim_order = p.dim_order;
                vol.meta = p.meta.clone();
                vol.physical_size_x = p.physical_size_x;
                vol.physical_size_y = p.physical_size_y;
                vol.compute_levels();
            }
        }
        // Record ETS summary in the first series' metadata.
        if let Some(s) = self.inner.series_list_mut().first_mut() {
            s.metadata.series_metadata.insert(
                "cellsens.ets_file_count".into(),
                MetadataValue::Int(volumes.len() as i64),
            );
            for (i, v) in volumes.iter().enumerate() {
                let p = format!("cellsens.ets.{}", i);
                s.metadata.series_metadata.insert(
                    format!("{p}.tile_size"),
                    MetadataValue::String(format!("{}x{}", v.tile_x, v.tile_y)),
                );
                s.metadata
                    .series_metadata
                    .insert(format!("{p}.size_c"), MetadataValue::Int(v.size_c as i64));
                s.metadata.series_metadata.insert(
                    format!("{p}.compression"),
                    MetadataValue::Int(v.compression as i64),
                );
                s.metadata.series_metadata.insert(
                    format!("{p}.tile_count"),
                    MetadataValue::Int(v.tiles.len() as i64),
                );
                s.metadata.series_metadata.insert(
                    format!("{p}.dimensions"),
                    MetadataValue::Int(v.n_dimensions as i64),
                );
                s.metadata.series_metadata.insert(
                    format!("{p}.resolution_count"),
                    MetadataValue::Int(v.levels.len() as i64),
                );
                if let Some(l0) = v.levels.first() {
                    s.metadata.series_metadata.insert(
                        format!("{p}.size"),
                        MetadataValue::String(format!("{}x{}", l0.size_x, l0.size_y)),
                    );
                }
                let _ = v.pixel_type_code;

                // Non-geometry acquisition metadata (CellSensReader.java:1881-1979).
                insert_cellsens_acquisition_metadata(&mut s.metadata.series_metadata, &p, &v.meta);
            }
        }
        self.ets = volumes;

        // Build the flattened logical-series ordering. Mirrors Java with
        // setFlattenedResolutions(true): each ETS pyramid resolution level is a
        // distinct series, followed by one embedded TIFF image (the overview, the
        // first IFD of the .vsi). When ETS files exist, Java exposes
        // `files.size()` core series = (#ETS pyramids) + 1 overview, and the other
        // embedded TIFF IFDs are NOT exposed (CellSensReader.java:732-855).
        self.series_map.clear();
        self.series_names.clear();
        self.series_phys.clear();
        let filename = vsi_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("image")
            .to_string();
        if self.ets.is_empty() {
            // No ETS: fall back to exposing the inner TIFF series directly.
            for s in 0..self.tiff_series {
                self.series_map.push(CellSensTarget::Tiff(s));
                self.series_names.push(format!("{filename} #{}", s + 1));
                self.series_phys.push(None);
            }
        } else {
            // ETS pyramid resolution levels first (flattened).
            for (vi, vol) in self.ets.iter().enumerate() {
                for res in 0..vol.levels.len() {
                    self.series_map.push(CellSensTarget::Ets {
                        volume: vi,
                        resolution: res,
                    });
                    // Image 0 of the first pyramid takes the pyramid (stack) name;
                    // later resolution levels get the default "filename #N"
                    // (CellSensReader.java:994-1031 + populatePixels defaults).
                    let series_idx = self.series_map.len() - 1;
                    if res == 0 && vi == 0 {
                        let name = vol
                            .meta
                            .name
                            .clone()
                            .unwrap_or_else(|| format!("{filename} #{}", series_idx + 1));
                        self.series_names.push(name);
                        self.series_phys
                            .push(match (vol.physical_size_x, vol.physical_size_y) {
                                (Some(x), Some(y)) => Some((x, y)),
                                _ => None,
                            });
                    } else {
                        self.series_names
                            .push(format!("{filename} #{}", series_idx + 1));
                        self.series_phys.push(None);
                    }
                }
            }
            // One embedded TIFF overview image last (CellSensReader.java:826-855).
            if self.tiff_series > 0 {
                self.series_map.push(CellSensTarget::Tiff(0));
                self.series_names.push("macro image".to_string());
                self.series_phys.push(None);
            }
        }
    }

    /// Resolve a flattened logical-series index into a concrete target.
    fn resolve_series(&self, s: usize) -> Option<CellSensTarget> {
        self.series_map.get(s).copied()
    }

    /// IFD index backing inner TIFF series `ts`, plane `plane`.
    fn overview_ifd_index(&self, ts: usize, plane: u32) -> Option<usize> {
        let series = self.inner.series_list().get(ts)?;
        let p = plane as usize;
        if !series.plane_ifd_indices.is_empty() {
            series.plane_ifd_indices.get(p).copied().flatten()
        } else {
            series.ifd_indices.get(p).copied()
        }
    }

    /// Decode the full, chunky-interleaved overview plane when its embedded-TIFF
    /// IFD is a single-strip baseline JPEG whose quantization/Huffman tables live
    /// in the JPEGTables tag (347). Mirrors Java `TiffParser.getTile`, which
    /// splices `jpegTable[..len-2] + scan[2..]` before handing the stream to the
    /// JPEG codec. Returns `None` for any other IFD (uncompressed/LZW/multi-strip),
    /// so the caller delegates to the inner TIFF reader unchanged.
    fn decode_overview_jpeg_full(&mut self, ts: usize, plane: u32) -> Result<Option<Vec<u8>>> {
        let Some(idx) = self.overview_ifd_index(ts, plane) else {
            return Ok(None);
        };
        let Some(ifd) = self.inner.ifd(idx) else {
            return Ok(None);
        };
        if !matches!(ifd.compression(), Compression::Jpeg | Compression::JpegNew) {
            return Ok(None);
        }
        let Some(tables) = ifd.get(tag::JPEG_TABLES).and_then(ifd_raw_bytes) else {
            return Ok(None);
        };
        let offsets = ifd
            .get(tag::STRIP_OFFSETS)
            .map(IfdValue::as_vec_u64)
            .unwrap_or_default();
        let counts = ifd
            .get(tag::STRIP_BYTE_COUNTS)
            .map(IfdValue::as_vec_u64)
            .unwrap_or_default();
        // Only the single-strip case is handled here (the cellSens overview).
        if offsets.len() != 1 || counts.len() != 1 {
            return Ok(None);
        }
        let photometric = ifd
            .get(tag::PHOTOMETRIC_INTERPRETATION)
            .and_then(IfdValue::as_u16)
            .unwrap_or(0);
        let Some(path) = self.vsi_path.clone() else {
            return Ok(None);
        };
        let mut file = BufReader::new(File::open(&path)?);
        let scan = read_bytes_at(&mut file, offsets[0], counts[0] as usize)?;
        let combined = merge_jpeg_tables(&tables, &scan);
        let mut decoder = jpeg_decoder::Decoder::new(combined.as_slice());
        // PhotometricInterpretation RGB (2): the stored components already ARE RGB,
        // so suppress jpeg_decoder's default YCbCr->RGB transform (matches Java's
        // ImageIO, which emits the components as-is). YCbCr keeps the default.
        if photometric == 2 {
            decoder.set_color_transform(jpeg_decoder::ColorTransform::RGB);
        }
        let pixels = decoder
            .decode()
            .map_err(|e| BioFormatsError::Codec(e.to_string()))?;
        Ok(Some(pixels))
    }

    fn logical_series_metadata_for_ome(&self, target: CellSensTarget) -> Option<ImageMetadata> {
        match target {
            CellSensTarget::Tiff(ts) => {
                let mut meta = self.inner.series_list().get(ts)?.metadata.clone();
                meta.is_interleaved = false;
                meta.dimension_order = crate::common::metadata::DimensionOrder::XYCZT;
                Some(meta)
            }
            CellSensTarget::Ets { volume, resolution } => {
                self.ets.get(volume)?.level_metadata(resolution).ok()
            }
        }
    }
}

/// Raw byte payload of an UNDEFINED/BYTE IFD value (e.g. the JPEGTables tag).
fn ifd_raw_bytes(v: &IfdValue) -> Option<Vec<u8>> {
    match v {
        IfdValue::Undefined(b) | IfdValue::Byte(b) => Some(b.clone()),
        _ => None,
    }
}

/// Splice abbreviated JPEGTables (tag 347) ahead of a strip's entropy-coded scan:
/// `SOI + tables[without SOI/EOI] + scan[without SOI]`.
fn merge_jpeg_tables(tables: &[u8], scan: &[u8]) -> Vec<u8> {
    let starts_soi = |d: &[u8]| d.len() >= 2 && d[0] == 0xff && d[1] == 0xd8;
    if !starts_soi(tables) {
        return scan.to_vec();
    }
    let mut table_payload = &tables[2..];
    if table_payload.len() >= 2
        && table_payload[table_payload.len() - 2] == 0xff
        && table_payload[table_payload.len() - 1] == 0xd9
    {
        table_payload = &table_payload[..table_payload.len() - 2];
    }
    let scan_payload = if starts_soi(scan) { &scan[2..] } else { scan };
    let mut out = Vec::with_capacity(2 + table_payload.len() + scan_payload.len());
    out.extend_from_slice(&[0xff, 0xd8]);
    out.extend_from_slice(table_payload);
    out.extend_from_slice(scan_payload);
    out
}

/// Convert a chunky/interleaved plane (pixel-major) to planar (channel-major).
fn deinterleave_to_planar(chunky: &[u8], plane: usize, spp: usize, sample: usize) -> Vec<u8> {
    let pixel = spp * sample;
    let mut out = vec![0u8; chunky.len()];
    for px in 0..plane {
        for ch in 0..spp {
            let src = px * pixel + ch * sample;
            let dst = (ch * plane + px) * sample;
            out[dst..dst + sample].copy_from_slice(&chunky[src..src + sample]);
        }
    }
    out
}

/// Crop a `w`×`h` chunky region at (`x`,`y`) out of a chunky full plane.
fn crop_chunky(
    full: &[u8],
    full_width: usize,
    x: usize,
    y: usize,
    w: usize,
    h: usize,
    pixel: usize,
) -> Vec<u8> {
    let mut out = vec![0u8; w * h * pixel];
    let row_len = w * pixel;
    for row in 0..h {
        let src = ((y + row) * full_width + x) * pixel;
        let dst = row * row_len;
        if src + row_len <= full.len() {
            out[dst..dst + row_len].copy_from_slice(&full[src..src + row_len]);
        }
    }
    out
}

impl Default for CellSensReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for CellSensReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("vsi"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let _ = self.close();
        self.inner.set_id(path).map_err(|_| {
            BioFormatsError::UnsupportedFormat(
                "Olympus cellSens VSI: could not parse as TIFF (may require ETS companion files)"
                    .to_string(),
            )
        })?;
        self.vsi_path = Some(path.to_path_buf());
        self.enrich_metadata(path);
        // Default to logical series 0.
        if !self.series_map.is_empty() {
            let _ = self.set_series(0);
        }
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.ets.clear();
        self.tiff_series = 0;
        self.target = CellSensTarget::Tiff(0);
        self.ets_meta = None;
        self.series_map.clear();
        self.series_names.clear();
        self.series_phys.clear();
        self.current = 0;
        self.vsi_path = None;
        self.inner.close()
    }
    fn series_count(&self) -> usize {
        self.series_map.len()
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        match self.resolve_series(s) {
            Some(CellSensTarget::Tiff(ts)) => {
                self.inner.set_series(ts)?;
                let _ = self.inner.set_resolution(0);
                self.target = CellSensTarget::Tiff(ts);
                // The embedded overview is reported by Java as non-interleaved
                // (planar) with dimensionOrder XYCZT (CellSensReader.java:845, 851).
                let mut om = self.inner.metadata().clone();
                om.is_interleaved = false;
                om.dimension_order = crate::common::metadata::DimensionOrder::XYCZT;
                self.ets_meta = Some(om);
                self.current = s;
                Ok(())
            }
            Some(CellSensTarget::Ets { volume, resolution }) => {
                self.target = CellSensTarget::Ets { volume, resolution };
                self.ets_meta = Some(self.ets[volume].level_metadata(resolution)?);
                self.current = s;
                Ok(())
            }
            None => Err(BioFormatsError::SeriesOutOfRange(s)),
        }
    }
    fn series(&self) -> usize {
        self.current
    }
    fn metadata(&self) -> &ImageMetadata {
        self.ets_meta
            .as_ref()
            .unwrap_or_else(|| self.inner.metadata())
    }
    // Flattened resolutions: every logical series is a single resolution level.
    fn resolution_count(&self) -> usize {
        1
    }
    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if level == 0 {
            Ok(())
        } else {
            Err(BioFormatsError::PlaneOutOfRange(level as u32))
        }
    }
    fn resolution(&self) -> usize {
        0
    }
    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        match self.target {
            CellSensTarget::Tiff(ts) => {
                // The inner TIFF reader returns the overview chunky/interleaved
                // (or fails on a JPEGTables-abbreviated JPEG strip, which we decode
                // ourselves). Java reports the overview as non-interleaved (planar),
                // so the FULL plane must be de-interleaved to match — the inner
                // reader does NOT do this, which is why a raw `open_bytes` diverged
                // everywhere except the very first pixels.
                let full = match self.decode_overview_jpeg_full(ts, p)? {
                    Some(f) => f,
                    None => self.inner.open_bytes(p)?,
                };
                let meta = self.metadata();
                let spp = if meta.is_rgb {
                    meta.size_c.max(1) as usize
                } else {
                    1
                };
                let sample = meta.pixel_type.bytes_per_sample();
                let plane = (meta.size_x as usize) * (meta.size_y as usize);
                if spp > 1 && sample > 0 && full.len() == plane * spp * sample {
                    Ok(deinterleave_to_planar(&full, plane, spp, sample))
                } else {
                    Ok(full)
                }
            }
            CellSensTarget::Ets { volume, resolution } => {
                let vol = &self.ets[volume];
                let level = vol
                    .levels
                    .get(resolution)
                    .ok_or(BioFormatsError::PlaneOutOfRange(p))?;
                // XYCZT plane ordering: C fastest, then Z, then T.
                let n_c = (level.size_c / vol.rgb_channels().max(1)).max(1);
                let n_z = level.size_z.max(1);
                let count = n_c * n_z * level.size_t.max(1);
                if p >= count {
                    return Err(BioFormatsError::PlaneOutOfRange(p));
                }
                let c = (p % n_c) as i32;
                let z = ((p / n_c) % n_z) as i32;
                let t = (p / (n_c * n_z)) as i32;
                vol.assemble_plane(resolution, z, c, t)
            }
        }
    }
    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        match self.target {
            CellSensTarget::Tiff(ts) => {
                // Source the requested chunky/interleaved region: either by cropping
                // our manually decoded JPEG overview (the inner reader can't merge
                // the JPEGTables tag), or via the inner reader for plain IFDs.
                let buf = match self.decode_overview_jpeg_full(ts, p)? {
                    Some(full) => {
                        let meta = self.metadata();
                        let spp = if meta.is_rgb {
                            meta.size_c.max(1) as usize
                        } else {
                            1
                        };
                        let pixel = spp * meta.pixel_type.bytes_per_sample();
                        crop_chunky(
                            &full,
                            meta.size_x as usize,
                            x as usize,
                            y as usize,
                            w as usize,
                            h as usize,
                            pixel,
                        )
                    }
                    None => self.inner.open_bytes_region(p, x, y, w, h)?,
                };
                // Java reports the overview as non-interleaved (planar). The region
                // buffer is interleaved RGB; de-interleave to match.
                let meta = self.metadata();
                let spp = if meta.is_rgb {
                    meta.size_c.max(1) as usize
                } else {
                    1
                };
                let bpp = meta.pixel_type.bytes_per_sample();
                if spp > 1 && bpp > 0 {
                    let plane = (w as usize) * (h as usize);
                    let sample = bpp;
                    let pixel = spp * sample;
                    if buf.len() == plane * pixel {
                        let mut out = vec![0u8; buf.len()];
                        for px in 0..plane {
                            for ch in 0..spp {
                                let src = px * pixel + ch * sample;
                                let dst = (ch * plane + px) * sample;
                                out[dst..dst + sample].copy_from_slice(&buf[src..src + sample]);
                            }
                        }
                        return Ok(out);
                    }
                }
                Ok(buf)
            }
            CellSensTarget::Ets { volume, .. } => {
                // ETS tiles interleave all channels into one plane.
                let spp = self.ets[volume].rgb_channels() as usize;
                let full = self.open_bytes(p)?;
                let meta = self.metadata();
                crate::common::region::crop_full_plane("cellSens ETS", &full, meta, spp, x, y, w, h)
            }
        }
    }
    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        match self.target {
            CellSensTarget::Tiff(_) => self.inner.open_thumb_bytes(p),
            CellSensTarget::Ets { .. } => {
                let meta = self.metadata();
                let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
                let (tx, ty) = ((meta.size_x - tw) / 2, (meta.size_y - th) / 2);
                self.open_bytes_region(p, tx, ty, tw, th)
            }
        }
    }

    /// Build one OME image per flattened logical series, mirroring Java's
    /// post-flattening `OMEPyramidStore` population (image 0 = pyramid/stack name
    /// + physical pixel size, intermediate pyramid levels = default "filename #N"
    /// names, overview = "macro image").
    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        use crate::common::ome_metadata::{OmeImage, OmeMetadata};
        if self.series_map.is_empty() {
            return None;
        }
        use crate::common::ome_metadata::OmeChannel;
        let mut images = Vec::with_capacity(self.series_map.len());
        for (i, target) in self.series_map.iter().enumerate() {
            let phys = self.series_phys.get(i).copied().flatten();
            // Each image carries one OME Channel; for RGB series its
            // samplesPerPixel == the RGB channel count (CellSensReader exposes 3).
            let spp = match *target {
                CellSensTarget::Ets { volume, .. } => self.ets[volume].rgb_channels(),
                CellSensTarget::Tiff(ts) => self.inner.series_list().get(ts).map_or(1, |s| {
                    if s.metadata.is_rgb {
                        s.metadata.size_c.max(1)
                    } else {
                        1
                    }
                }),
            };
            images.push(OmeImage {
                name: self.series_names.get(i).cloned(),
                physical_size_x: phys.map(|(x, _)| x),
                physical_size_y: phys.map(|(_, y)| y),
                channels: vec![OmeChannel {
                    name: None,
                    samples_per_pixel: spp,
                    ..OmeChannel::default()
                }],
                ..OmeImage::default()
            });
        }
        let mut ome = OmeMetadata {
            images,
            ..OmeMetadata::default()
        };
        for (i, target) in self.series_map.iter().copied().enumerate() {
            if let Some(meta) = self.logical_series_metadata_for_ome(target) {
                let _ = ome.add_original_metadata_annotations(&meta, i);
            }
        }
        Some(ome)
    }
}

// ---------------------------------------------------------------------------
// 11. Volocity clipping ACFF
// ---------------------------------------------------------------------------
/// Volocity Library Clipping format reader (`.acff`).
///
/// Port of the Java `VolocityClippingReader`. The header encodes endianness via
/// its first byte (`'I'` = little-endian), then a `FFCA` magic string. After
/// locating the geometry marker (`0x208`, or the big-endian `AISF` variant),
/// width/height/Z are read; pixels follow at a fixed offset. Pixel data is
/// either raw or LZO-compressed (auto-detected exactly as in Java). Single
/// channel/timepoint; `pixelType` defaults to `UINT8` and is refined to match
/// the decompressed plane size when the data is LZO-compressed.
pub struct VolocityClippingReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    /// Offset of the first pixel byte (after geometry header).
    pixel_offset: u64,
    little_endian: bool,
}

const VOLOCITY_CLIPPING_MAGIC: &str = "FFCA";
/// `AISF` as produced by a big-endian `readInt` over the four ASCII bytes.
const VOLOCITY_AISF: u32 = 0x4653_4941;

impl VolocityClippingReader {
    pub fn new() -> Self {
        VolocityClippingReader {
            path: None,
            meta: None,
            pixel_offset: 0,
            little_endian: true,
        }
    }
}

impl Default for VolocityClippingReader {
    fn default() -> Self {
        Self::new()
    }
}

/// Read a 4-byte integer with the given endianness from `data` at `pos`.
fn volocity_read_int(data: &[u8], pos: usize, little_endian: bool) -> Option<u32> {
    if pos + 4 > data.len() {
        return None;
    }
    let b: [u8; 4] = data[pos..pos + 4].try_into().unwrap();
    Some(if little_endian {
        u32::from_le_bytes(b)
    } else {
        u32::from_be_bytes(b)
    })
}

impl FormatReader for VolocityClippingReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("acff"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        // Mirrors Java `isThisType(RandomAccessInputStream)`, which returns false.
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        if data.len() < 9 {
            return Err(BioFormatsError::Format(
                "Volocity clipping file is too short".into(),
            ));
        }

        let mut little_endian = data[0] == b'I';
        // skip first byte + 4 bytes, then read the 4-char magic string
        let magic = String::from_utf8_lossy(&data[5..9]).into_owned();
        if magic != VOLOCITY_CLIPPING_MAGIC {
            return Err(BioFormatsError::Format(format!(
                "Found invalid magic string: {magic}"
            )));
        }

        // Scan for the geometry marker (0x208) or big-endian AISF variant.
        let mut fp = 9usize;
        let mut check = volocity_read_int(&data, fp, little_endian)
            .ok_or_else(|| BioFormatsError::Format("Volocity clipping header truncated".into()))?;
        fp += 4;
        while check != 0x208 && check != VOLOCITY_AISF {
            // Java: in.seek(filePointer - 3); check = readInt();
            fp = fp.checked_sub(3).ok_or_else(|| {
                BioFormatsError::Format("Volocity clipping geometry marker not found".into())
            })?;
            check = match volocity_read_int(&data, fp, little_endian) {
                Some(v) => v,
                None => {
                    return Err(BioFormatsError::Format(
                        "Volocity clipping geometry marker not found".into(),
                    ))
                }
            };
            fp += 4;
        }
        if check == VOLOCITY_AISF {
            little_endian = false;
            fp += 28; // skipBytes(28)
        }

        let read_at = |fp: &mut usize| -> Result<u32> {
            let v = volocity_read_int(&data, *fp, little_endian).ok_or_else(|| {
                BioFormatsError::Format("Volocity clipping dimensions truncated".into())
            })?;
            *fp += 4;
            Ok(v)
        };
        let size_x = read_at(&mut fp)?;
        let size_y = read_at(&mut fp)?;
        let size_z = read_at(&mut fp)?.max(1);

        if size_x == 0 || size_y == 0 {
            return Err(BioFormatsError::Format(
                "Volocity clipping image has zero width or height".into(),
            ));
        }

        // pixelOffset = filePointer + 65
        let mut pixel_offset = (fp + 65) as u64;
        let mut pixel_type = PixelType::Uint8;

        // If the raw payload is implausibly small, the data is LZO-compressed;
        // probe successive offsets and infer pixel type from the decompressed
        // plane size (port of the Java auto-detection loop). We only adjust the
        // pixel type here; actual decompression happens in open_bytes.
        let plane_pixels = (size_x as usize) * (size_y as usize);
        if plane_pixels.saturating_mul(100) >= data.len() {
            let mut probe = pixel_offset as usize;
            while probe < data.len() {
                if let Ok(decoded) = crate::common::codec::decompress_lzo(&data[probe..]) {
                    if !decoded.is_empty() && decoded.len() % plane_pixels == 0 {
                        let bytes = decoded.len() / plane_pixels;
                        pixel_type = match bytes {
                            1 => PixelType::Uint8,
                            2 => PixelType::Uint16,
                            4 => PixelType::Float32,
                            _ => PixelType::Uint8,
                        };
                        pixel_offset = probe as u64;
                        break;
                    }
                }
                probe += 1;
            }
        }

        let meta = ImageMetadata {
            size_x,
            size_y,
            size_z,
            size_c: 1,
            size_t: 1,
            image_count: size_z,
            pixel_type,
            bits_per_pixel: (pixel_type.bytes_per_sample() * 8) as u8,
            dimension_order: crate::common::metadata::DimensionOrder::XYCZT,
            is_little_endian: little_endian,
            ..ImageMetadata::default()
        };

        self.path = Some(path.to_path_buf());
        self.little_endian = little_endian;
        self.pixel_offset = pixel_offset;
        self.meta = Some(meta);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.pixel_offset = 0;
        self.little_endian = true;
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
        let plane_size =
            (meta.size_x as usize) * (meta.size_y as usize) * meta.pixel_type.bytes_per_sample();
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        let start = self.pixel_offset as usize;

        // Java: if planeSize * 2 + pixelOffset < fileLength -> data is raw.
        let full = if plane_size * 2 + start < data.len() {
            data.get(start..).map(<[u8]>::to_vec).unwrap_or_default()
        } else {
            crate::common::codec::decompress_lzo(data.get(start..).unwrap_or_default())?
        };

        let offset = (plane_index as usize) * plane_size;
        let end = offset + plane_size;
        if end > full.len() {
            return Err(BioFormatsError::InvalidData(
                "Volocity clipping plane extends beyond available pixel data".into(),
            ));
        }
        Ok(full[offset..end].to_vec())
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
        crop_full_plane("Volocity clipping", &full, meta, 1, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let (sx, sy) = {
            let meta = self.metadata();
            (meta.size_x, meta.size_y)
        };
        if sx == 0 || sy == 0 {
            return Err(BioFormatsError::NotInitialized);
        }
        let tw = sx.min(256);
        let th = sy.min(256);
        let tx = (sx - tw) / 2;
        let ty = (sy - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}

// ---------------------------------------------------------------------------
// 12. Bruker MicroCT — TIFF delegate
// ---------------------------------------------------------------------------
/// Bruker MicroCT format reader (`.ctf`).
///
/// MicroCT files use TIFF data; delegates to `TiffReader`.
pub struct MicroCtReader {
    inner: crate::tiff::TiffReader,
}

impl MicroCtReader {
    pub fn new() -> Self {
        MicroCtReader {
            inner: crate::tiff::TiffReader::new(),
        }
    }
}

impl Default for MicroCtReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for MicroCtReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("ctf"))
    }
    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
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
    fn set_resolution(&mut self, level: usize) -> Result<()> {
        self.inner.set_resolution(level)
    }
}

// ---------------------------------------------------------------------------
// 13. Bio-Rad SCN confocal — TIFF delegate
// ---------------------------------------------------------------------------
/// Bio-Rad SCN confocal format reader (`.scn`).
///
/// Ported from the Java `BioRadSCNReader`. These `.scn` files are NOT TIFF;
/// they are a MIME-multipart container (magic "Generated by Image Lab"). The
/// reader walks `Content-Type`/`Content-Length`/boundary headers: an
/// `application/octet-stream` part holds the raw little/big-endian pixel data,
/// and `text/xml` parts describe the image (`<size_pix width=.. height=..>`,
/// `<scanner max_value=..>` → pixel type, `<size_mm>`, `<endian>`,
/// `<channel_count>`, gain/exposure/serial/binning/imager).
pub struct BioRadScnReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    pixels_offset: u64,
}

impl BioRadScnReader {
    pub fn new() -> Self {
        BioRadScnReader {
            path: None,
            meta: None,
            pixels_offset: 0,
        }
    }
}

impl Default for BioRadScnReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for BioRadScnReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("scn"))
    }
    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // Magic string "Generated by Image Lab" within the first 64 bytes.
        let n = header.len().min(64);
        let prefix = String::from_utf8_lossy(&header[..n]);
        prefix.contains("Generated by Image Lab")
    }
    fn set_id(&mut self, path: &Path) -> Result<()> {
        use crate::common::metadata::MetadataValue;
        self.close()?;
        let bytes = std::fs::read(path).map_err(BioFormatsError::Io)?;
        let text = String::from_utf8_lossy(&bytes);
        if !text.contains("Generated by Image Lab") {
            return Err(BioFormatsError::UnsupportedFormat(
                "Bio-Rad SCN: missing 'Generated by Image Lab' magic".into(),
            ));
        }

        // Walk the MIME-multipart structure to find the pixel block offset and
        // collect text/xml blocks. We track byte offsets so the octet-stream
        // body offset matches the on-disk pixel position.
        let mut pixels_offset: Option<u64> = None;
        let mut pixels_length: Option<usize> = None;
        let mut current_type = String::new();
        let mut current_boundary = String::new();
        let mut current_length = 0usize;
        let mut xml_blocks: Vec<String> = Vec::new();

        // Iterate over physical lines, tracking byte position.
        let mut pos = 0usize; // byte offset of start of current line
        let line_iter = bytes.split(|&b| b == b'\n');
        for raw_line in line_iter {
            // The byte offset just after this line's terminating '\n'.
            let line_len_with_nl = raw_line.len() + 1;
            let line = String::from_utf8_lossy(raw_line);
            let line = line.trim_end_matches('\r').trim();

            if line.starts_with("Content-Type") {
                current_type =
                    line[line.find(' ').map(|i| i + 1).unwrap_or(line.len())..].to_string();
                if let Some(b) = current_type.find("boundary") {
                    // boundary=<value> ; value ends at trailing quote/semicolon
                    let after = &current_type[b + "boundary".len()..];
                    let after = after.trim_start_matches(['=', '"', ' ']);
                    let end = after.find(['"', ';']).unwrap_or(after.len());
                    current_boundary = after[..end].to_string();
                }
                if let Some(sc) = current_type.find(';') {
                    current_type = current_type[..sc].to_string();
                }
                current_type = current_type.trim().to_string();
            } else if !current_boundary.is_empty() && line == format!("--{}", current_boundary) {
                current_length = 0;
            } else if line.starts_with("Content-Length") {
                current_length = line[line.find(' ').map(|i| i + 1).unwrap_or(line.len())..]
                    .trim()
                    .parse()
                    .unwrap_or(0);
            } else if line.is_empty() {
                // A blank line ends the headers of a part; its body follows.
                let body_offset = (pos + line_len_with_nl) as u64;
                if current_type == "application/octet-stream" {
                    pixels_offset = Some(body_offset);
                    pixels_length = Some(current_length);
                } else if current_type == "text/xml" {
                    let start = body_offset as usize;
                    let end = (start + current_length).min(bytes.len());
                    if start <= end {
                        xml_blocks.push(String::from_utf8_lossy(&bytes[start..end]).into_owned());
                    }
                }
            }
            pos += line_len_with_nl;
        }

        // Parse the XML metadata blocks via the lightweight tag scanner.
        let mut meta = ImageMetadata {
            size_z: 1,
            size_t: 1,
            size_c: 1,
            image_count: 1,
            is_little_endian: true,
            pixel_type: PixelType::Uint8,
            bits_per_pixel: 8,
            dimension_order: crate::common::metadata::DimensionOrder::XYCZT,
            ..ImageMetadata::default()
        };
        let mut size_mm_x: Option<f64> = None;
        let mut size_mm_y: Option<f64> = None;

        for block in &xml_blocks {
            for tag in scn_scan_tags(block) {
                match tag.0.as_str() {
                    "size_pix" => {
                        if let Some(w) = tag.1.get("width").and_then(|s| s.parse().ok()) {
                            meta.size_x = w;
                        }
                        if let Some(h) = tag.1.get("height").and_then(|s| s.parse().ok()) {
                            meta.size_y = h;
                        }
                    }
                    "scanner" => {
                        if let Some(mv) = tag.1.get("max_value").and_then(|s| s.parse::<u64>().ok())
                        {
                            if mv <= 256 {
                                meta.pixel_type = PixelType::Uint8;
                                meta.bits_per_pixel = 8;
                            } else if mv <= 65535 {
                                meta.pixel_type = PixelType::Uint16;
                                meta.bits_per_pixel = 16;
                            }
                        }
                    }
                    "size_mm" => {
                        if let Some(w) = tag.1.get("width").and_then(|s| s.parse().ok()) {
                            size_mm_x = Some(w);
                        }
                        if let Some(h) = tag.1.get("height").and_then(|s| s.parse().ok()) {
                            size_mm_y = Some(h);
                        }
                    }
                    "serial_number" => {
                        if let Some(v) = tag.1.get("value") {
                            meta.series_metadata.insert(
                                "biorad.serial_number".into(),
                                MetadataValue::String(v.clone()),
                            );
                        }
                    }
                    "binning" => {
                        if let Some(v) = tag.1.get("value") {
                            meta.series_metadata
                                .insert("biorad.binning".into(), MetadataValue::String(v.clone()));
                        }
                    }
                    "image_date" => {
                        if let Some(v) = tag.1.get("value") {
                            meta.series_metadata.insert(
                                "biorad.acquisition_date".into(),
                                MetadataValue::String(v.clone()),
                            );
                        }
                    }
                    "imager" => {
                        if let Some(v) = tag.1.get("value") {
                            meta.series_metadata
                                .insert("biorad.model".into(), MetadataValue::String(v.clone()));
                        }
                    }
                    _ => {}
                }
            }
            // Element-text values: endian, channel_count, gain, exposure, name.
            if let Some(v) = scn_element_text(block, "endian") {
                meta.is_little_endian = v == "little";
            }
            if let Some(v) = scn_element_text(block, "channel_count").and_then(|s| s.parse().ok()) {
                meta.size_c = v;
            }
            if let Some(v) = scn_element_text(block, "application_gain") {
                if let Ok(g) = v.parse::<f64>() {
                    meta.series_metadata
                        .insert("biorad.gain".into(), MetadataValue::Float(g));
                }
            }
            if let Some(v) = scn_element_text(block, "exposure_time") {
                if let Ok(e) = v.parse::<f64>() {
                    meta.series_metadata
                        .insert("biorad.exposure_time".into(), MetadataValue::Float(e));
                }
            }
            if let Some(v) = scn_element_text(block, "name") {
                meta.series_metadata
                    .insert("biorad.image_name".into(), MetadataValue::String(v));
            }
        }

        // Physical pixel size (mm -> um per pixel).
        if let (Some(w), true) = (size_mm_x, meta.size_x > 0) {
            meta.series_metadata.insert(
                "biorad.physical_size_x".into(),
                MetadataValue::Float(w / meta.size_x as f64 * 1000.0),
            );
        }
        if let (Some(h), true) = (size_mm_y, meta.size_y > 0) {
            meta.series_metadata.insert(
                "biorad.physical_size_y".into(),
                MetadataValue::Float(h / meta.size_y as f64 * 1000.0),
            );
        }

        if meta.size_x == 0 || meta.size_y == 0 {
            return Err(BioFormatsError::Format(
                "Bio-Rad SCN: missing or invalid image dimensions".into(),
            ));
        }
        if meta.size_c == 0 {
            return Err(BioFormatsError::Format(
                "Bio-Rad SCN: channel count must be non-zero".into(),
            ));
        }
        let pixels_offset = pixels_offset.ok_or_else(|| {
            BioFormatsError::Format("Bio-Rad SCN: missing pixel octet-stream part".into())
        })?;
        let pixels_length = pixels_length.ok_or_else(|| {
            BioFormatsError::Format("Bio-Rad SCN: missing pixel octet-stream length".into())
        })?;
        let bpp = meta.pixel_type.bytes_per_sample();
        let plane = (meta.size_x as usize)
            .checked_mul(meta.size_y as usize)
            .and_then(|px| px.checked_mul(bpp))
            .ok_or_else(|| BioFormatsError::Format("Bio-Rad SCN: plane size overflows".into()))?;
        meta.image_count = meta
            .size_z
            .max(1)
            .checked_mul(meta.size_c.max(1))
            .and_then(|v| v.checked_mul(meta.size_t.max(1)))
            .ok_or_else(|| BioFormatsError::Format("Bio-Rad SCN: image count overflows".into()))?;
        let expected_pixels = plane
            .checked_mul(meta.image_count as usize)
            .ok_or_else(|| {
                BioFormatsError::Format("Bio-Rad SCN: pixel payload size overflows".into())
            })?;
        if pixels_length < expected_pixels {
            return Err(BioFormatsError::Format(format!(
                "Bio-Rad SCN: pixel payload is {pixels_length} bytes, expected at least {expected_pixels}"
            )));
        }
        let pixel_end = pixels_offset
            .checked_add(pixels_length as u64)
            .ok_or_else(|| {
                BioFormatsError::Format("Bio-Rad SCN: pixel payload end overflows".into())
            })?;
        if pixel_end > bytes.len() as u64 {
            return Err(BioFormatsError::Format(
                "Bio-Rad SCN: pixel payload extends beyond file".into(),
            ));
        }

        self.pixels_offset = pixels_offset;
        self.meta = Some(meta);
        self.path = Some(path.to_path_buf());
        Ok(())
    }
    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.pixels_offset = 0;
        Ok(())
    }
    fn series_count(&self) -> usize {
        usize::from(self.meta.is_some())
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
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
    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if p >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(p));
        }
        let bpp = meta.pixel_type.bytes_per_sample();
        let plane = meta.size_x as usize * meta.size_y as usize * bpp;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut reader = BufReader::new(File::open(path).map_err(BioFormatsError::Io)?);
        read_bytes_at(
            &mut reader,
            self.pixels_offset + (p as u64 * plane as u64),
            plane,
        )
    }
    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let meta = self
            .meta
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?
            .clone();
        let full = self.open_bytes(p)?;
        crop_full_plane("ScanR", &full, &meta, 1, x, y, w, h)
    }
    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        self.open_bytes(p)
    }
    fn resolution_count(&self) -> usize {
        1
    }
    fn set_resolution(&mut self, _level: usize) -> Result<()> {
        Ok(())
    }
}

/// Scan an XML fragment into `(tag_name, attributes)` pairs for each start tag.
/// Minimal parser sufficient for the attribute-only Bio-Rad SCN elements.
fn scn_scan_tags(xml: &str) -> Vec<(String, std::collections::HashMap<String, String>)> {
    let bytes = xml.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        if matches!(bytes.get(i + 1), Some(b'/') | Some(b'?') | Some(b'!')) {
            if let Some(end) = xml[i..].find('>') {
                i += end + 1;
            } else {
                break;
            }
            continue;
        }
        let mut j = i + 1;
        let mut quote = 0u8;
        while j < bytes.len() {
            let c = bytes[j];
            if quote != 0 {
                if c == quote {
                    quote = 0;
                }
            } else if c == b'"' || c == b'\'' {
                quote = c;
            } else if c == b'>' {
                break;
            }
            j += 1;
        }
        if j >= bytes.len() {
            break;
        }
        let inner = xml[i + 1..j].trim_end().trim_end_matches('/');
        let name_end = inner
            .find(|c: char| c.is_whitespace())
            .unwrap_or(inner.len());
        let name = inner[..name_end].to_string();
        let mut attrs = std::collections::HashMap::new();
        let mut a = &inner[name_end..];
        loop {
            let a_trim = a.trim_start();
            if a_trim.is_empty() {
                break;
            }
            let Some(eq) = a_trim.find('=') else { break };
            let key = a_trim[..eq].trim().to_string();
            let rest = a_trim[eq + 1..].trim_start();
            let rb = rest.as_bytes();
            if rb.is_empty() {
                break;
            }
            if rb[0] == b'"' || rb[0] == b'\'' {
                let q = rb[0];
                if let Some(close) = rest[1..].find(q as char) {
                    let val = rest[1..1 + close].to_string();
                    if !key.is_empty() {
                        attrs.insert(key, val);
                    }
                    a = &rest[1 + close + 1..];
                } else {
                    break;
                }
            } else {
                let end = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
                if !key.is_empty() {
                    attrs.insert(key, rest[..end].to_string());
                }
                a = &rest[end..];
            }
        }
        out.push((name, attrs));
        i = j + 1;
    }
    out
}

/// Return the text content of the first `<tag>...</tag>` element (no nested
/// elements). Helper for the Bio-Rad SCN XML blocks.
fn scn_element_text(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{}", tag);
    let start = xml.find(&open)?;
    let after = &xml[start..];
    let gt = after.find('>')?;
    // Self-closing element has no text.
    if after.as_bytes().get(gt.wrapping_sub(1)) == Some(&b'/') {
        return None;
    }
    let body = &after[gt + 1..];
    let end = body.find('<')?;
    let text = body[..end].trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

// ---------------------------------------------------------------------------
// 14. 3i SlideBook TIFF export — TIFF delegate
// ---------------------------------------------------------------------------
/// 3i SlideBook TIFF export format reader (`.tif`).
///
/// Ported from the Java `SlidebookTiffReader`. SlideBook TIFFs carry private
/// tags in the first IFD: 65000/65001/65002 (X/Y/Z stage position), 65004
/// (channel name), 65005 (objective magnification), 65007 (physical pixel
/// size). The magic check is `Software == "SlideBook"` plus presence of one of
/// these tags.
///
/// We port the single-file tag enrichment. The Java reader's multi-file
/// grouping by timestamp (each matching `.tif` is a separate channel) is not
/// replicated here; a single `.tif` is exposed via the inner `TiffReader`.
pub struct SlidebookTiffReader {
    inner: crate::tiff::TiffReader,
}

const SLIDEBOOK_X_POS_TAG: u16 = 65000;
const SLIDEBOOK_Y_POS_TAG: u16 = 65001;
const SLIDEBOOK_Z_POS_TAG: u16 = 65002;
const SLIDEBOOK_CHANNEL_TAG: u16 = 65004;
const SLIDEBOOK_MAGNIFICATION_TAG: u16 = 65005;
const SLIDEBOOK_PHYSICAL_SIZE_TAG: u16 = 65007;

fn slidebook_ifd_value_text(ifd: &Ifd, tag: u16) -> Option<String> {
    match ifd.get(tag)? {
        IfdValue::Ascii(s) => Some(s.clone()),
        IfdValue::Byte(v) | IfdValue::Undefined(v) => {
            let end = v.iter().position(|&b| b == 0).unwrap_or(v.len());
            if v[..end].iter().all(|b| b.is_ascii_graphic() || *b == b' ') {
                Some(String::from_utf8_lossy(&v[..end]).trim().to_string())
            } else {
                None
            }
        }
        other => {
            let values = other.as_vec_f64();
            values.first().map(|v| v.to_string())
        }
    }
}

fn slidebook_ifd_value_f64(ifd: &Ifd, tag: u16) -> Option<f64> {
    if let Some(s) = ifd.get_str(tag) {
        return s.trim().parse::<f64>().ok();
    }
    ifd.get(tag)
        .and_then(|value| value.as_vec_f64().first().copied())
}

fn slidebook_clean_channel_name(name: &str) -> String {
    let mut n = name;
    if let Some(p) = n.find(':') {
        n = &n[p + 1..];
    }
    if let Some(p) = n.find(';') {
        n = &n[..p];
    }
    n.trim().to_string()
}

impl SlidebookTiffReader {
    pub fn new() -> Self {
        SlidebookTiffReader {
            inner: crate::tiff::TiffReader::new(),
        }
    }

    fn enrich_metadata(&mut self) {
        use crate::common::metadata::MetadataValue;
        let mut vendor: Vec<(String, MetadataValue)> = Vec::new();
        let mut channel_name: Option<String> = None;
        if let Some(ifd) = self.inner.ifd(0) {
            if let Some(name) = slidebook_ifd_value_text(ifd, SLIDEBOOK_CHANNEL_TAG) {
                channel_name = Some(slidebook_clean_channel_name(&name));
            }
            if let Some(p) = slidebook_ifd_value_f64(ifd, SLIDEBOOK_PHYSICAL_SIZE_TAG) {
                if p > 0.0 {
                    vendor.push(("slidebook.physical_size_x".into(), MetadataValue::Float(p)));
                    vendor.push(("slidebook.physical_size_y".into(), MetadataValue::Float(p)));
                }
            }
            if let Some(mag) = slidebook_ifd_value_f64(ifd, SLIDEBOOK_MAGNIFICATION_TAG) {
                vendor.push(("slidebook.magnification".into(), MetadataValue::Float(mag)));
            }
            for (tag, key) in [
                (SLIDEBOOK_X_POS_TAG, "slidebook.position_x"),
                (SLIDEBOOK_Y_POS_TAG, "slidebook.position_y"),
                (SLIDEBOOK_Z_POS_TAG, "slidebook.position_z"),
            ] {
                if let Some(v) = slidebook_ifd_value_f64(ifd, tag) {
                    vendor.push((key.into(), MetadataValue::Float(v)));
                }
            }
        }
        if let Some(s) = self.inner.series_list_mut().first_mut() {
            if let Some(cn) = channel_name {
                s.metadata
                    .series_metadata
                    .insert("slidebook.channel.0.name".into(), MetadataValue::String(cn));
            }
            for (k, v) in vendor {
                s.metadata.series_metadata.insert(k, v);
            }
        }
    }
}

impl Default for SlidebookTiffReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for SlidebookTiffReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("tif"))
    }
    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }
    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.inner.set_id(path)?;
        self.enrich_metadata();
        Ok(())
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
    fn set_resolution(&mut self, level: usize) -> Result<()> {
        self.inner.set_resolution(level)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestEntry {
        tag: u16,
        typ: u16,
        count: u32,
        value: Vec<u8>,
    }

    fn temp_cif_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("bioformats_flowsight_{nanos}_{name}.cif"))
    }

    fn temp_flim2_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("bioformats_flim2_{nanos}_{name}"))
    }

    fn write_synthetic_raw(
        path: &Path,
        magic: &[u8],
        dims: (u32, u32, u32, u32, u32),
        pixel_code: u16,
        payload: &[u8],
    ) {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(magic);
        bytes.extend_from_slice(&dims.0.to_le_bytes());
        bytes.extend_from_slice(&dims.1.to_le_bytes());
        bytes.extend_from_slice(&dims.2.to_le_bytes());
        bytes.extend_from_slice(&dims.3.to_le_bytes());
        bytes.extend_from_slice(&dims.4.to_le_bytes());
        bytes.extend_from_slice(&pixel_code.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(payload);
        std::fs::write(path, bytes).unwrap();
    }

    fn write_native_ivision(
        path: &Path,
        data_type: u8,
        size_x: u32,
        size_y: u32,
        size_z: u16,
        payload: &[u8],
    ) {
        let mut bytes = vec![0u8; 72];
        bytes[..4].copy_from_slice(b"1.0A");
        bytes[4] = 1;
        bytes[5] = data_type;
        bytes[6..10].copy_from_slice(&size_x.to_be_bytes());
        bytes[10..14].copy_from_slice(&size_y.to_be_bytes());
        bytes[20..22].copy_from_slice(&size_z.to_be_bytes());
        if size_x > 1 && size_y > 1 {
            bytes.extend_from_slice(&vec![0u8; 2048]);
        }
        bytes.extend_from_slice(payload);
        std::fs::write(path, bytes).unwrap();
    }

    fn build_slidebook7_npy(descr: &str, shape: &[u32], payload: &[u8]) -> Vec<u8> {
        let shape_text = if shape.len() == 1 {
            format!("({},)", shape[0])
        } else {
            shape
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mut header =
            format!("{{'descr': '{descr}', 'fortran_order': False, 'shape': ({shape_text}), }}");
        let preamble_len = 10usize;
        let padding = (16 - ((preamble_len + header.len() + 1) % 16)) % 16;
        header.extend(std::iter::repeat_n(' ', padding));
        header.push('\n');

        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"\x93NUMPY");
        bytes.push(1);
        bytes.push(0);
        bytes.extend_from_slice(&(header.len() as u16).to_le_bytes());
        bytes.extend_from_slice(header.as_bytes());
        bytes.extend_from_slice(payload);
        bytes
    }

    fn write_slidebook7_npy(path: &Path, descr: &str, shape: &[u32], payload: &[u8]) {
        let bytes = build_slidebook7_npy(descr, shape, payload);
        std::fs::write(path, bytes).unwrap();
    }

    fn write_slidebook7_npyz(path: &Path, descr: &str, shape: &[u32], payload: &[u8]) {
        let npy = build_slidebook7_npy(descr, shape, payload);
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&npy).unwrap();
        std::fs::write(path, encoder.finish().unwrap()).unwrap();
    }

    fn write_native_slidebook7(
        slide_path: &Path,
        title: &str,
        dims: (u32, u32, u32, u32, u32),
        planes_by_channel_time: &[((u32, u32), Vec<u16>)],
    ) {
        std::fs::write(slide_path, b"SlideBook 7 native placeholder").unwrap();
        let root = slide_path.with_extension("dir");
        let group = root.join(format!("{title}.imgdir"));
        std::fs::create_dir_all(&group).unwrap();
        let image_record = format!(
            "mWidth: {}\nmHeight: {}\nmNumPlanes: {}\nmNumChannels: {}\nmNumTimepoints: {}\n",
            dims.0, dims.1, dims.2, dims.3, dims.4
        );
        std::fs::write(group.join("ImageRecord.yaml"), image_record).unwrap();
        for ((channel, timepoint), values) in planes_by_channel_time {
            let payload = values
                .iter()
                .copied()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>();
            let path = group.join(format!("ImageData_Ch{channel}_TP{timepoint:07}.npy"));
            write_slidebook7_npy(&path, "<u2", &[dims.2, dims.1, dims.0], &payload);
        }
    }

    fn im3_record(name: &str, rec_type: u32, payload: Vec<u8>) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(name.len() as u32).to_le_bytes());
        bytes.extend_from_slice(name.as_bytes());
        bytes.extend_from_slice(&((payload.len() + 8) as u32).to_le_bytes());
        bytes.extend_from_slice(&rec_type.to_le_bytes());
        bytes.extend_from_slice(&payload);
        bytes
    }

    fn im3_container(name: &str, children: Vec<Vec<u8>>) -> Vec<u8> {
        let mut payload = vec![0u8; 8];
        for child in children {
            payload.extend_from_slice(&child);
        }
        im3_record(name, 0, payload)
    }

    fn im3_int_array(name: &str, values: &[u32]) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&1u32.to_le_bytes());
        payload.extend_from_slice(&(values.len() as u32).to_le_bytes());
        for value in values {
            payload.extend_from_slice(&value.to_le_bytes());
        }
        im3_record(name, 6, payload)
    }

    fn im3_int_scalar(name: &str, value: u32) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&0u32.to_le_bytes());
        payload.extend_from_slice(&value.to_le_bytes());
        im3_record(name, 6, payload)
    }

    fn im3_string_scalar(name: &str, value: &str) -> Vec<u8> {
        let mut payload = value.as_bytes().to_vec();
        payload.push(0);
        im3_record(name, 2, payload)
    }

    fn im3_java_string_scalar(name: &str, value: &str) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&0u32.to_le_bytes());
        payload.extend_from_slice(&(value.len() as u32).to_le_bytes());
        payload.extend_from_slice(value.as_bytes());
        im3_record(name, 10, payload)
    }

    fn im3_float_scalar(name: &str, value: f32) -> Vec<u8> {
        im3_record(name, 3, value.to_le_bytes().to_vec())
    }

    fn im3_double_scalar(name: &str, value: f64) -> Vec<u8> {
        im3_record(name, 4, value.to_le_bytes().to_vec())
    }

    fn im3_java_float_array(name: &str, values: &[f32]) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&1u32.to_le_bytes());
        payload.extend_from_slice(&(values.len() as u32).to_le_bytes());
        for value in values {
            payload.extend_from_slice(&value.to_le_bytes());
        }
        im3_record(name, 7, payload)
    }

    fn im3_java_float_scalar(name: &str, value: f32) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&0u32.to_le_bytes());
        payload.extend_from_slice(&value.to_le_bytes());
        im3_record(name, 7, payload)
    }

    fn im3_data_record(size_x: u32, size_y: u32, size_c: u32, pixels: &[u8]) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&0u32.to_le_bytes());
        payload.extend_from_slice(&size_x.to_le_bytes());
        payload.extend_from_slice(&size_y.to_le_bytes());
        payload.extend_from_slice(&size_c.to_le_bytes());
        payload.extend_from_slice(pixels);
        im3_record("Data", 1, payload)
    }

    fn write_native_im3(path: &Path, size_x: u32, size_y: u32, size_c: u32, pixels: &[u8]) {
        let dataset = im3_container(
            "",
            vec![
                im3_int_array("Shape", &[size_x, size_y, size_c]),
                im3_data_record(size_x, size_y, size_c, pixels),
            ],
        );
        let data_set = im3_container("DataSet", vec![dataset]);
        let root = im3_container("Root", vec![data_set]);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1985u32.to_be_bytes());
        bytes.extend_from_slice(&root);
        std::fs::write(path, bytes).unwrap();
    }

    fn assert_synthetic_raw_reader<R: FormatReader>(mut reader: R, path: &Path, format_name: &str) {
        reader.set_id(path).expect("synthetic raw file");
        assert_eq!(reader.series_count(), 1);
        assert_eq!(reader.metadata().size_x, 3);
        assert_eq!(reader.metadata().size_y, 2);
        assert_eq!(reader.metadata().size_z, 1);
        assert_eq!(reader.metadata().size_c, 2);
        assert_eq!(reader.metadata().size_t, 1);
        assert_eq!(reader.metadata().image_count, 2);
        assert_eq!(reader.metadata().pixel_type, PixelType::Uint16);
        assert_eq!(reader.metadata().bits_per_pixel, 16);
        assert!(reader.metadata().is_little_endian);
        assert_eq!(
            reader.open_bytes(1).unwrap(),
            [10u16, 11, 12, 13, 14, 15]
                .into_iter()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            reader.open_bytes_region(1, 1, 0, 2, 2).unwrap(),
            [11u16, 12, 14, 15]
                .into_iter()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>()
        );
        assert!(matches!(
            reader.open_bytes(2),
            Err(BioFormatsError::PlaneOutOfRange(2))
        ));
        let err = reader.open_bytes_region(0, 2, 0, 2, 1).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::Format(ref message) if message.contains(format_name)),
            "unexpected region error: {err:?}"
        );
    }

    #[test]
    fn im3_reads_explicit_synthetic_raw_subset() {
        let path = temp_flim2_path("synthetic.im3");
        let payload = [0u16, 1, 2, 3, 4, 5, 10, 11, 12, 13, 14, 15]
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        write_synthetic_raw(
            &path,
            SYNTHETIC_IM3_MAGIC,
            (3, 2, 1, 2, 1),
            SYNTHETIC_RAW_U16,
            &payload,
        );

        assert!(Im3Reader::new().is_this_type_by_name(&path));
        assert!(Im3Reader::new().is_this_type_by_bytes(SYNTHETIC_IM3_MAGIC));
        assert_synthetic_raw_reader(Im3Reader::new(), &path, "IM3");

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn im3_reads_bounded_native_interleaved_uint16_channels_like_java() {
        let path = temp_flim2_path("native.im3");
        let pixels = [
            10u16, 100, 20, 200, //
            30, 300, 40, 400, //
            50, 500, 60, 600,
        ]
        .into_iter()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
        write_native_im3(&path, 3, 2, 2, &pixels);

        let mut reader = Im3Reader::new();
        assert!(reader.is_this_type_by_bytes(&1985u32.to_be_bytes()));
        reader.set_id(&path).expect("native IM3 fixture");
        assert_eq!(reader.series_count(), 1);
        assert_eq!(reader.metadata().size_x, 3);
        assert_eq!(reader.metadata().size_y, 2);
        assert_eq!(reader.metadata().size_z, 1);
        assert_eq!(reader.metadata().size_c, 2);
        assert_eq!(reader.metadata().size_t, 1);
        assert_eq!(reader.metadata().image_count, 2);
        assert_eq!(reader.metadata().pixel_type, PixelType::Uint16);
        assert_eq!(reader.metadata().bits_per_pixel, 16);
        assert!(reader.metadata().is_little_endian);
        assert_eq!(
            reader.open_bytes(0).unwrap(),
            [10u16, 20, 30, 40, 50, 60]
                .into_iter()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            reader.open_bytes(1).unwrap(),
            [100u16, 200, 300, 400, 500, 600]
                .into_iter()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            reader.open_bytes_region(1, 1, 0, 2, 2).unwrap(),
            [200u16, 300, 500, 600]
                .into_iter()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>()
        );
        assert!(matches!(
            reader.open_bytes(2),
            Err(BioFormatsError::PlaneOutOfRange(2))
        ));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn im3_reads_multi_dataset_native_as_series() {
        let multi = temp_flim2_path("native-multi.im3");
        let first_pixels = [1u16, 2]
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        let second_pixels = [10u16, 20, 30, 40]
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        let dataset_a = im3_container(
            "",
            vec![
                im3_int_array("Shape", &[2, 1, 1]),
                im3_data_record(2, 1, 1, &first_pixels),
            ],
        );
        let dataset_b = im3_container(
            "",
            vec![
                im3_int_array("Shape", &[2, 1, 2]),
                im3_data_record(2, 1, 2, &second_pixels),
            ],
        );
        let mut multi_bytes = Vec::new();
        multi_bytes.extend_from_slice(&1985u32.to_be_bytes());
        multi_bytes.extend_from_slice(&im3_container(
            "Root",
            vec![im3_container("DataSet", vec![dataset_a, dataset_b])],
        ));
        std::fs::write(&multi, multi_bytes).unwrap();

        let mut reader = Im3Reader::new();
        reader.set_id(&multi).expect("native multi-dataset IM3");
        assert_eq!(reader.series_count(), 2);
        assert_eq!(reader.series(), 0);
        assert_eq!(reader.metadata().size_x, 2);
        assert_eq!(reader.metadata().size_c, 1);
        assert_eq!(reader.metadata().image_count, 1);
        assert!(matches!(
            reader.metadata().series_metadata.get("IM3 DataSets"),
            Some(crate::common::metadata::MetadataValue::Int(2))
        ));
        assert!(matches!(
            reader.metadata().series_metadata.get("IM3 DataSet Index"),
            Some(crate::common::metadata::MetadataValue::Int(0))
        ));
        assert_eq!(reader.open_bytes(0).unwrap(), first_pixels);

        reader.set_series(1).unwrap();
        assert_eq!(reader.series(), 1);
        assert_eq!(reader.metadata().size_x, 2);
        assert_eq!(reader.metadata().size_c, 2);
        assert_eq!(reader.metadata().image_count, 2);
        assert!(matches!(
            reader.metadata().series_metadata.get("IM3 DataSet Index"),
            Some(crate::common::metadata::MetadataValue::Int(1))
        ));
        assert_eq!(
            reader.open_bytes(0).unwrap(),
            [10u16, 30]
                .into_iter()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            reader.open_bytes(1).unwrap(),
            [20u16, 40]
                .into_iter()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>()
        );
        assert!(matches!(
            reader.set_series(2),
            Err(BioFormatsError::SeriesOutOfRange(2))
        ));
        let _ = std::fs::remove_file(multi);
    }

    #[test]
    fn im3_preserves_bounded_native_scalar_metadata() {
        let path = temp_flim2_path("native-metadata.im3");
        let pixels = [7u16, 70, 700, 11, 110, 1100]
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        let dataset = im3_container(
            "",
            vec![
                im3_int_array("Shape", &[2, 1, 3]),
                im3_int_scalar("Exposure Time", 125),
                im3_int_array("Channel Wavelengths", &[420, 520, 620]),
                im3_string_scalar("Channel Names", "DAPI,FITC,Cy5"),
                im3_int_scalar("Camera-Gain#1", 9),
                im3_string_scalar("Instrument Name", "ImageStream X"),
                im3_float_scalar("Exposure Seconds", 0.125),
                im3_double_scalar("Laser Power", 4.5),
                im3_data_record(2, 1, 3, &pixels),
            ],
        );
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1985u32.to_be_bytes());
        bytes.extend_from_slice(&im3_container(
            "Root",
            vec![im3_container("DataSet", vec![dataset])],
        ));
        std::fs::write(&path, bytes).unwrap();

        let mut reader = Im3Reader::new();
        reader.set_id(&path).expect("native IM3 metadata fixture");
        let metadata = &reader.metadata().series_metadata;
        assert!(matches!(
            metadata.get("im3.native.exposure_time"),
            Some(crate::common::metadata::MetadataValue::Int(125))
        ));
        assert!(matches!(
            metadata.get("im3.native.channel_wavelengths"),
            Some(crate::common::metadata::MetadataValue::String(value)) if value == "420,520,620"
        ));
        assert!(matches!(
            metadata.get("im3.channel.0.emission_wavelength"),
            Some(crate::common::metadata::MetadataValue::Float(value)) if (*value - 420.0).abs() < 1e-12
        ));
        assert!(matches!(
            metadata.get("im3.channel.2.emission_wavelength"),
            Some(crate::common::metadata::MetadataValue::Float(value)) if (*value - 620.0).abs() < 1e-12
        ));
        assert!(matches!(
            metadata.get("im3.channel.0.name"),
            Some(crate::common::metadata::MetadataValue::String(value)) if value == "DAPI"
        ));
        assert!(matches!(
            metadata.get("im3.channel.2.name"),
            Some(crate::common::metadata::MetadataValue::String(value)) if value == "Cy5"
        ));
        assert!(matches!(
            metadata.get("im3.native.camera_gain_1"),
            Some(crate::common::metadata::MetadataValue::Int(9))
        ));
        assert!(matches!(
            metadata.get("im3.acquisition.camera_gain"),
            Some(crate::common::metadata::MetadataValue::Float(value)) if (*value - 9.0).abs() < 1e-12
        ));
        assert!(matches!(
            metadata.get("im3.native.instrument_name"),
            Some(crate::common::metadata::MetadataValue::String(value)) if value == "ImageStream X"
        ));
        assert!(matches!(
            metadata.get("im3.instrument.name"),
            Some(crate::common::metadata::MetadataValue::String(value)) if value == "ImageStream X"
        ));
        assert!(matches!(
            metadata.get("im3.native.exposure_seconds"),
            Some(crate::common::metadata::MetadataValue::Float(value)) if (*value - 0.125).abs() < 1e-12
        ));
        assert!(matches!(
            metadata.get("im3.acquisition.exposure_seconds"),
            Some(crate::common::metadata::MetadataValue::Float(value)) if (*value - 0.125).abs() < 1e-12
        ));
        assert!(matches!(
            metadata.get("im3.native.laser_power"),
            Some(crate::common::metadata::MetadataValue::Float(value)) if (*value - 4.5).abs() < 1e-12
        ));
        assert!(matches!(
            metadata.get("im3.acquisition.laser_power"),
            Some(crate::common::metadata::MetadataValue::Float(value)) if (*value - 4.5).abs() < 1e-12
        ));
        let modulo = reader
            .metadata()
            .modulo_c
            .as_ref()
            .expect("IM3 channel wavelength modulo");
        assert_eq!(modulo.parent_dimension, "C");
        assert_eq!(modulo.modulo_type, "lambda");
        assert_eq!(modulo.unit, "nm");
        assert_eq!(modulo.labels, ["420 nm", "520 nm", "620 nm"]);
        assert!((modulo.start - 420.0).abs() < 1e-12);
        assert!((modulo.step - 100.0).abs() < 1e-12);
        assert!((modulo.end - 620.0).abs() < 1e-12);
        let ome = reader.ome_metadata().expect("IM3 OME metadata");
        let original = ome
            .annotations
            .iter()
            .find_map(|annotation| match annotation {
                crate::common::ome_metadata::OmeAnnotation::MapAnnotation {
                    id: Some(id),
                    values,
                    ..
                } if id == "Annotation:OriginalMetadata:0" => Some(values),
                _ => None,
            })
            .expect("IM3 original metadata annotation");
        assert!(original
            .iter()
            .any(|(key, value)| key == "im3.instrument.name" && value == "ImageStream X"));
        assert!(original
            .iter()
            .any(|(key, value)| { key == "im3.channel.1.emission_wavelength" && value == "520" }));
        assert_eq!(
            reader.open_bytes(0).unwrap(),
            [7u16, 11]
                .into_iter()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>()
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn im3_reports_bounded_unsupported_native_metadata_records() {
        let path = temp_flim2_path("native-unsupported-metadata.im3");
        let pixels = [5u16, 50]
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        let dataset = im3_container(
            "",
            vec![
                im3_int_array("Shape", &[1, 1, 2]),
                im3_string_scalar("Channel Names", "A,B"),
                im3_record("Vendor Object", 99, vec![1, 2, 3, 4]),
                im3_record("Packed Metadata", 3, vec![0, 1]),
                im3_data_record(1, 1, 2, &pixels),
            ],
        );
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1985u32.to_be_bytes());
        bytes.extend_from_slice(&im3_container(
            "Root",
            vec![im3_container("DataSet", vec![dataset])],
        ));
        std::fs::write(&path, bytes).unwrap();

        let mut reader = Im3Reader::new();
        reader
            .set_id(&path)
            .expect("native IM3 unsupported metadata fixture");
        let metadata = &reader.metadata().series_metadata;
        assert!(matches!(
            metadata.get("im3.channel.0.name"),
            Some(crate::common::metadata::MetadataValue::String(value)) if value == "A"
        ));
        assert!(matches!(
            metadata.get("im3.native.unsupported_metadata_record_count"),
            Some(crate::common::metadata::MetadataValue::Int(2))
        ));
        assert!(matches!(
            metadata.get("im3.native.unsupported_metadata_records"),
            Some(crate::common::metadata::MetadataValue::String(value))
                if value.contains("Vendor Object(type=99,len=4)")
                    && value.contains("Packed Metadata(type=3,len=2)")
        ));
        assert_eq!(
            reader.open_bytes(1).unwrap(),
            [50u16]
                .into_iter()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>()
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn im3_parses_java_string_and_float_record_types() {
        let path = temp_flim2_path("native-java-record-types.im3");
        let pixels = [5u16, 50]
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        let dataset = im3_container(
            "",
            vec![
                im3_int_array("Shape", &[1, 1, 2]),
                im3_java_string_scalar("Instrument Name", "Nuance FX"),
                im3_java_float_scalar("Exposure Seconds", 0.25),
                im3_java_float_array("Wavelengths", &[450.0, 550.0]),
                im3_data_record(1, 1, 2, &pixels),
            ],
        );
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1985u32.to_be_bytes());
        bytes.extend_from_slice(&im3_container(
            "Root",
            vec![im3_container("DataSet", vec![dataset])],
        ));
        std::fs::write(&path, bytes).unwrap();

        let mut reader = Im3Reader::new();
        reader
            .set_id(&path)
            .expect("native IM3 Java-style record fixture");
        let metadata = &reader.metadata().series_metadata;
        assert!(matches!(
            metadata.get("im3.native.instrument_name"),
            Some(crate::common::metadata::MetadataValue::String(value)) if value == "Nuance FX"
        ));
        assert!(matches!(
            metadata.get("im3.instrument.name"),
            Some(crate::common::metadata::MetadataValue::String(value)) if value == "Nuance FX"
        ));
        assert!(matches!(
            metadata.get("im3.native.exposure_seconds"),
            Some(crate::common::metadata::MetadataValue::Float(value)) if (*value - 0.25).abs() < 1e-12
        ));
        assert!(matches!(
            metadata.get("im3.native.wavelengths"),
            Some(crate::common::metadata::MetadataValue::String(value))
                if value == "450,550"
        ));
        assert_eq!(
            reader.open_bytes(1).unwrap(),
            [50u16]
                .into_iter()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>()
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn im3_reports_bounded_unsupported_nested_native_metadata_records() {
        let path = temp_flim2_path("native-unsupported-nested-metadata.im3");
        let pixels = [9u16, 90]
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        let dataset = im3_container(
            "",
            vec![
                im3_int_array("Shape", &[1, 1, 2]),
                im3_container(
                    "Spectral Library",
                    vec![
                        im3_string_scalar("Library Name", "Synthetic Spectra"),
                        im3_record("Packed Entry", 99, vec![1, 2, 3]),
                    ],
                ),
                im3_data_record(1, 1, 2, &pixels),
            ],
        );
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1985u32.to_be_bytes());
        bytes.extend_from_slice(&im3_container(
            "Root",
            vec![im3_container("DataSet", vec![dataset])],
        ));
        std::fs::write(&path, bytes).unwrap();

        let mut reader = Im3Reader::new();
        reader
            .set_id(&path)
            .expect("native IM3 nested unsupported metadata fixture");
        let metadata = &reader.metadata().series_metadata;
        assert!(matches!(
            metadata.get("im3.native.unsupported_metadata_record_count"),
            Some(crate::common::metadata::MetadataValue::Int(1))
        ));
        assert!(matches!(
            metadata.get("im3.native.unsupported_metadata_records"),
            Some(crate::common::metadata::MetadataValue::String(value))
                if value.contains("Spectral Library(type=0,")
                    && value.contains("children=2")
        ));
        assert_eq!(
            reader.open_bytes(1).unwrap(),
            [90u16]
                .into_iter()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>()
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn im3_native_unsupported_variants_stay_explicit() {
        let mismatch = temp_flim2_path("native-mismatch.im3");
        let dataset = im3_container(
            "",
            vec![
                im3_int_array("Shape", &[2, 1, 1]),
                im3_data_record(1, 1, 1, &[0, 0]),
            ],
        );
        let mut mismatch_bytes = Vec::new();
        mismatch_bytes.extend_from_slice(&1985u32.to_be_bytes());
        mismatch_bytes.extend_from_slice(&im3_container(
            "Root",
            vec![im3_container("DataSet", vec![dataset])],
        ));
        std::fs::write(&mismatch, mismatch_bytes).unwrap();
        let err = Im3Reader::new().set_id(&mismatch).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("Shape/Data mismatch")),
            "unexpected native IM3 mismatch error: {err:?}"
        );
        let _ = std::fs::remove_file(mismatch);
    }

    #[test]
    fn slidebook7_reads_explicit_synthetic_raw_subset() {
        let path = temp_flim2_path("synthetic.sld");
        let payload = [0u16, 1, 2, 3, 4, 5, 10, 11, 12, 13, 14, 15]
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        write_synthetic_raw(
            &path,
            SYNTHETIC_SLIDEBOOK7_MAGIC,
            (3, 2, 1, 2, 1),
            SYNTHETIC_RAW_U16,
            &payload,
        );

        assert!(SlideBook7Reader::new().is_this_type_by_name(&path));
        assert!(SlideBook7Reader::new().is_this_type_by_bytes(SYNTHETIC_SLIDEBOOK7_MAGIC));
        assert_synthetic_raw_reader(SlideBook7Reader::new(), &path, "SlideBook 7");

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn slidebook7_reads_bounded_native_sldy_npy_planes() {
        let path = temp_flim2_path("native.sldy");
        write_native_slidebook7(
            &path,
            "Capture",
            (2, 2, 2, 2, 1),
            &[
                ((0, 0), vec![0, 1, 2, 3, 10, 11, 12, 13]),
                ((1, 0), vec![100, 101, 102, 103, 110, 111, 112, 113]),
            ],
        );

        let mut reader = SlideBook7Reader::new();
        reader.set_id(&path).expect("native SlideBook 7 fixture");
        assert_eq!(reader.series_count(), 1);
        assert_eq!(reader.metadata().size_x, 2);
        assert_eq!(reader.metadata().size_y, 2);
        assert_eq!(reader.metadata().size_z, 2);
        assert_eq!(reader.metadata().size_c, 2);
        assert_eq!(reader.metadata().size_t, 1);
        assert_eq!(reader.metadata().image_count, 4);
        assert_eq!(reader.metadata().pixel_type, PixelType::Uint16);
        assert!(reader.metadata().is_little_endian);
        assert_eq!(
            reader.open_bytes(2).unwrap(),
            [100u16, 101, 102, 103]
                .into_iter()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            reader.open_bytes_region(3, 1, 0, 1, 2).unwrap(),
            [111u16, 113]
                .into_iter()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>()
        );
        let ome = reader.ome_metadata().expect("SlideBook 7 OME metadata");
        let original = ome
            .annotations
            .iter()
            .find_map(|annotation| match annotation {
                crate::common::ome_metadata::OmeAnnotation::MapAnnotation {
                    id: Some(id),
                    values,
                    ..
                } if id == "Annotation:OriginalMetadata:0" => Some(values),
                _ => None,
            })
            .expect("SlideBook 7 original metadata annotation");
        assert!(original
            .iter()
            .any(|(key, value)| key == "slidebook7.record.mwidth" && value == "2"));
        assert!(original
            .iter()
            .any(|(key, value)| key == "slidebook7.record.mnumchannels" && value == "2"));
        assert!(matches!(
            reader.open_bytes(4),
            Err(BioFormatsError::PlaneOutOfRange(4))
        ));

        let _ = std::fs::remove_dir_all(path.with_extension("dir"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn slidebook7_reads_native_sldy_npyz_gzip_payload() {
        let path = temp_flim2_path("native-npyz.sldy");
        std::fs::write(&path, b"SlideBook 7 native placeholder").unwrap();
        let root = path.with_extension("dir");
        let group = root.join("Capture.imgdir");
        std::fs::create_dir_all(&group).unwrap();
        std::fs::write(
            group.join("ImageRecord.yaml"),
            "mWidth: 2\nmHeight: 2\nmNumPlanes: 1\nmNumChannels: 1\nmNumTimepoints: 2\n",
        )
        .unwrap();
        let payload = [10u16, 11, 12, 13, 20, 21, 22, 23]
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        write_slidebook7_npyz(
            &group.join("ImageData_Ch0_TP0000000.npyz"),
            "<u2",
            &[2, 2, 2],
            &payload,
        );

        let mut reader = SlideBook7Reader::new();
        reader
            .set_id(&path)
            .expect("native SlideBook 7 npyz fixture");
        assert_eq!(reader.metadata().size_x, 2);
        assert_eq!(reader.metadata().size_y, 2);
        assert_eq!(reader.metadata().size_z, 1);
        assert_eq!(reader.metadata().size_c, 1);
        assert_eq!(reader.metadata().size_t, 2);
        assert_eq!(reader.metadata().image_count, 2);
        assert_eq!(
            reader.open_bytes(1).unwrap(),
            [20u16, 21, 22, 23]
                .into_iter()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            reader.open_bytes_region(1, 1, 0, 1, 2).unwrap(),
            [21u16, 23]
                .into_iter()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>()
        );

        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn slidebook7_parses_multi_digit_channel_indices() {
        let path = temp_flim2_path("native-ch10.sldy");
        std::fs::write(&path, b"SlideBook 7 native placeholder").unwrap();
        let root = path.with_extension("dir");
        let group = root.join("Capture.imgdir");
        std::fs::create_dir_all(&group).unwrap();
        std::fs::write(
            group.join("ImageRecord.yaml"),
            "mWidth: 2\nmHeight: 1\nmNumPlanes: 1\nmNumChannels: 11\nmNumTimepoints: 1\n",
        )
        .unwrap();
        let payload = [100u16, 110]
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        write_slidebook7_npy(
            &group.join("ImageData_Ch10_TP0000000.npy"),
            "<u2",
            &[1, 2],
            &payload,
        );

        let mut reader = SlideBook7Reader::new();
        reader
            .set_id(&path)
            .expect("native SlideBook 7 Ch10 fixture");
        assert_eq!(reader.metadata().size_c, 11);
        assert_eq!(reader.metadata().image_count, 11);
        assert_eq!(reader.open_bytes(10).unwrap(), payload);
        assert!(matches!(
            reader.open_bytes(9),
            Err(BioFormatsError::UnsupportedFormat(ref message))
                if message.contains("missing ImageData plane")
        ));

        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn slidebook7_rejects_sldyz_archives_without_supported_native_groups() {
        let path = temp_flim2_path("compressed.sldyz");
        let file = File::create(&path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        zip.start_file("notes.txt", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"not a SlideBook 7 native group").unwrap();
        zip.finish().unwrap();
        let err = SlideBook7Reader::new().set_id(&path).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("no image groups")),
            "unexpected SlideBook 7 sldyz unsupported error: {err:?}"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn im3_and_slidebook7_preserve_unsupported_for_nonmatching_files() {
        let im3 = temp_flim2_path("realish.im3");
        std::fs::write(&im3, b"not the synthetic im3 raw magic").unwrap();
        let err = Im3Reader::new().set_id(&im3).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("IM3 proprietary")),
            "unexpected IM3 unsupported error: {err:?}"
        );
        assert!(!Im3Reader::new().is_this_type_by_bytes(b"not the synthetic im3 raw magic"));
        let _ = std::fs::remove_file(im3);

        let sld = temp_flim2_path("realish.sld");
        std::fs::write(&sld, b"not the synthetic slidebook raw magic").unwrap();
        let err = SlideBook7Reader::new().set_id(&sld).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("SlideBook 7 native")),
            "unexpected SlideBook unsupported error: {err:?}"
        );
        assert!(!SlideBook7Reader::new()
            .is_this_type_by_bytes(b"not the synthetic slidebook raw magic"));
        let _ = std::fs::remove_file(sld);
    }

    #[test]
    fn ivision_reads_explicit_synthetic_raw_subset() {
        let path = temp_flim2_path("synthetic.ipm");
        let payload = [0u16, 1, 2, 3, 4, 5, 10, 11, 12, 13, 14, 15]
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        write_synthetic_raw(
            &path,
            SYNTHETIC_IVISION_MAGIC,
            (3, 2, 1, 2, 1),
            SYNTHETIC_RAW_U16,
            &payload,
        );

        assert!(IvisionReader::new().is_this_type_by_name(&path));
        assert!(IvisionReader::new().is_this_type_by_bytes(SYNTHETIC_IVISION_MAGIC));
        assert_synthetic_raw_reader(IvisionReader::new(), &path, "iVision IPM");

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn ivision_reads_bounded_native_big_endian_planes_like_java() {
        let path = temp_flim2_path("native.ipm");
        let payload = [
            0x0102u16, 0x0304, 0x0506, 0x0708, 0x1112, 0x1314, 0x1516, 0x1718,
        ]
        .into_iter()
        .flat_map(u16::to_be_bytes)
        .collect::<Vec<_>>();
        write_native_ivision(&path, 6, 2, 2, 2, &payload);

        let mut reader = IvisionReader::new();
        assert!(reader.is_this_type_by_bytes(b"1.0A\0\x06"));
        reader.set_id(&path).expect("native iVision fixture");
        assert_eq!(reader.series_count(), 1);
        assert_eq!(reader.metadata().size_x, 2);
        assert_eq!(reader.metadata().size_y, 2);
        assert_eq!(reader.metadata().size_z, 2);
        assert_eq!(reader.metadata().size_c, 1);
        assert_eq!(reader.metadata().size_t, 1);
        assert_eq!(reader.metadata().image_count, 2);
        assert_eq!(reader.metadata().pixel_type, PixelType::Uint16);
        assert_eq!(reader.metadata().bits_per_pixel, 16);
        assert!(!reader.metadata().is_little_endian);
        let native_metadata = &reader.metadata().series_metadata;
        assert!(matches!(
            native_metadata.get("iVision Version"),
            Some(crate::common::metadata::MetadataValue::String(version)) if version == "1.0A"
        ));
        assert!(matches!(
            native_metadata.get("iVision FileFormat"),
            Some(crate::common::metadata::MetadataValue::Int(1))
        ));
        assert!(matches!(
            native_metadata.get("iVision DataType"),
            Some(crate::common::metadata::MetadataValue::Int(6))
        ));
        assert!(matches!(
            native_metadata.get("iVision DataType Name"),
            Some(crate::common::metadata::MetadataValue::String(name)) if name == "16-bit unsigned mono"
        ));
        assert!(matches!(
            native_metadata.get("iVision Native Width"),
            Some(crate::common::metadata::MetadataValue::Int(2))
        ));
        assert!(matches!(
            native_metadata.get("iVision Native Height"),
            Some(crate::common::metadata::MetadataValue::Int(2))
        ));
        assert!(matches!(
            native_metadata.get("iVision Native Z Sections"),
            Some(crate::common::metadata::MetadataValue::Int(2))
        ));
        assert!(matches!(
            native_metadata.get("iVision Image Offset"),
            Some(crate::common::metadata::MetadataValue::Int(2120))
        ));
        assert!(matches!(
            native_metadata.get("iVision Disk Plane Bytes"),
            Some(crate::common::metadata::MetadataValue::Int(8))
        ));
        assert!(matches!(
            native_metadata.get("iVision Output Plane Bytes"),
            Some(crate::common::metadata::MetadataValue::Int(8))
        ));
        assert!(matches!(
            native_metadata.get("iVision Has Padding Byte"),
            Some(crate::common::metadata::MetadataValue::Bool(false))
        ));
        assert_eq!(
            reader.open_bytes(1).unwrap(),
            [0x1112u16, 0x1314, 0x1516, 0x1718]
                .into_iter()
                .flat_map(u16::to_be_bytes)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            reader.open_bytes_region(1, 1, 0, 1, 2).unwrap(),
            [0x1314u16, 0x1718]
                .into_iter()
                .flat_map(u16::to_be_bytes)
                .collect::<Vec<_>>()
        );
        assert!(matches!(
            reader.open_bytes(2),
            Err(BioFormatsError::PlaneOutOfRange(2))
        ));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn ivision_native_padding_byte_rgb_planes_are_stripped() {
        let path = temp_flim2_path("native-padding.ipm");
        let payload = [
            0, 1, 2, 3, //
            0, 4, 5, 6, //
        ];
        write_native_ivision(&path, 5, 2, 1, 1, &payload);

        let mut reader = IvisionReader::new();
        reader.set_id(&path).expect("native padded iVision fixture");
        assert_eq!(reader.metadata().size_x, 2);
        assert_eq!(reader.metadata().size_y, 1);
        assert_eq!(reader.metadata().size_c, 3);
        assert_eq!(reader.metadata().pixel_type, PixelType::Uint8);
        assert!(matches!(
            reader
                .metadata()
                .series_metadata
                .get("iVision Has Padding Byte"),
            Some(crate::common::metadata::MetadataValue::Bool(true))
        ));
        assert_eq!(reader.open_bytes(0).unwrap(), [1, 2, 3, 4, 5, 6]);
        assert_eq!(reader.open_bytes_region(0, 1, 0, 1, 1).unwrap(), [4, 5, 6]);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn ivision_native_unsigned_16bit_rgb_planes_are_bounded() {
        let path = temp_flim2_path("native-u16-rgb.ipm");
        let payload = [
            0x0102u16, 0x0304, 0x0506, //
            0x1112, 0x1314, 0x1516, //
            0x2122, 0x2324, 0x2526, //
            0x3132, 0x3334, 0x3536, //
        ]
        .into_iter()
        .flat_map(u16::to_be_bytes)
        .collect::<Vec<_>>();
        write_native_ivision(&path, 8, 2, 2, 1, &payload);

        let mut reader = IvisionReader::new();
        reader
            .set_id(&path)
            .expect("native unsigned 16-bit RGB iVision fixture");
        assert_eq!(reader.metadata().size_x, 2);
        assert_eq!(reader.metadata().size_y, 2);
        assert_eq!(reader.metadata().size_c, 3);
        assert_eq!(reader.metadata().pixel_type, PixelType::Uint16);
        assert_eq!(reader.metadata().bits_per_pixel, 16);
        assert!(!reader.metadata().is_little_endian);
        let native_metadata = &reader.metadata().series_metadata;
        assert!(matches!(
            native_metadata.get("iVision DataType Name"),
            Some(crate::common::metadata::MetadataValue::String(name)) if name == "16-bit unsigned color"
        ));
        assert!(matches!(
            native_metadata.get("iVision Samples Per Pixel"),
            Some(crate::common::metadata::MetadataValue::Int(3))
        ));
        assert!(matches!(
            native_metadata.get("iVision Storage Layout"),
            Some(crate::common::metadata::MetadataValue::String(layout))
                if layout == "big-endian unsigned 16-bit RGB samples"
        ));
        assert!(matches!(
            native_metadata.get("iVision Disk Plane Bytes"),
            Some(crate::common::metadata::MetadataValue::Int(24))
        ));
        assert_eq!(reader.open_bytes(0).unwrap(), payload);
        assert_eq!(
            reader.open_bytes_region(0, 1, 0, 1, 2).unwrap(),
            [0x1112u16, 0x1314, 0x1516, 0x3132, 0x3334, 0x3536]
                .into_iter()
                .flat_map(u16::to_be_bytes)
                .collect::<Vec<_>>()
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn ivision_native_unsupported_variants_stay_explicit() {
        let path = temp_flim2_path("native-sqrt.ipm");
        write_native_ivision(&path, 7, 2, 1, 1, &[0x01, 0x00, 0x04, 0x00]);

        let mut reader = IvisionReader::new();
        reader.set_id(&path).expect("square-root iVision metadata");
        assert_eq!(reader.metadata().size_x, 2);
        assert_eq!(reader.metadata().size_y, 1);
        assert_eq!(reader.metadata().size_c, 1);
        assert_eq!(reader.metadata().pixel_type, PixelType::Float32);
        assert_eq!(reader.metadata().bits_per_pixel, 32);
        let native_metadata = &reader.metadata().series_metadata;
        assert!(matches!(
            native_metadata.get("iVision DataType Name"),
            Some(crate::common::metadata::MetadataValue::String(name)) if name == "square-root float"
        ));
        assert!(matches!(
            native_metadata.get("iVision Storage Layout"),
            Some(crate::common::metadata::MetadataValue::String(layout))
                if layout == "big-endian square-root encoded 16-bit samples with float output"
        ));
        assert!(matches!(
            native_metadata.get("iVision Disk Plane Bytes"),
            Some(crate::common::metadata::MetadataValue::Int(4))
        ));
        assert!(matches!(
            native_metadata.get("iVision Output Plane Bytes"),
            Some(crate::common::metadata::MetadataValue::Int(8))
        ));
        let err = reader.open_bytes(0).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::UnsupportedFormat(ref message)
                if message.contains("Square-root iVision pixel decoding is not supported")
                    && message.contains("transfer curve unimplemented")),
            "unexpected iVision native unsupported error: {err:?}"
        );
        let err = reader.open_bytes_region(0, 0, 0, 1, 1).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::UnsupportedFormat(ref message)
                if message.contains("Square-root iVision pixel decoding is not supported")),
            "unexpected iVision native region unsupported error: {err:?}"
        );

        let _ = std::fs::remove_file(path);

        let color = temp_flim2_path("native-16bit-color.ipm");
        write_native_ivision(&color, 4, 2, 1, 1, &[0x7c, 0x00, 0x07, 0xe0]);

        let mut reader = IvisionReader::new();
        reader
            .set_id(&color)
            .expect("packed 16-bit color iVision metadata");
        assert_eq!(reader.metadata().size_x, 2);
        assert_eq!(reader.metadata().size_y, 1);
        assert_eq!(reader.metadata().size_c, 3);
        assert_eq!(reader.metadata().pixel_type, PixelType::Uint8);
        assert_eq!(reader.metadata().bits_per_pixel, 8);
        let native_metadata = &reader.metadata().series_metadata;
        assert!(matches!(
            native_metadata.get("iVision DataType Name"),
            Some(crate::common::metadata::MetadataValue::String(name)) if name == "16-bit color"
        ));
        assert!(matches!(
            native_metadata.get("iVision Storage Layout"),
            Some(crate::common::metadata::MetadataValue::String(layout))
                if layout == "packed 16-bit color samples with unresolved RGB555/RGB565 masks"
        ));
        assert!(matches!(
            native_metadata.get("iVision Disk Plane Bytes"),
            Some(crate::common::metadata::MetadataValue::Int(4))
        ));
        assert!(matches!(
            native_metadata.get("iVision Output Plane Bytes"),
            Some(crate::common::metadata::MetadataValue::Int(6))
        ));
        let err = reader.open_bytes(0).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::UnsupportedFormat(ref message)
                if message.contains("Packed 16-bit color iVision pixel decoding is not supported")
                    && message.contains("does not identify RGB555 vs RGB565")),
            "unexpected iVision native color error: {err:?}"
        );
        let err = reader.open_bytes_region(0, 0, 0, 1, 1).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::UnsupportedFormat(ref message)
                if message.contains("channel bit order")),
            "unexpected iVision native color region error: {err:?}"
        );

        let _ = std::fs::remove_file(color);

        let unknown = temp_flim2_path("native-unknown-type.ipm");
        write_native_ivision(&unknown, 9, 1, 1, 1, &[]);

        let err = IvisionReader::new().set_id(&unknown).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("native data type 9 is unsupported")),
            "unexpected iVision native unknown type error: {err:?}"
        );

        let _ = std::fs::remove_file(unknown);
    }

    #[test]
    fn ivision_preserves_unsupported_for_nonmatching_files() {
        let path = temp_flim2_path("realish.ipm");
        std::fs::write(&path, b"not the synthetic ivision raw magic").unwrap();
        let err = IvisionReader::new().set_id(&path).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("iVision IPM")),
            "unexpected iVision unsupported error: {err:?}"
        );
        assert!(!IvisionReader::new().is_this_type_by_bytes(b"not the synthetic ivision raw magic"));
        let mut reader = IvisionReader::new();
        assert_eq!(reader.series_count(), 0);
        assert_eq!(reader.metadata().size_x, 0);
        assert!(matches!(
            reader.open_bytes(0),
            Err(BioFormatsError::NotInitialized)
        ));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn synthetic_raw_subset_validates_header_and_payload() {
        let zero_width = temp_flim2_path("zero-width.im3");
        write_synthetic_raw(
            &zero_width,
            SYNTHETIC_IM3_MAGIC,
            (0, 2, 1, 1, 1),
            SYNTHETIC_RAW_U8,
            &[0, 1],
        );
        let err = Im3Reader::new().set_id(&zero_width).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::Format(ref message) if message.contains("width")),
            "unexpected zero-width error: {err:?}"
        );
        let _ = std::fs::remove_file(zero_width);

        let unsupported_pixel = temp_flim2_path("pixel.im3");
        write_synthetic_raw(
            &unsupported_pixel,
            SYNTHETIC_IM3_MAGIC,
            (1, 1, 1, 1, 1),
            99,
            &[0],
        );
        let err = Im3Reader::new().set_id(&unsupported_pixel).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("pixel type")),
            "unexpected pixel type error: {err:?}"
        );
        let _ = std::fs::remove_file(unsupported_pixel);

        let short_payload = temp_flim2_path("short-payload.sld");
        write_synthetic_raw(
            &short_payload,
            SYNTHETIC_SLIDEBOOK7_MAGIC,
            (2, 2, 1, 1, 1),
            SYNTHETIC_RAW_U16,
            &[0, 1],
        );
        let err = SlideBook7Reader::new().set_id(&short_payload).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::InvalidData(ref message) if message.contains("payload length")),
            "unexpected payload length error: {err:?}"
        );
        let _ = std::fs::remove_file(short_payload);
    }

    fn short_entry(tag: u16, value: u16) -> TestEntry {
        TestEntry {
            tag,
            typ: 3,
            count: 1,
            value: value.to_le_bytes().to_vec(),
        }
    }

    fn long_entry(tag: u16, value: u32) -> TestEntry {
        TestEntry {
            tag,
            typ: 4,
            count: 1,
            value: value.to_le_bytes().to_vec(),
        }
    }

    fn ascii_entry(tag: u16, value: &str) -> TestEntry {
        let mut bytes = value.as_bytes().to_vec();
        bytes.push(0);
        TestEntry {
            tag,
            typ: 2,
            count: bytes.len() as u32,
            value: bytes,
        }
    }

    fn double_entry(tag: u16, value: f64) -> TestEntry {
        TestEntry {
            tag,
            typ: 12,
            count: 1,
            value: value.to_le_bytes().to_vec(),
        }
    }

    fn ifd_table_len(entry_count: usize) -> usize {
        2 + entry_count * 12 + 4
    }

    fn ifd_extra_len(entries: &[TestEntry]) -> usize {
        entries
            .iter()
            .map(|entry| {
                if entry.value.len() > 4 {
                    entry.value.len()
                } else {
                    0
                }
            })
            .sum()
    }

    fn write_test_ifd(
        out: &mut Vec<u8>,
        entries: &[TestEntry],
        ifd_offset: usize,
        next_ifd_offset: u32,
    ) {
        let mut entries = entries
            .iter()
            .map(|entry| (entry.tag, entry))
            .collect::<Vec<_>>();
        entries.sort_by_key(|(tag, _)| *tag);
        let mut extra = Vec::new();
        let extra_base = ifd_offset + ifd_table_len(entries.len());

        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        for (_, entry) in entries {
            out.extend_from_slice(&entry.tag.to_le_bytes());
            out.extend_from_slice(&entry.typ.to_le_bytes());
            out.extend_from_slice(&entry.count.to_le_bytes());
            if entry.value.len() <= 4 {
                let mut inline = [0u8; 4];
                inline[..entry.value.len()].copy_from_slice(&entry.value);
                out.extend_from_slice(&inline);
            } else {
                let offset = (extra_base + extra.len()) as u32;
                out.extend_from_slice(&offset.to_le_bytes());
                extra.extend_from_slice(&entry.value);
            }
        }
        out.extend_from_slice(&next_ifd_offset.to_le_bytes());
        out.extend_from_slice(&extra);
    }

    fn write_synthetic_flowsight_cif(
        path: &Path,
        bits_per_sample: u16,
        compression: u16,
        compressed: &[u8],
    ) {
        let ifd0_entries = vec![
            short_entry(FLOWSIGHT_CHANNEL_COUNT_TAG, 2),
            ascii_entry(FLOWSIGHT_CHANNEL_NAMES_TAG, "BF|SSC"),
            ascii_entry(FLOWSIGHT_CHANNEL_DESCS_TAG, "Brightfield|Scatter"),
            ascii_entry(
                FLOWSIGHT_METADATA_XML_TAG,
                "<Root><Imaging><ChannelInUseIndicators>1 1</ChannelInUseIndicators></Imaging></Root>",
            ),
        ];
        let ifd0_offset = 8usize;
        let ifd1_offset =
            ifd0_offset + ifd_table_len(ifd0_entries.len()) + ifd_extra_len(&ifd0_entries);
        let ifd1_entry_count = 7usize;
        let ifd1_entries = vec![
            long_entry(tag::IMAGE_WIDTH, 4),
            long_entry(tag::IMAGE_LENGTH, 1),
            short_entry(tag::BITS_PER_SAMPLE, bits_per_sample),
            short_entry(tag::COMPRESSION, compression),
            long_entry(tag::ROWS_PER_STRIP, 1),
            long_entry(
                tag::STRIP_OFFSETS,
                (ifd1_offset + ifd_table_len(ifd1_entry_count)) as u32,
            ),
            long_entry(tag::STRIP_BYTE_COUNTS, compressed.len() as u32),
        ];

        let mut data = Vec::new();
        data.extend_from_slice(b"II");
        data.extend_from_slice(&42u16.to_le_bytes());
        data.extend_from_slice(&(ifd0_offset as u32).to_le_bytes());
        write_test_ifd(&mut data, &ifd0_entries, ifd0_offset, ifd1_offset as u32);
        write_test_ifd(&mut data, &ifd1_entries, ifd1_offset, 0);
        data.extend_from_slice(compressed);

        let mut file = File::create(path).unwrap();
        file.write_all(&data).unwrap();
    }

    fn write_slidebook_tiff(path: &Path) {
        let mut entries = vec![
            long_entry(tag::IMAGE_WIDTH, 1),
            long_entry(tag::IMAGE_LENGTH, 1),
            short_entry(tag::BITS_PER_SAMPLE, 8),
            short_entry(tag::COMPRESSION, 1),
            short_entry(tag::PHOTOMETRIC_INTERPRETATION, 1),
            long_entry(tag::ROWS_PER_STRIP, 1),
            long_entry(tag::STRIP_BYTE_COUNTS, 1),
            ascii_entry(tag::SOFTWARE, "SlideBook"),
            ascii_entry(SLIDEBOOK_CHANNEL_TAG, "Channel: DAPI; raw"),
            double_entry(SLIDEBOOK_PHYSICAL_SIZE_TAG, 0.25),
            double_entry(SLIDEBOOK_MAGNIFICATION_TAG, 60.0),
            double_entry(SLIDEBOOK_X_POS_TAG, 1.5),
            double_entry(SLIDEBOOK_Y_POS_TAG, 2.5),
            double_entry(SLIDEBOOK_Z_POS_TAG, 3.5),
        ];
        let strip_offset = 8 + ifd_table_len(entries.len() + 1) + ifd_extra_len(&entries);
        entries.push(long_entry(tag::STRIP_OFFSETS, strip_offset as u32));

        let mut data = Vec::new();
        data.extend_from_slice(b"II");
        data.extend_from_slice(&42u16.to_le_bytes());
        data.extend_from_slice(&8u32.to_le_bytes());
        write_test_ifd(&mut data, &entries, 8, 0);
        data.resize(strip_offset, 0);
        data.push(0x7f);

        let mut file = File::create(path).unwrap();
        file.write_all(&data).unwrap();
    }

    fn write_one_pixel_tiff(path: &Path, value: u8) {
        let mut entries = vec![
            long_entry(tag::IMAGE_WIDTH, 1),
            long_entry(tag::IMAGE_LENGTH, 1),
            short_entry(tag::BITS_PER_SAMPLE, 8),
            short_entry(tag::COMPRESSION, 1),
            short_entry(tag::PHOTOMETRIC_INTERPRETATION, 1),
            long_entry(tag::ROWS_PER_STRIP, 1),
            long_entry(tag::STRIP_BYTE_COUNTS, 1),
        ];
        let strip_offset = 8 + ifd_table_len(entries.len() + 1) + ifd_extra_len(&entries);
        entries.push(long_entry(tag::STRIP_OFFSETS, strip_offset as u32));

        let mut data = Vec::new();
        data.extend_from_slice(b"II");
        data.extend_from_slice(&42u16.to_le_bytes());
        data.extend_from_slice(&8u32.to_le_bytes());
        write_test_ifd(&mut data, &entries, 8, 0);
        data.resize(strip_offset, 0);
        data.push(value);

        let mut file = File::create(path).unwrap();
        file.write_all(&data).unwrap();
    }

    fn write_one_pixel_png(path: &Path, value: u8) {
        let image = image::GrayImage::from_raw(1, 1, vec![value]).unwrap();
        image.save(path).unwrap();
    }

    fn write_one_pixel_bmp(path: &Path, red: u8, green: u8, blue: u8) {
        let mut data = Vec::new();
        data.extend_from_slice(b"BM");
        data.extend_from_slice(&58u32.to_le_bytes());
        data.extend_from_slice(&0u16.to_le_bytes());
        data.extend_from_slice(&0u16.to_le_bytes());
        data.extend_from_slice(&54u32.to_le_bytes());
        data.extend_from_slice(&40u32.to_le_bytes());
        data.extend_from_slice(&1i32.to_le_bytes());
        data.extend_from_slice(&1i32.to_le_bytes());
        data.extend_from_slice(&1u16.to_le_bytes());
        data.extend_from_slice(&24u16.to_le_bytes());
        data.extend_from_slice(&0u32.to_le_bytes());
        data.extend_from_slice(&4u32.to_le_bytes());
        data.extend_from_slice(&0i32.to_le_bytes());
        data.extend_from_slice(&0i32.to_le_bytes());
        data.extend_from_slice(&0u32.to_le_bytes());
        data.extend_from_slice(&0u32.to_le_bytes());
        data.extend_from_slice(&[blue, green, red, 0]);
        std::fs::write(path, data).unwrap();
    }

    fn utf16le(value: &str) -> Vec<u8> {
        value
            .encode_utf16()
            .flat_map(|unit| unit.to_le_bytes())
            .collect()
    }

    fn build_xlef_test_lof(width: u32, height: u32, pixels: &[u8]) -> Vec<u8> {
        let xml = format!(
            "<Image><ImageDescription>\
<Channels><ChannelDescription Resolution=\"8\" BytesInc=\"1\"/></Channels>\
<Dimensions>\
<DimensionDescription DimID=\"1\" NumberOfElements=\"{width}\" BytesInc=\"1\"/>\
<DimensionDescription DimID=\"2\" NumberOfElements=\"{height}\" BytesInc=\"{width}\"/>\
</Dimensions>\
</ImageDescription></Image>"
        );
        let xml_units = xml.encode_utf16().count() as i32;

        let mut b = Vec::new();
        b.extend_from_slice(&0x70i32.to_le_bytes());
        b.extend_from_slice(&0i32.to_le_bytes());
        b.push(0x2a);
        b.extend_from_slice(&15i32.to_le_bytes());
        b.extend_from_slice(&utf16le("LMS_Object_File"));
        b.push(0x2a);
        b.extend_from_slice(&2i32.to_le_bytes());
        b.push(0x2a);
        b.extend_from_slice(&0i32.to_le_bytes());
        b.push(0x2a);
        b.extend_from_slice(&(pixels.len() as i64).to_le_bytes());
        b.extend_from_slice(pixels);
        b.extend_from_slice(&0x70i32.to_le_bytes());
        b.extend_from_slice(&0i32.to_le_bytes());
        b.push(0x2a);
        b.extend_from_slice(&xml_units.to_le_bytes());
        b.extend_from_slice(&utf16le(&xml));
        b
    }

    fn build_xlef_strict_lms(width: u32, height: u32, pixel_type: u16, payload: &[u8]) -> Vec<u8> {
        let mut data = b"BIOFORMATS-RS-ZEISS-LMS-STRICT-RAW-V1\n".to_vec();
        data.extend_from_slice(&width.to_le_bytes());
        data.extend_from_slice(&height.to_le_bytes());
        data.extend_from_slice(&pixel_type.to_le_bytes());
        data.extend_from_slice(&0u16.to_le_bytes());
        data.extend_from_slice(payload);
        data
    }

    #[test]
    fn im3_and_ivision_native_byte_probes_match_java_headers() {
        assert!(Im3Reader::new().is_this_type_by_bytes(&1985u32.to_be_bytes()));
        assert!(!Im3Reader::new().is_this_type_by_bytes(&1985u32.to_le_bytes()));

        assert!(IvisionReader::new().is_this_type_by_bytes(b"1.0A\0\x03"));
        assert!(!IvisionReader::new().is_this_type_by_bytes(b"1-0A\0\x03"));
        assert!(!IvisionReader::new().is_this_type_by_bytes(b"1.0A\0\x09"));
    }

    #[test]
    fn slidebook7_accepts_java_native_suffixes() {
        let reader = SlideBook7Reader::new();
        assert!(reader.is_this_type_by_name(Path::new("dataset.sldy")));
        assert!(reader.is_this_type_by_name(Path::new("dataset.sldyz")));
    }

    #[test]
    fn xlef_references_single_tiff() {
        let xlef = temp_flim2_path("project.xlef");
        let tiff = xlef.with_file_name("image.tif");
        std::fs::write(&xlef, r#"<XLEF><Image File="image.tif"/></XLEF>"#).unwrap();
        let refs = xlef_referenced_paths(&std::fs::read_to_string(&xlef).unwrap(), &xlef);
        assert_eq!(refs, vec![tiff]);
        let images = XlefReader::referenced_images(&xlef).unwrap();
        assert_eq!(images, vec![XlefReference::Image(refs[0].clone())]);
        let _ = std::fs::remove_file(xlef);
    }

    #[test]
    fn xlef_opens_multiple_tiff_references_as_project_series() {
        let xlef = temp_flim2_path("multi.xlef");
        let tiff_a = xlef.with_file_name("a.tif");
        let tiff_b = xlef.with_file_name("b.tif");
        write_one_pixel_tiff(&tiff_a, 11);
        write_one_pixel_tiff(&tiff_b, 22);
        std::fs::write(
            &xlef,
            r#"<XLEF><Image File="a.tif"/><Image File="b.tif"/></XLEF>"#,
        )
        .unwrap();

        let mut reader = XlefReader::new();
        reader.set_id(&xlef).unwrap();
        assert_eq!(reader.series_count(), 2);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![11]);
        reader.set_series(1).unwrap();
        assert_eq!(reader.metadata().size_x, 1);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![22]);

        let _ = std::fs::remove_file(xlef);
        let _ = std::fs::remove_file(tiff_a);
        let _ = std::fs::remove_file(tiff_b);
    }

    #[test]
    fn xlef_follows_xlif_to_tiff_and_lof_leaves() {
        let xlef = temp_flim2_path("nested.xlef");
        let xlif = xlef.with_file_name("nested.xlif");
        let tiff = xlef.with_file_name("nested.tif");
        let lof = xlef.with_file_name("nested.lof");
        write_one_pixel_tiff(&tiff, 33);
        std::fs::write(&lof, build_xlef_test_lof(4, 1, &[1, 2, 3, 4])).unwrap();
        std::fs::write(
            &xlif,
            r#"<XLIF><Image File="nested.tif"/><Image File="nested.lof"/></XLIF>"#,
        )
        .unwrap();
        std::fs::write(&xlef, r#"<XLEF><Project File="nested.xlif"/></XLEF>"#).unwrap();

        let mut reader = XlefReader::new();
        reader.set_id(&xlef).unwrap();
        assert_eq!(reader.series_count(), 2);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![33]);
        reader.set_series(1).unwrap();
        assert_eq!(reader.metadata().size_x, 4);
        assert_eq!(reader.open_bytes_region(0, 1, 0, 2, 1).unwrap(), vec![2, 3]);

        let _ = std::fs::remove_file(xlef);
        let _ = std::fs::remove_file(xlif);
        let _ = std::fs::remove_file(tiff);
        let _ = std::fs::remove_file(lof);
    }

    #[test]
    fn xlef_opens_raster_png_and_bmp_leaves_as_project_series() {
        let xlef = temp_flim2_path("rasters.xlef");
        let xlif = xlef.with_file_name("rasters.xlif");
        let png = xlef.with_file_name("leaf.png");
        let bmp = xlef.with_file_name("leaf.bmp");
        write_one_pixel_png(&png, 77);
        write_one_pixel_bmp(&bmp, 10, 20, 30);
        std::fs::write(
            &xlif,
            r#"<XLIF><Image File="leaf.png"/><Image File="leaf.bmp"/></XLIF>"#,
        )
        .unwrap();
        std::fs::write(&xlef, r#"<XLEF><Project File="rasters.xlif"/></XLEF>"#).unwrap();

        let mut reader = XlefReader::new();
        reader.set_id(&xlef).unwrap();
        assert_eq!(reader.series_count(), 2);
        assert_eq!(reader.metadata().size_x, 1);
        assert_eq!(reader.metadata().size_y, 1);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![77]);

        reader.set_series(1).unwrap();
        assert_eq!(reader.metadata().size_c, 3);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![10, 20, 30]);

        let _ = std::fs::remove_file(xlef);
        let _ = std::fs::remove_file(xlif);
        let _ = std::fs::remove_file(png);
        let _ = std::fs::remove_file(bmp);
    }

    #[test]
    fn xlef_opens_strict_lms_leaf_with_pixel_delegate() {
        let xlef = temp_flim2_path("lms_only.xlef");
        let lms = xlef.with_extension("lms");
        let pixels = vec![1, 0, 2, 0, 3, 0, 4, 0, 5, 0, 6, 0];
        std::fs::write(&lms, build_xlef_strict_lms(3, 2, 2, &pixels)).unwrap();
        std::fs::write(
            &xlef,
            format!(
                r#"<XLEF><Image File="{}"/></XLEF>"#,
                lms.file_name().unwrap().to_string_lossy()
            ),
        )
        .unwrap();

        let mut reader = XlefReader::new();
        reader.set_id(&xlef).unwrap();
        assert_eq!(reader.series_count(), 1);
        let meta = reader.metadata();
        assert_eq!(meta.size_x, 3);
        assert_eq!(meta.size_y, 2);
        assert_eq!(meta.pixel_type, PixelType::Uint16);
        assert_eq!(reader.open_bytes(0).unwrap(), pixels);
        assert_eq!(
            reader.open_bytes_region(0, 1, 0, 2, 2).unwrap(),
            vec![2, 0, 3, 0, 5, 0, 6, 0]
        );

        let _ = std::fs::remove_file(xlef);
        let _ = std::fs::remove_file(lms);
    }

    #[test]
    fn xlef_exposes_xml_lms_leaves_as_metadata_only_series() {
        let xlef = temp_flim2_path("lms_xml_only.xlef");
        let lms = xlef.with_extension("lms");
        std::fs::write(
            &lms,
            r#"<XLIF><Element Name="LMS dataset"><Data><Image Name="scan">
<ImageDescription>
<Channels><ChannelDescription Resolution="16"/></Channels>
<Dimensions>
<DimensionDescription DimID="1" NumberOfElements="5"/>
<DimensionDescription DimID="2" NumberOfElements="4"/>
<DimensionDescription DimID="3" NumberOfElements="2"/>
<DimensionDescription DimID="5" NumberOfElements="3"/>
</Dimensions>
</ImageDescription>
</Image></Data></Element></XLIF>"#,
        )
        .unwrap();
        std::fs::write(
            &xlef,
            format!(
                r#"<XLEF><Image File="{}"/></XLEF>"#,
                lms.file_name().unwrap().to_string_lossy()
            ),
        )
        .unwrap();

        let mut reader = XlefReader::new();
        reader.set_id(&xlef).unwrap();
        assert_eq!(reader.series_count(), 1);
        let meta = reader.metadata();
        assert_eq!(meta.size_x, 5);
        assert_eq!(meta.size_y, 4);
        assert_eq!(meta.size_z, 2);
        assert_eq!(meta.size_c, 3);
        assert_eq!(meta.pixel_type, PixelType::Uint16);
        assert!(matches!(
            meta.series_metadata.get("xlef.lms.element.name"),
            Some(crate::common::metadata::MetadataValue::String(name)) if name == "LMS dataset"
        ));
        let err = reader.open_bytes(0).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::UnsupportedFormat(message) if message.contains("LMS metadata series has no pixel delegate"))
        );

        let _ = std::fs::remove_file(xlef);
        let _ = std::fs::remove_file(lms);
    }

    #[test]
    fn xlef_opens_supported_leaves_and_lms_metadata_series() {
        let xlef = temp_flim2_path("mixed.xlef");
        let tiff = xlef.with_file_name("supported.tif");
        let lms = xlef.with_extension("lms");
        write_one_pixel_tiff(&tiff, 44);
        std::fs::write(
            &lms,
            r#"<XLIF><Element Name="metadata only"><Data><Image>
<ImageDescription><Dimensions>
<DimensionDescription DimID="1" NumberOfElements="2"/>
<DimensionDescription DimID="2" NumberOfElements="3"/>
</Dimensions></ImageDescription>
</Image></Data></Element></XLIF>"#,
        )
        .unwrap();
        std::fs::write(
            &xlef,
            format!(
                r#"<XLEF><Image File="supported.tif"/><Image File="{}"/></XLEF>"#,
                lms.file_name().unwrap().to_string_lossy()
            ),
        )
        .unwrap();

        let mut reader = XlefReader::new();
        reader.set_id(&xlef).unwrap();
        assert_eq!(reader.series_count(), 2);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![44]);
        reader.set_series(1).unwrap();
        assert_eq!(reader.metadata().size_x, 2);
        assert_eq!(reader.metadata().size_y, 3);
        assert!(reader.open_bytes(0).is_err());

        let _ = std::fs::remove_file(xlef);
        let _ = std::fs::remove_file(tiff);
        let _ = std::fs::remove_file(lms);
    }

    #[test]
    fn xlef_lms_leaf_requires_bounded_xy_metadata() {
        let xlef = temp_flim2_path("bad_lms.xlef");
        let lms = xlef.with_extension("lms");
        std::fs::write(&lms, r#"<XLIF><Element Name="bad"/></XLIF>"#).unwrap();
        std::fs::write(
            &xlef,
            format!(
                r#"<XLEF><Image File="{}"/></XLEF>"#,
                lms.file_name().unwrap().to_string_lossy()
            ),
        )
        .unwrap();

        let err = XlefReader::new().set_id(&xlef).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::UnsupportedFormat(message) if message.contains("does not declare bounded X/Y dimensions"))
        );

        let _ = std::fs::remove_file(xlef);
        let _ = std::fs::remove_file(lms);
    }

    #[test]
    fn slidebook_tiff_enriches_numeric_private_tags() {
        let path = temp_flim2_path("slidebook.tif");
        write_slidebook_tiff(&path);

        let mut reader = SlidebookTiffReader::new();
        reader.set_id(&path).expect("SlideBook TIFF should open");
        let md = &reader.metadata().series_metadata;

        assert!(matches!(
            md.get("slidebook.channel.0.name"),
            Some(crate::common::metadata::MetadataValue::String(name)) if name == "DAPI"
        ));
        assert!(matches!(
            md.get("slidebook.physical_size_x"),
            Some(crate::common::metadata::MetadataValue::Float(v)) if (*v - 0.25).abs() < 1e-12
        ));
        assert!(matches!(
            md.get("slidebook.magnification"),
            Some(crate::common::metadata::MetadataValue::Float(v)) if (*v - 60.0).abs() < 1e-12
        ));
        assert!(matches!(
            md.get("slidebook.position_z"),
            Some(crate::common::metadata::MetadataValue::Float(v)) if (*v - 3.5).abs() < 1e-12
        ));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn flowsight_bitmask_decodes_run_pairs_across_strips() {
        let strip_a = [0x00, 1, 0xff, 2];
        let strip_b = [0x7f, 0];

        let out = decode_flowsight_bitmask_strips(&[&strip_a, &strip_b], 3, 2)
            .expect("FlowSight bitmask decode");

        assert_eq!(out, vec![0x00, 0x00, 0xff, 0xff, 0xff, 0x7f]);
    }

    #[test]
    fn flowsight_bitmask_rejects_shortfall_and_overrun() {
        let short = [0x00, 1];
        let err = decode_flowsight_bitmask_strips(&[&short], 3, 1)
            .expect_err("short bitmask data must fail");
        assert!(matches!(
            err,
            BioFormatsError::InvalidData(message)
                if message.contains("ended before filling")
        ));

        let long = [0x00, 3];
        let err = decode_flowsight_bitmask_strips(&[&long], 3, 1)
            .expect_err("overlong bitmask data must fail");
        assert!(matches!(
            err,
            BioFormatsError::InvalidData(message) if message.contains("exceeds image size")
        ));
    }

    #[test]
    fn flowsight_greyscale_decodes_predictive_nibble_diffs() {
        // Low nibble is read first. These nibbles encode diffs:
        // 10, 3, 2, 5. The Java predictor reconstructs:
        // row 0: 10, 13
        // row 1: 12, 20
        let encoded = [0x1a, 0x23, 0x0d];

        let out = decode_flowsight_greyscale_strips(&[&encoded], 2, 2, true)
            .expect("FlowSight greyscale decode");

        assert_eq!(
            out,
            [10i16, 13, 12, 20]
                .into_iter()
                .flat_map(i16::to_le_bytes)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn flowsight_greyscale_decodes_negative_diffs_and_big_endian_output() {
        // Diffs: 5, -2. Reconstructed pixels: 5, 3.
        let encoded = [0x0d, 0x06];

        let out = decode_flowsight_greyscale_strips(&[&encoded], 2, 1, false)
            .expect("FlowSight greyscale decode");

        assert_eq!(
            out,
            [5i16, 3]
                .into_iter()
                .flat_map(i16::to_be_bytes)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn flowsight_greyscale_rejects_truncated_diffs() {
        let err = decode_flowsight_greyscale_strips(&[&[0x8a]], 1, 1, true)
            .expect_err("unterminated diff must fail");

        assert!(matches!(
            err,
            BioFormatsError::InvalidData(message) if message.contains("ended before filling")
        ));
    }

    #[test]
    fn flowsight_reader_decodes_greyscale_channels_from_synthetic_cif() {
        let path = temp_cif_path("greyscale");
        write_synthetic_flowsight_cif(
            &path,
            16,
            FLOWSIGHT_GREYSCALE_COMPRESSION,
            &[0x1a, 0x91, 0x11],
        );

        let mut reader = FlowSightReader::new();
        reader.set_id(&path).expect("synthetic FlowSight CIF");

        assert_eq!(reader.series_count(), 1);
        assert_eq!(reader.metadata().size_x, 2);
        assert_eq!(reader.metadata().size_y, 1);
        assert_eq!(reader.metadata().size_c, 2);
        assert_eq!(reader.metadata().image_count, 2);
        assert_eq!(reader.metadata().pixel_type, PixelType::Uint16);
        assert_eq!(
            reader.open_bytes(0).unwrap(),
            [10i16, 11]
                .into_iter()
                .flat_map(i16::to_le_bytes)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            reader.open_bytes(1).unwrap(),
            [20i16, 21]
                .into_iter()
                .flat_map(i16::to_le_bytes)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            reader.open_bytes_region(1, 1, 0, 1, 1).unwrap(),
            21i16.to_le_bytes()
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn flowsight_reader_decodes_bitmask_channels_from_synthetic_cif() {
        let path = temp_cif_path("bitmask");
        write_synthetic_flowsight_cif(&path, 8, FLOWSIGHT_BITMASK_COMPRESSION, &[0x00, 1, 0xff, 1]);

        let mut reader = FlowSightReader::new();
        reader.set_id(&path).expect("synthetic FlowSight CIF");

        assert_eq!(reader.metadata().pixel_type, PixelType::Uint8);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![0x00, 0x00]);
        assert_eq!(reader.open_bytes(1).unwrap(), vec![0xff, 0xff]);
        assert_eq!(reader.open_bytes_region(1, 1, 0, 1, 1).unwrap(), vec![0xff]);

        let _ = std::fs::remove_file(path);
    }

    // ---- VSI tag-tree parser tests -------------------------------------

    /// One leaf field for the synthetic VSI tag stream.
    struct VsiField {
        field_type: i32,
        tag: i32,
        /// inline data bytes that follow the 16-byte record (may be empty).
        data: Vec<u8>,
    }

    /// Build a VSI tag container starting at byte offset 8 from a list of leaf
    /// fields, wiring `nextField` to chain them and a terminating `nextField=0`.
    fn build_vsi_tag_stream(fields: &[VsiField]) -> Vec<u8> {
        // 0..8: filler (parser starts at offset 8).
        let mut out = vec![0u8; 8];
        // Container header (24 bytes): headerSize, version, volumeVersion,
        // dataFieldOffset(i64), flags, skip4.
        out.extend_from_slice(&24i16.to_le_bytes());
        out.extend_from_slice(&21321i16.to_le_bytes());
        out.extend_from_slice(&0i32.to_le_bytes()); // volumeVersion
        out.extend_from_slice(&24i64.to_le_bytes()); // dataFieldOffset -> right after header
        out.extend_from_slice(&(fields.len() as i32).to_le_bytes()); // flags = tagCount
        out.extend_from_slice(&0i32.to_le_bytes()); // skip 4

        // The corrected parser navigates container-relative: it keeps the field
        // cursor pinned at the container header (container_fp = 8) and re-seeks to
        // `container_fp + nextField` for each sibling. So `nextField` must encode
        // the container-relative offset of the *next* field, not the byte advance
        // from the current one. Field 0 starts at dataFieldOffset (24); each record
        // is 16 bytes + inline data.
        let mut field_starts = Vec::with_capacity(fields.len());
        let mut rel = 24i64; // dataFieldOffset: first field directly after header
        for f in fields {
            field_starts.push(rel);
            rel += 16 + f.data.len() as i64;
        }

        for (i, f) in fields.iter().enumerate() {
            let next_field = if i + 1 < fields.len() {
                field_starts[i + 1]
            } else {
                0
            };
            out.extend_from_slice(&f.field_type.to_le_bytes());
            out.extend_from_slice(&f.tag.to_le_bytes());
            out.extend_from_slice(&(next_field as u32).to_le_bytes());
            out.extend_from_slice(&(f.data.len() as i32).to_le_bytes());
            out.extend_from_slice(&f.data);
        }
        out
    }

    fn int_rect(vals: [i32; 4]) -> Vec<u8> {
        let mut v = Vec::new();
        for x in vals {
            v.extend_from_slice(&x.to_le_bytes());
        }
        v
    }

    #[test]
    fn vsi_tags_parse_image_boundary_and_tile_origin() {
        // IMAGE_FRAME_VOLUME then EXTERNAL_FILE_PROPERTIES bumps metadata_index to
        // 0, then IMAGE_BOUNDARY + TILE_ORIGIN fill pyramid[0].
        let fields = vec![
            VsiField {
                field_type: VSI_INT,
                tag: VSI_IMAGE_FRAME_VOLUME,
                data: 0i32.to_le_bytes().to_vec(),
            },
            VsiField {
                field_type: VSI_INT,
                tag: VSI_EXTERNAL_FILE_PROPERTIES,
                data: 0i32.to_le_bytes().to_vec(),
            },
            VsiField {
                field_type: 259, // INT_RECT
                tag: VSI_IMAGE_BOUNDARY,
                data: int_rect([0, 0, 1234, 567]),
            },
            VsiField {
                field_type: 256, // INT_2
                tag: VSI_TILE_ORIGIN,
                data: {
                    let mut v = Vec::new();
                    v.extend_from_slice(&16i32.to_le_bytes());
                    v.extend_from_slice(&32i32.to_le_bytes());
                    v
                },
            },
        ];
        let stream = build_vsi_tag_stream(&fields);
        let mut parser = VsiTagParser::new(&stream);
        parser.read_tags(8, false, "");

        assert_eq!(parser.pyramids.len(), 1, "one pyramid expected");
        let p = &parser.pyramids[0];
        assert_eq!(p.width, Some(1234), "IMAGE_BOUNDARY width = intValues[2]");
        assert_eq!(p.height, Some(567), "IMAGE_BOUNDARY height = intValues[3]");
        assert_eq!(p.tile_origin_x, Some(16));
        assert_eq!(p.tile_origin_y, Some(32));
    }

    #[test]
    fn vsi_has_external_file_sets_expect_ets() {
        let fields = vec![VsiField {
            field_type: VSI_INT,
            tag: VSI_HAS_EXTERNAL_FILE,
            data: 1i32.to_le_bytes().to_vec(),
        }];
        let stream = build_vsi_tag_stream(&fields);
        let mut parser = VsiTagParser::new(&stream);
        parser.read_tags(8, false, "");
        assert!(parser.expect_ets, "HAS_EXTERNAL_FILE=1 must set expect_ets");
    }

    /// Exact pyramid width/height from the tag-tree overrides the tile-grid
    /// extent for level 0 (CellSensReader.java:1463-1464).
    #[test]
    fn ets_level0_uses_pyramid_width_height() {
        let mut vol = EtsVolume {
            n_dimensions: 2,
            size_c: 1,
            compression: ETS_RAW,
            tile_x: 512,
            tile_y: 512,
            pixel_type_code: ETS_PT_UCHAR,
            use_pyramid: false,
            // A 3x3 tile grid would give 1536x1536, but the pyramid declares
            // the true size as 1234x567.
            tiles: vec![(vec![0, 0], 0, 0), (vec![2, 2], 0, 0)],
            pyramid_width: Some(1234),
            pyramid_height: Some(567),
            ..Default::default()
        };
        vol.compute_levels();
        let l0 = &vol.levels[0];
        assert_eq!(l0.size_x, 1234);
        assert_eq!(l0.size_y, 567);
        // The tile grid still spans 3x3 for stitching.
        assert_eq!(l0.cols, 3);
        assert_eq!(l0.rows, 3);
    }

    /// Tile-origin cropping shifts the tile grid and crops to the declared size
    /// (CellSensReader.java:552-583).
    #[test]
    fn ets_assemble_plane_applies_tile_origin_crop() {
        // Single 4x4 RAW grayscale tile, origin (1,1), declared image 3x3.
        // The output should be the tile's sub-rectangle [1..4, 1..4].
        let mut tile = vec![0u8; 16];
        for (i, b) in tile.iter_mut().enumerate() {
            *b = i as u8; // row-major values 0..15
        }
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("bioformats_ets_crop_{nanos}.bin"));
        let mut f = File::create(&path).unwrap();
        f.write_all(&tile).unwrap();
        drop(f);

        let mut vol = EtsVolume {
            path: path.clone(),
            n_dimensions: 2,
            size_c: 1,
            compression: ETS_RAW,
            tile_x: 4,
            tile_y: 4,
            pixel_type_code: ETS_PT_UCHAR,
            use_pyramid: false,
            tiles: vec![(vec![0, 0], 0, 16)],
            pyramid_width: Some(3),
            pyramid_height: Some(3),
            tile_origin_x: Some(1),
            tile_origin_y: Some(1),
            ..Default::default()
        };
        vol.compute_levels();
        let plane = vol.assemble_plane(0, 0, 0, 0).unwrap();
        // Mirrors Java openBytes compaction (CellSensReader.java:537-592): the
        // tile is shifted by tileOrigin (1,1) so it covers image rect (1,1,2,2);
        // the 2x2 intersecting region (tile rows 0..2 cols 0..2 -> values 0,1 /
        // 4,5) is copied compacting from output column 0 of each row band.
        // Output buffer is 3x3; untouched cells stay 0.
        // row band starts at output_row 0: out[0..2]=[0,1], out[3..5]=[4,5].
        assert_eq!(plane, vec![0, 1, 0, 4, 5, 0, 0, 0, 0]);

        let _ = std::fs::remove_file(path);
    }

    /// Dimension ordering tag+2 maps Z/C/T to coordinate slots, and the
    /// resolution slot (last, usePyramid) is excluded for Z/T
    /// (CellSensReader.java:1377-1388).
    #[test]
    fn ets_dim_order_tag_plus_two_with_resolution_slot_exclusion() {
        // 5-dim coordinate: [col, row, c, t, resolution]. dim_order tags: C=0,T=1.
        // Slots: C=2, T=3. usePyramid -> last slot (4) is resolution.
        let mut vol = EtsVolume {
            n_dimensions: 5,
            size_c: 1,
            tile_x: 256,
            tile_y: 256,
            pixel_type_code: ETS_PT_UCHAR,
            use_pyramid: true,
            tiles: vec![
                (vec![0, 0, 0, 0, 0], 0, 0),
                (vec![0, 0, 1, 0, 0], 0, 0), // c=1
                (vec![0, 0, 0, 2, 0], 0, 0), // t=2
            ],
            dim_order: VsiDimOrder {
                c: Some(0),
                t: Some(1),
                z: None,
                l: None,
            },
            ..Default::default()
        };
        vol.compute_levels();
        assert_eq!(vol.dim_c, Some(2));
        assert_eq!(vol.dim_t, Some(3));
        assert_eq!(vol.dim_z, None);
        let l0 = &vol.levels[0];
        assert_eq!(l0.size_c, 2, "maxC=1 -> sizeC*(1+1)");
        assert_eq!(l0.size_t, 3, "maxT=2 -> sizeT=3");
    }

    /// Z tag colliding with the resolution slot is dropped
    /// (CellSensReader.java:1385-1388).
    #[test]
    fn ets_dim_order_z_collides_with_resolution_slot() {
        // 4-dim: [col,row,z,resolution]. Z tag = 1 -> slot 3 == last (resolution).
        // usePyramid -> Z must be cleared.
        let mut vol = EtsVolume {
            n_dimensions: 4,
            size_c: 1,
            tile_x: 256,
            tile_y: 256,
            pixel_type_code: ETS_PT_UCHAR,
            use_pyramid: true,
            tiles: vec![(vec![0, 0, 0, 0], 0, 0)],
            dim_order: VsiDimOrder {
                z: Some(1),
                t: None,
                c: None,
                l: None,
            },
            ..Default::default()
        };
        vol.compute_levels();
        assert_eq!(vol.dim_z, None, "Z slot == resolution slot must be dropped");
    }

    /// No T/Z ordering + long coordinate triggers the C/T/Z inference fallback
    /// (CellSensReader.java:1409-1444). 6-dim, no dim_order -> C=2,T=3,Z=4.
    #[test]
    fn ets_dim_order_inference_fallback_for_long_coords() {
        let mut vol = EtsVolume {
            n_dimensions: 6,
            size_c: 1,
            tile_x: 256,
            tile_y: 256,
            pixel_type_code: ETS_PT_UCHAR,
            use_pyramid: true,
            tiles: vec![
                (vec![0, 0, 1, 0, 0, 0], 0, 0), // c=1
                (vec![0, 0, 0, 3, 0, 0], 0, 0), // t=3
                (vec![0, 0, 0, 0, 2, 0], 0, 0), // z=2
            ],
            dim_order: VsiDimOrder::default(),
            ..Default::default()
        };
        vol.compute_levels();
        assert_eq!(vol.dim_c, Some(2));
        assert_eq!(vol.dim_t, Some(3));
        assert_eq!(vol.dim_z, Some(4));
        let l0 = &vol.levels[0];
        assert_eq!(l0.size_c, 2);
        assert_eq!(l0.size_t, 4);
        assert_eq!(l0.size_z, 3);
    }

    /// `max_pixel_extent` returns the resolution-0 tile-grid extent in pixels,
    /// the primitive used for orphan-ETS matching (CellSensReader.java:1330-1339).
    #[test]
    fn ets_max_pixel_extent_at_resolution_zero() {
        let vol = EtsVolume {
            n_dimensions: 3,
            tile_x: 100,
            tile_y: 200,
            pixel_type_code: ETS_PT_UCHAR,
            use_pyramid: true,
            // res slot is index 2. res0 tiles span cols 0..2, rows 0..1.
            // A res>0 tile at a larger col must NOT widen the extent.
            tiles: vec![
                (vec![0, 0, 0], 0, 0),
                (vec![2, 1, 0], 0, 0),
                (vec![9, 9, 1], 0, 0), // resolution 1, ignored
            ],
            ..Default::default()
        };
        // maxX=2 -> (2+1)*100 = 300; maxY=1 -> (1+1)*200 = 400.
        assert_eq!(vol.max_pixel_extent(), (300, 400));
    }

    fn build_synthetic_ets(
        n_dimensions: u32,
        pixel_type_code: i32,
        size_c: u32,
        tile_x: u32,
        tile_y: u32,
        n_bytes: u32,
        payload_len: usize,
    ) -> Vec<u8> {
        let additional_header_offset = 64usize;
        let used_chunk_offset = 256usize;
        let entry_len = 4 + n_dimensions as usize * 4 + 8 + 4 + 4;
        let tile_offset = used_chunk_offset + entry_len;
        let mut bytes = vec![0u8; tile_offset + payload_len];

        bytes[0..4].copy_from_slice(b"SIS ");
        bytes[12..16].copy_from_slice(&n_dimensions.to_le_bytes());
        bytes[16..24].copy_from_slice(&(additional_header_offset as u64).to_le_bytes());
        bytes[32..40].copy_from_slice(&(used_chunk_offset as u64).to_le_bytes());
        bytes[40..44].copy_from_slice(&1u32.to_le_bytes());

        bytes[additional_header_offset..additional_header_offset + 4].copy_from_slice(b"ETS ");
        let base = additional_header_offset + 8;
        bytes[base..base + 4].copy_from_slice(&pixel_type_code.to_le_bytes());
        bytes[base + 4..base + 8].copy_from_slice(&size_c.to_le_bytes());
        bytes[base + 12..base + 16].copy_from_slice(&ETS_RAW.to_le_bytes());
        bytes[base + 20..base + 24].copy_from_slice(&tile_x.to_le_bytes());
        bytes[base + 24..base + 28].copy_from_slice(&tile_y.to_le_bytes());

        let mut off = used_chunk_offset + 4;
        for coord in [0i32, 0].into_iter().take(n_dimensions as usize) {
            bytes[off..off + 4].copy_from_slice(&coord.to_le_bytes());
            off += 4;
        }
        bytes[off..off + 8].copy_from_slice(&(tile_offset as u64).to_le_bytes());
        off += 8;
        bytes[off..off + 4].copy_from_slice(&n_bytes.to_le_bytes());
        for (i, b) in bytes[tile_offset..].iter_mut().enumerate() {
            *b = i as u8;
        }
        bytes
    }

    #[test]
    fn ets_parse_rejects_unsupported_pixel_type_instead_of_fallback() {
        let path = temp_flim2_path("bad-pixel.ets");
        std::fs::write(&path, build_synthetic_ets(2, 99, 1, 1, 1, 1, 1)).unwrap();

        let err = CellSensReader::parse_ets(&path).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("pixel type code 99")),
            "{err:?}"
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn ets_parse_rejects_malformed_tile_counts_before_metadata() {
        let short_payload = temp_flim2_path("short-raw-tile.ets");
        std::fs::write(
            &short_payload,
            build_synthetic_ets(2, ETS_PT_USHORT, 1, 2, 2, 2, 2),
        )
        .unwrap();
        let err = CellSensReader::parse_ets(&short_payload).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::InvalidData(ref message) if message.contains("RAW tile byte count")),
            "{err:?}"
        );
        let _ = std::fs::remove_file(short_payload);

        let truncated_table = temp_flim2_path("truncated-table.ets");
        let mut bytes = build_synthetic_ets(2, ETS_PT_UCHAR, 1, 1, 1, 1, 1);
        bytes.truncate(260);
        std::fs::write(&truncated_table, bytes).unwrap();
        let err = CellSensReader::parse_ets(&truncated_table).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::Format(ref message) if message.contains("truncated header/table")),
            "{err:?}"
        );
        let _ = std::fs::remove_file(truncated_table);
    }

    #[test]
    fn ets_parse_rejects_zero_dimensions_and_missing_payload() {
        let zero_tile = temp_flim2_path("zero-tile.ets");
        std::fs::write(
            &zero_tile,
            build_synthetic_ets(2, ETS_PT_UCHAR, 1, 0, 1, 1, 1),
        )
        .unwrap();
        let err = CellSensReader::parse_ets(&zero_tile).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::Format(ref message) if message.contains("non-zero")),
            "{err:?}"
        );
        let _ = std::fs::remove_file(zero_tile);

        let missing_payload = temp_flim2_path("missing-payload.ets");
        std::fs::write(
            &missing_payload,
            build_synthetic_ets(2, ETS_PT_UCHAR, 1, 2, 2, 4, 2),
        )
        .unwrap();
        let err = CellSensReader::parse_ets(&missing_payload).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::InvalidData(ref message) if message.contains("past end of file")),
            "{err:?}"
        );
        let _ = std::fs::remove_file(missing_payload);
    }

    #[test]
    fn cellsens_failed_set_id_clears_existing_state() {
        let mut reader = CellSensReader::new();
        reader.tiff_series = 1;
        reader.ets.push(EtsVolume {
            n_dimensions: 2,
            size_c: 1,
            tile_x: 1,
            tile_y: 1,
            pixel_type_code: ETS_PT_UCHAR,
            tiles: vec![(vec![0, 0], 0, 1)],
            ..Default::default()
        });

        let missing = temp_flim2_path("missing.vsi");
        let err = reader.set_id(&missing).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("could not parse as TIFF")),
            "{err:?}"
        );
        assert_eq!(reader.series_count(), 0);
        assert!(matches!(
            reader.set_series(0),
            Err(BioFormatsError::SeriesOutOfRange(0))
        ));
    }

    /// Non-geometry acquisition metadata tags are captured into the pyramid meta
    /// (CellSensReader.java:1881-1979).
    #[test]
    fn vsi_captures_non_geometry_metadata() {
        // metadataIndex is incremented by EXTERNAL_FILE_PROPERTIES preceded by
        // IMAGE_FRAME_VOLUME, so seed those first, then the metadata leaves.
        let fields = vec![
            VsiField {
                field_type: VSI_INT,
                tag: VSI_IMAGE_FRAME_VOLUME,
                data: 0i32.to_le_bytes().to_vec(),
            },
            VsiField {
                field_type: VSI_INT,
                tag: VSI_EXTERNAL_FILE_PROPERTIES,
                data: 0i32.to_le_bytes().to_vec(),
            },
            VsiField {
                field_type: VSI_TCHAR,
                tag: VSI_DEVICE_NAME,
                data: b"CameraX\0".to_vec(),
            },
            VsiField {
                field_type: VSI_DOUBLE,
                tag: VSI_OBJECTIVE_MAG,
                data: 40.0f64.to_le_bytes().to_vec(),
            },
            VsiField {
                field_type: VSI_DOUBLE,
                tag: VSI_NUMERICAL_APERTURE,
                data: 0.95f64.to_le_bytes().to_vec(),
            },
            VsiField {
                field_type: VSI_INT,
                tag: VSI_BIT_DEPTH,
                data: 12i32.to_le_bytes().to_vec(),
            },
            VsiField {
                field_type: VSI_LONG,
                tag: VSI_EXPOSURE_TIME,
                data: 25000i64.to_le_bytes().to_vec(),
            },
        ];
        let stream = build_vsi_tag_stream(&fields);
        let mut parser = VsiTagParser::new(&stream);
        parser.read_tags(8, false, "");

        assert_eq!(parser.pyramids.len(), 1);
        let m = &parser.pyramids[0].meta;
        assert_eq!(m.device_names, vec!["CameraX".to_string()]);
        assert_eq!(m.magnification, Some(40.0));
        assert_eq!(m.numerical_aperture, Some(0.95));
        assert_eq!(m.bit_depth, Some(12));
        assert_eq!(m.exposure_times, vec![25000]);
    }

    #[test]
    fn ets_level_metadata_includes_cellsens_acquisition_metadata() {
        let mut vol = EtsVolume {
            n_dimensions: 2,
            size_c: 1,
            compression: ETS_RAW,
            tile_x: 1,
            tile_y: 1,
            pixel_type_code: ETS_PT_UCHAR,
            tiles: vec![(vec![0, 0], 0, 1)],
            meta: VsiPyramidMeta {
                device_names: vec!["CameraX".to_string()],
                device_ids: vec!["cam-1".to_string()],
                objective_names: vec!["UPlanSApo".to_string()],
                magnification: Some(40.0),
                numerical_aperture: Some(0.95),
                gain: Some(1.5),
                bit_depth: Some(12),
                exposure_times: vec![25000],
                channel_wavelengths: vec![488.0],
                channel_names: vec!["DAPI".to_string()],
                name: Some("Stack A".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        vol.compute_levels();

        let meta = vol.level_metadata(0).unwrap();
        let md = &meta.series_metadata;
        assert!(matches!(
            md.get("cellsens.ets.device_name"),
            Some(crate::common::metadata::MetadataValue::String(v)) if v == "CameraX"
        ));
        assert!(matches!(
            md.get("cellsens.ets.device_id"),
            Some(crate::common::metadata::MetadataValue::String(v)) if v == "cam-1"
        ));
        assert!(matches!(
            md.get("cellsens.ets.objective_name"),
            Some(crate::common::metadata::MetadataValue::String(v)) if v == "UPlanSApo"
        ));
        assert!(matches!(
            md.get("cellsens.ets.objective_magnification"),
            Some(crate::common::metadata::MetadataValue::Float(v)) if (*v - 40.0).abs() < 1e-12
        ));
        assert!(matches!(
            md.get("cellsens.ets.camera_gain"),
            Some(crate::common::metadata::MetadataValue::Float(v)) if (*v - 1.5).abs() < 1e-12
        ));
        assert!(matches!(
            md.get("cellsens.ets.bit_depth"),
            Some(crate::common::metadata::MetadataValue::Int(12))
        ));
        assert!(matches!(
            md.get("cellsens.ets.exposure_time"),
            Some(crate::common::metadata::MetadataValue::Int(25000))
        ));
        assert!(matches!(
            md.get("cellsens.ets.channel_wavelength.0"),
            Some(crate::common::metadata::MetadataValue::Float(v)) if (*v - 488.0).abs() < 1e-12
        ));
        assert!(matches!(
            md.get("cellsens.ets.channel_name.0"),
            Some(crate::common::metadata::MetadataValue::String(v)) if v == "DAPI"
        ));
        assert!(matches!(
            md.get("cellsens.ets.stack_name"),
            Some(crate::common::metadata::MetadataValue::String(v)) if v == "Stack A"
        ));
    }

    #[test]
    fn cellsens_ome_metadata_preserves_ets_original_metadata_annotation() {
        let mut vol = EtsVolume {
            n_dimensions: 2,
            size_c: 1,
            compression: ETS_RAW,
            tile_x: 1,
            tile_y: 1,
            pixel_type_code: ETS_PT_UCHAR,
            tiles: vec![(vec![0, 0], 0, 1)],
            meta: VsiPyramidMeta {
                device_names: vec!["CameraX".to_string()],
                objective_names: vec!["UPlanSApo".to_string()],
                channel_wavelengths: vec![488.0],
                name: Some("Stack A".to_string()),
                ..Default::default()
            },
            physical_size_x: Some(0.25),
            physical_size_y: Some(0.5),
            ..Default::default()
        };
        vol.compute_levels();

        let mut reader = CellSensReader::new();
        reader.ets.push(vol);
        reader.series_map.push(CellSensTarget::Ets {
            volume: 0,
            resolution: 0,
        });
        reader.series_names.push("Stack A".to_string());
        reader.series_phys.push(Some((0.25, 0.5)));

        let ome = reader.ome_metadata().expect("CellSens OME metadata");
        assert_eq!(ome.images.len(), 1);
        assert_eq!(ome.images[0].name.as_deref(), Some("Stack A"));
        assert_eq!(ome.images[0].physical_size_x, Some(0.25));
        assert_eq!(ome.images[0].physical_size_y, Some(0.5));

        let values = ome
            .annotations
            .iter()
            .find_map(|ann| match ann {
                crate::common::ome_metadata::OmeAnnotation::MapAnnotation {
                    id,
                    namespace,
                    values,
                } if id.as_deref() == Some("Annotation:OriginalMetadata:0")
                    && namespace.as_deref()
                        == Some("openmicroscopy.org/bioformats/original-metadata") =>
                {
                    Some(values)
                }
                _ => None,
            })
            .expect("CellSens original metadata annotation");

        assert!(values
            .iter()
            .any(|(key, value)| key == "cellsens.ets.device_name" && value == "CameraX"));
        assert!(values
            .iter()
            .any(|(key, value)| key == "cellsens.ets.objective_name" && value == "UPlanSApo"));
        assert!(values
            .iter()
            .any(|(key, value)| key == "cellsens.ets.channel_wavelength.0" && value == "488"));
        assert!(values
            .iter()
            .any(|(key, value)| key == "cellsens.ets.stack_name" && value == "Stack A"));
    }

    /// Prefix-gated VALUE metadata: the same VALUE tag is disambiguated entirely
    /// by the recursive tag-name prefix accumulated while descending volumes
    /// (CellSensReader.java:1960-1979). Drives `capture_metadata` directly with
    /// each prefix the way `getVolumeName` would have set it during the walk.
    #[test]
    fn vsi_value_tag_routed_by_prefix() {
        let mut parser = VsiTagParser::new(&[]);
        parser.pyramids.push(VsiPyramid::default());
        parser.metadata_index = 0;

        // tag 2417 volume -> "Channel Wavelength " (CellSensReader.java:2097-2098).
        parser.capture_metadata(VSI_VALUE, "488", "Channel Wavelength ");
        parser.capture_metadata(VSI_VALUE, "561", "Channel Wavelength ");
        // empty-name NEW_MDIM Z volumes fall back to these literal prefixes
        // (CellSensReader.java:1707-1719, 1967-1974).
        parser.capture_metadata(VSI_VALUE, "1.5", "Z start position");
        parser.capture_metadata(VSI_VALUE, "0.25", "Z increment");
        parser.capture_metadata(VSI_VALUE, "0.0", "Z value");
        parser.capture_metadata(VSI_VALUE, "0.25", "Z value");
        // TIME_VALUE volume -> "Timestamp " (CellSensReader.java:2101-2102).
        parser.capture_metadata(VSI_VALUE, "100.0", "Timestamp ");
        // WORKING_DISTANCE volume -> "Objective Working Distance "
        // (CellSensReader.java:2099-2100, 1964-1965).
        parser.capture_metadata(VSI_VALUE, "0.21", "Objective Working Distance ");

        let m = &parser.pyramids[0].meta;
        assert_eq!(m.channel_wavelengths, vec![488.0, 561.0]);
        assert_eq!(m.z_start, Some(1.5));
        assert_eq!(m.z_increment, Some(0.25));
        assert_eq!(m.z_values, vec![0.0, 0.25]);
        assert_eq!(m.t_values, vec![100.0]);
        assert_eq!(m.working_distance, Some(0.21));
    }

    /// EXPOSURE_TIME is routed by whether the tag prefix is empty
    /// (CellSensReader.java:1899-1905): empty -> exposureTimes; non-empty ->
    /// defaultExposureTime + otherExposureTimes.
    #[test]
    fn vsi_exposure_time_split_by_prefix() {
        let mut parser = VsiTagParser::new(&[]);
        parser.pyramids.push(VsiPyramid::default());
        parser.metadata_index = 0;

        parser.capture_metadata(VSI_EXPOSURE_TIME, "1000", "");
        parser.capture_metadata(VSI_EXPOSURE_TIME, "2000", "Microscope ");
        parser.capture_metadata(VSI_EXPOSURE_TIME, "3000", "Microscope ");

        let m = &parser.pyramids[0].meta;
        assert_eq!(
            m.exposure_times,
            vec![1000],
            "empty prefix -> exposureTimes"
        );
        assert_eq!(m.default_exposure_time, Some(3000), "last prefixed wins");
        assert_eq!(m.other_exposure_times, vec![2000, 3000]);
    }
}
