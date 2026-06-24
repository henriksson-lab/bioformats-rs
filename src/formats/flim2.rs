//! Additional FLIM, flow cytometry, and miscellaneous imaging format readers.
//!
//! Includes FlowSightReader with binary header inspection plus explicit
//! unsupported detectors and bounded native readers.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufReader, ErrorKind, Read, Seek};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::common::codec::{decompress_deflate, decompress_lzw, decompress_packbits};
use crate::common::error::{BioFormatsError, Result};
use crate::common::io::read_bytes_at;
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue, ModuloAnnotation};
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

fn flowsight_pre_xml_channel_count(ifd0: &Ifd) -> usize {
    let mut channel_count = ifd0.get_u32(FLOWSIGHT_CHANNEL_COUNT_TAG).unwrap_or(1) as usize;
    if let Some(names) = ifd0.get_str(FLOWSIGHT_CHANNEL_NAMES_TAG) {
        channel_count = split_flowsight_pipe_list(names).len();
    }
    channel_count
}

fn validate_flowsight_channel_descriptions(ifd0: &Ifd, channel_count: usize) -> Result<()> {
    if let Some(descs) = ifd0.get_str(FLOWSIGHT_CHANNEL_DESCS_TAG) {
        let desc_count = split_flowsight_pipe_list(descs).len();
        if desc_count != channel_count {
            return Err(BioFormatsError::Format(format!(
                "Channel count ({channel_count}) does not match number of channel descriptions ({desc_count}) in string \"{descs}\""
            )));
        }
    }
    Ok(())
}

fn flowsight_channel_count(ifd0: &Ifd) -> usize {
    // Match Java FlowSightReader (lines 150-200): start with the CHANNEL_COUNT_TAG
    // default, override with the channel-names count if present, then override
    // AGAIN with the XML ChannelInUseIndicators count if the XML provides it.
    // The XML count is applied LAST so it wins when sources disagree.
    let mut channel_count = flowsight_pre_xml_channel_count(ifd0);
    if let Some(xml) = ifd0.get_str(FLOWSIGHT_METADATA_XML_TAG) {
        if let Some(count) = count_flowsight_channels_in_use(xml) {
            channel_count = count;
        }
    }
    channel_count
}

fn split_flowsight_pipe_list(value: &str) -> Vec<String> {
    // Java FlowSightReader uses String.split("\\|"), which preserves empty
    // interior tokens but discards trailing empty tokens.
    if value.is_empty() {
        return vec![String::new()];
    }
    let mut parts: Vec<String> = value.split('|').map(str::to_owned).collect();
    while parts.last().is_some_and(|s| s.is_empty()) {
        parts.pop();
    }
    parts
}

fn count_flowsight_channels_in_use(xml: &str) -> Option<usize> {
    let imaging_start = xml.find("<Imaging")?;
    let imaging_open_end = xml[imaging_start..].find('>')? + imaging_start + 1;
    let imaging_close = xml[imaging_open_end..].find("</Imaging>")? + imaging_open_end;
    let mut pos = imaging_open_end;
    let mut found = None;
    while pos < imaging_close {
        let Some(rel_open) = xml[pos..imaging_close].find('<') else {
            break;
        };
        let open = pos + rel_open;
        if xml[open..].starts_with("</") {
            break;
        }
        if xml[open..].starts_with("<?") || xml[open..].starts_with("<!") {
            pos = xml[open..imaging_close]
                .find('>')
                .map(|rel| open + rel + 1)
                .unwrap_or(imaging_close);
            continue;
        }
        let tag_name_start = open + 1;
        let tag_name_end = xml[tag_name_start..imaging_close]
            .find(|ch: char| ch == '>' || ch == '/' || ch.is_whitespace())
            .map(|rel| tag_name_start + rel)?;
        let tag_name = &xml[tag_name_start..tag_name_end];
        let open_end = xml[tag_name_end..imaging_close].find('>')? + tag_name_end;
        if tag_name.starts_with("ChannelInUseIndicators") {
            let close_tag = format!("</{tag_name}>");
            let close = xml[open_end + 1..imaging_close].find(&close_tag)? + open_end + 1;
            found = Some(
                xml[open_end + 1..close]
                    .split(' ')
                    .filter(|token| *token == "1")
                    .count(),
            );
            pos = close + close_tag.len();
            continue;
        }
        let close_tag = format!("</{tag_name}>");
        pos = xml[open_end + 1..imaging_close]
            .find(&close_tag)
            .map(|rel| open_end + 1 + rel + close_tag.len())
            .unwrap_or(open_end + 1);
    }
    found
}

fn build_flowsight_metadata(
    ifd: &Ifd,
    ifd0: &Ifd,
    channel_count: usize,
    little_endian: bool,
) -> Result<ImageMetadata> {
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
        is_little_endian: little_endian,
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

fn flowsight_has_metadata_tag(header: &[u8]) -> bool {
    let cursor = std::io::Cursor::new(header);
    let mut parser = match TiffParser::new(cursor) {
        Ok(parser) => parser,
        Err(_) => return false,
    };
    if !matches!(parser.variant, crate::tiff::parser::TiffVariant::Classic) {
        return false;
    }
    let (ifd, _) = match parser.read_ifd(parser.first_ifd_offset) {
        Ok(value) => value,
        Err(_) => return false,
    };
    ifd.get_str(FLOWSIGHT_METADATA_XML_TAG).is_some()
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
        flowsight_has_metadata_tag(header)
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
        validate_flowsight_channel_descriptions(ifd0, flowsight_pre_xml_channel_count(ifd0))?;
        let channel_count = flowsight_channel_count(ifd0);
        let metas = ifds[1..]
            .iter()
            .map(|ifd| build_flowsight_metadata(ifd, ifd0, channel_count, little_endian))
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

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        use crate::common::ome_metadata::OmeMetadata;

        if self.metas.is_empty() {
            return None;
        }

        let channel_names = self
            .ifds
            .first()
            .and_then(|ifd| ifd.get_str(FLOWSIGHT_CHANNEL_NAMES_TAG))
            .map(split_flowsight_pipe_list);
        let channel_descs = self
            .ifds
            .first()
            .and_then(|ifd| ifd.get_str(FLOWSIGHT_CHANNEL_DESCS_TAG))
            .map(split_flowsight_pipe_list);

        let mut ome = OmeMetadata::default();
        for (series, meta) in self.metas.iter().enumerate() {
            let _ = ome.populate_pixels(meta, series);
            let Some(image) = ome.images.get_mut(series) else {
                continue;
            };
            if let (Some(_names), Some(descs)) = (channel_names.as_ref(), channel_descs.as_ref()) {
                let is_mask = meta.pixel_type == PixelType::Uint8;
                for (channel, desc) in image.channels.iter_mut().zip(descs.iter()) {
                    channel.name = Some(if is_mask {
                        format!("{desc} Mask")
                    } else {
                        desc.clone()
                    });
                }
            }
            image.modulo_z = meta.modulo_z.clone();
            image.modulo_c = meta.modulo_c.clone();
            image.modulo_t = meta.modulo_t.clone();
            let _ = ome.add_original_metadata_annotations(meta, series);
        }
        Some(ome)
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
        .is_some_and(|bytes| u32::from_le_bytes(bytes.try_into().unwrap()) == 1985)
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

/// A `Spectrum` record parsed from a SpectralLibrary, mirroring the Java
/// `IM3Reader.Spectrum` inner class (name plus per-bin wavelengths/magnitudes).
#[derive(Clone)]
struct Im3Spectrum {
    name: Option<String>,
    wavelengths: Option<Vec<f64>>,
    magnitudes: Option<Vec<f64>>,
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

/// Read the boolean entries of a `BooleanIM3Record` (Java `REC_BOOLEAN`).
///
/// Mirrors the Java `BooleanIM3Record.getNumEntries`/`getEntry` accessors: the
/// count is stored as an int32 at `offset + 4`, and each value is a single
/// non-zero byte starting at `offset + 8`.
fn im3_bool_entries(bytes: &[u8], rec: &Im3Record) -> Option<Vec<bool>> {
    if rec.rec_type != 9 || rec.payload_len < 8 {
        return None;
    }
    let count = im3_read_u32_le(bytes, rec.payload_offset + 4, "boolean count").ok()? as usize;
    if 8usize.checked_add(count)? > rec.payload_len || count > 4096 {
        return None;
    }
    let mut values = Vec::with_capacity(count);
    for index in 0..count {
        let byte = *bytes.get(rec.payload_offset + 8 + index)?;
        values.push(byte != 0);
    }
    Some(values)
}

/// Parse a `Spectrum` container record from a SpectralLibrary, mirroring the
/// Java `IM3Reader.Spectrum(IRandomAccess, ContainerRecord)` constructor.
///
/// The container groups a `Name` string record and a `Spectrum` container
/// record; the latter is walked by `im3_parse_spectrum_record` for the
/// per-bin wavelengths and magnitudes.
fn im3_parse_spectrum(bytes: &[u8], rec: &Im3Record) -> Result<Im3Spectrum> {
    let mut spectrum = Im3Spectrum {
        name: None,
        wavelengths: None,
        magnitudes: None,
    };
    for sub_rec in im3_container_children(bytes, rec)? {
        if sub_rec.name == "Name" && sub_rec.rec_type == 10 {
            spectrum.name = im3_string_entry(bytes, &sub_rec);
        } else if sub_rec.name == "Spectrum" && sub_rec.rec_type == 0 {
            im3_parse_spectrum_record(bytes, &sub_rec, &mut spectrum)?;
        }
    }
    Ok(spectrum)
}

/// Walk the nested `Spectrum` container, mirroring the Java
/// `IM3Reader.Spectrum.parseSpectrumRecord` method: one level of unnamed
/// container nesting holds the `Wavelengths` and `Magnitudes` float records.
fn im3_parse_spectrum_record(
    bytes: &[u8],
    rec: &Im3Record,
    spectrum: &mut Im3Spectrum,
) -> Result<()> {
    for sub_rec in im3_container_children(bytes, rec)? {
        if sub_rec.rec_type != 0 {
            continue;
        }
        for sub_sub_rec in im3_container_children(bytes, &sub_rec)? {
            if sub_sub_rec.name == "Wavelengths" && sub_sub_rec.rec_type == 7 {
                spectrum.wavelengths = im3_float_entries(bytes, &sub_sub_rec);
            } else if sub_sub_rec.name == "Magnitudes" && sub_sub_rec.rec_type == 7 {
                spectrum.magnitudes = im3_float_entries(bytes, &sub_sub_rec);
            }
        }
    }
    Ok(())
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
    } else if let Some(values) = im3_bool_entries(bytes, rec) {
        if values.len() == 1 {
            Ok(Some(MetadataValue::Bool(values[0])))
        } else {
            Ok(Some(MetadataValue::String(
                values
                    .iter()
                    .map(bool::to_string)
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
    let mut spectra: Vec<Im3Spectrum> = Vec::new();
    for rec in top_records.iter().filter(|rec| rec.rec_type == 0) {
        im3_collect_dataset_candidates(&bytes, rec, &mut datasets)?;
        // Mirror the SpectralLibrary branch of Java IM3Reader.initFile: for each
        // top-level container, locate the SpectralLibrary container, then descend
        // the unnamed container / Spectra / Values nesting to its Spectrum records.
        for sub_ds in im3_container_children(&bytes, rec)? {
            if sub_ds.rec_type == 0 && sub_ds.name == "SpectralLibrary" {
                for sl_container in im3_container_children(&bytes, &sub_ds)? {
                    if sl_container.rec_type != 0 {
                        continue;
                    }
                    for sl_spectra in im3_container_children(&bytes, &sl_container)? {
                        if sl_spectra.rec_type != 0 || sl_spectra.name != "Spectra" {
                            continue;
                        }
                        for sl_rec in im3_container_children(&bytes, &sl_spectra)? {
                            if sl_rec.name != "Values" || sl_rec.rec_type != 0 {
                                continue;
                            }
                            for spectrum_rec in im3_container_children(&bytes, &sl_rec)? {
                                if spectrum_rec.rec_type == 0 {
                                    spectra.push(im3_parse_spectrum(&bytes, &spectrum_rec)?);
                                }
                            }
                        }
                    }
                }
            }
        }
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
            &spectra,
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
    spectra: &[Im3Spectrum],
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

    // Surface the file-level SpectralLibrary spectra (Java IM3Reader.getSpectra)
    // as bounded series metadata: name, wavelengths, and magnitudes per spectrum.
    if !spectra.is_empty() {
        meta.series_metadata.insert(
            "im3.spectral_library.spectrum_count".into(),
            MetadataValue::Int(spectra.len() as i64),
        );
        for (index, spectrum) in spectra.iter().enumerate() {
            if let Some(name) = &spectrum.name {
                meta.series_metadata.insert(
                    format!("im3.spectral_library.spectrum.{index}.name"),
                    MetadataValue::String(name.clone()),
                );
            }
            if let Some(wavelengths) = &spectrum.wavelengths {
                meta.series_metadata.insert(
                    format!("im3.spectral_library.spectrum.{index}.wavelengths"),
                    MetadataValue::String(
                        wavelengths
                            .iter()
                            .map(|value| value.to_string())
                            .collect::<Vec<_>>()
                            .join(","),
                    ),
                );
            }
            if let Some(magnitudes) = &spectrum.magnitudes {
                meta.series_metadata.insert(
                    format!("im3.spectral_library.spectrum.{index}.magnitudes"),
                    MetadataValue::String(
                        magnitudes
                            .iter()
                            .map(|value| value.to_string())
                            .collect::<Vec<_>>()
                            .join(","),
                    ),
                );
            }
        }
    }

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
    // Decoded typed-record plane metadata (Java GetElapsedTime / GetExposureTime
    // / GetXPosition / GetYPosition / GetZPosition + GetInterplaneSpacing) used to
    // build OME plane timing/position.
    elapsed_times: Vec<f64>,
    channel_exposures: Vec<Option<i64>>,
    stage_positions: Vec<(f64, f64, f64)>,
    interplane_spacing: Option<f64>,
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
    descr: String,
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

// ── SlideBook 7 typed YAML record decoder ────────────────────────────────────
//
// Faithful translation of the `ClassDecoder`/record-class layer of the upstream
// Java `SlideBook7Reader`. Upstream parses each `*.yaml` record file with the
// third-party snakeyaml library (`Yaml.compose`) into a node tree, then a
// reflection-driven `ClassDecoder.Decode` walks `StartClass`/`EndClass`-delimited
// blocks and assigns recognised attributes onto typed record fields (unknown
// attributes are dropped — Java `DecodeUnknownString` has an empty body). Rust
// has no runtime reflection and the project takes no YAML dependency, so the
// snakeyaml `compose` is reproduced by `slidebook7_yaml_compose` and the
// reflection assignment becomes an explicit per-record `assign` closure; the
// `StartClass`/`ClassName`/`EndClass` control flow of `Decode` is preserved
// exactly. Declaring only the fields a record consumes is faithful: upstream
// silently ignores attributes that have no matching field.

#[derive(Clone, Debug, PartialEq)]
enum Sb7Node {
    Scalar(String),
    Sequence(Vec<Sb7Node>),
    Mapping(Vec<(String, Sb7Node)>),
}

// Stands in for snakeyaml `Yaml.compose`: an indentation-driven parse of the
// block mappings, block/flow scalar sequences, and nested mappings that the
// SlideBook 7 record files use. Duplicate keys (`StartClass`/`EndClass`) are
// preserved in order, matching snakeyaml's `MappingNode.getValue()` list.
fn slidebook7_yaml_compose(text: &str) -> Sb7Node {
    let lines: Vec<&str> = text.lines().collect();
    let mut pos = 0usize;
    slidebook7_yaml_compose_block(&lines, &mut pos, 0)
}

fn slidebook7_yaml_compose_block(lines: &[&str], pos: &mut usize, indent: usize) -> Sb7Node {
    let mut is_sequence = false;
    let mut peek = *pos;
    while peek < lines.len() {
        let trimmed = lines[peek].trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            peek += 1;
            continue;
        }
        is_sequence = trimmed == "-" || trimmed.starts_with("- ");
        break;
    }

    if is_sequence {
        let mut items = Vec::new();
        while *pos < lines.len() {
            let line = lines[*pos];
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                *pos += 1;
                continue;
            }
            let line_indent = line.len() - line.trim_start().len();
            if line_indent < indent {
                break;
            }
            if !(trimmed == "-" || trimmed.starts_with("- ")) {
                break;
            }
            let rest = trimmed[1..].trim_start().to_string();
            *pos += 1;
            if rest.is_empty() {
                items.push(slidebook7_yaml_compose_block(lines, pos, indent + 1));
            } else {
                items.push(slidebook7_yaml_scalar_or_flow(&rest));
            }
        }
        Sb7Node::Sequence(items)
    } else {
        let mut entries = Vec::new();
        while *pos < lines.len() {
            let line = lines[*pos];
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                *pos += 1;
                continue;
            }
            let line_indent = line.len() - line.trim_start().len();
            if line_indent < indent {
                break;
            }
            if line_indent > indent {
                *pos += 1;
                continue;
            }
            let Some((key, value)) = trimmed.split_once(':') else {
                *pos += 1;
                continue;
            };
            let key = key.trim().to_string();
            let value = value.trim().to_string();
            *pos += 1;
            if value.is_empty() {
                let mut child_indent = indent + 1;
                let mut child_peek = *pos;
                while child_peek < lines.len() {
                    let child_trimmed = lines[child_peek].trim();
                    if child_trimmed.is_empty() || child_trimmed.starts_with('#') {
                        child_peek += 1;
                        continue;
                    }
                    child_indent = lines[child_peek].len() - lines[child_peek].trim_start().len();
                    break;
                }
                if child_indent > indent {
                    let node = slidebook7_yaml_compose_block(lines, pos, child_indent);
                    entries.push((key, node));
                } else {
                    entries.push((key, Sb7Node::Scalar(String::new())));
                }
            } else {
                entries.push((key, slidebook7_yaml_scalar_or_flow(&value)));
            }
        }
        Sb7Node::Mapping(entries)
    }
}

fn slidebook7_yaml_scalar_or_flow(value: &str) -> Sb7Node {
    let trimmed = value.trim();
    if let Some(inner) = trimmed.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        let items = inner
            .split(',')
            .map(|item| item.trim())
            .filter(|item| !item.is_empty())
            .map(|item| Sb7Node::Scalar(item.to_string()))
            .collect();
        Sb7Node::Sequence(items)
    } else {
        Sb7Node::Scalar(trimmed.to_string())
    }
}

// Translation of Java `ClassDecoder.RestoreSpecialCharacters`.
fn slidebook7_restore_special_characters(value: &str) -> String {
    value
        .replace("_#9;", "\t")
        .replace("_#10;", "\n")
        .replace("_#13;", "\r")
        .replace("_#34;", "\"")
        .replace("_#58;", ":")
        .replace("_#92;", "\\")
        .replace("_#91;", "[")
        .replace("_#93;", "]")
        .replace("_#124;", "|")
        .replace("_#60;", "<")
        .replace("_#62;", ">")
        .replace("_#32;", " ")
        .replace("__empty", "")
}

// Translation of Java `ClassDecoder.Decode(MappingNode, inStartIndex)`. Walks the
// `StartClass`/`EndClass` blocks; for the first block whose leading `ClassName`
// attribute equals `sb_name`, invokes `assign` for each subsequent attribute and
// returns the index just past the consumed block (so composed records decode in
// sequence). Sequence-valued attributes keep snakeyaml's "element 0 is the count"
// convention, exposed to `assign` as the raw `Sb7Node::Sequence`.
fn slidebook7_class_decode(
    node: &Sb7Node,
    start_index: usize,
    sb_name: &str,
    mut assign: impl FnMut(&str, &Sb7Node),
) -> usize {
    let Sb7Node::Mapping(tuples) = node else {
        return start_index + 1;
    };
    let mut class_index = start_index;
    while class_index < tuples.len() {
        let (key, value) = &tuples[class_index];
        if key == "EndClass" {
            break;
        }
        if key != "StartClass" {
            class_index += 1;
            continue;
        }
        if let Sb7Node::Mapping(attrs) = value {
            for (attr_index, (attr_name, attr_value)) in attrs.iter().enumerate() {
                if attr_index == 0 {
                    if attr_name != "ClassName" {
                        break;
                    }
                    match attr_value {
                        Sb7Node::Scalar(name) if name == sb_name => {}
                        _ => break,
                    }
                    continue;
                }
                assign(attr_name, attr_value);
            }
        }
        class_index += 1;
    }
    class_index + 1
}

// Decodes a snakeyaml `Integer[]`/`Long[]`/`Float[]` sequence the way Java does:
// a list whose first element is the count, returning the trailing values. Java
// drops the array entirely when the list size is <= 1.
fn slidebook7_decode_number_array(node: &Sb7Node, first_is_size: bool) -> Option<Vec<f64>> {
    let Sb7Node::Sequence(items) = node else {
        return None;
    };
    // With first_is_size (Java GetXArray firstIsSize=true) element 0 is the count
    // and is dropped; Java drops the whole array when the list size is <= 1.
    let values: &[Sb7Node] = if first_is_size {
        if items.len() <= 1 {
            return None;
        }
        &items[1..]
    } else {
        &items[..]
    };
    let mut out = Vec::with_capacity(values.len());
    for item in values {
        match item {
            Sb7Node::Scalar(value) => out.push(value.trim().parse::<f64>().ok()?),
            _ => return None,
        }
    }
    Some(out)
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Sb7LensDef70 {
    name: Option<String>,
    na: Option<f64>,
    micron_per_pixel: Option<f64>,
    actual_magnification: Option<f64>,
    camera_name: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Sb7OptovarDef70 {
    name: Option<String>,
    magnification: Option<f64>,
    default: Option<bool>,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Sb7MainViewRecord70 {
    view_id: Option<i64>,
    red_channel: Option<i64>,
    green_channel: Option<i64>,
    blue_channel: Option<i64>,
    low: Option<Vec<f64>>,
    high: Option<Vec<f64>>,
    gamma: Option<Vec<f64>>,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Sb7ImageRecord70 {
    width: Option<i64>,
    height: Option<i64>,
    num_planes: Option<i64>,
    num_channels: Option<i64>,
    num_timepoints: Option<i64>,
    num_masks: Option<i64>,
    name: Option<String>,
    info: Option<String>,
    unique_id: Option<String>,
    thumbnail: Option<Vec<f64>>,
    lens: Sb7LensDef70,
    optovar: Sb7OptovarDef70,
    main_view: Sb7MainViewRecord70,
}

impl Sb7ImageRecord70 {
    // Translation of Java `CImageRecord70.Decode`: decode this record's own
    // fields, then chain the composed lens / optovar / main-view sub-records from
    // the returned index.
    fn decode(node: &Sb7Node) -> Self {
        let mut record = Sb7ImageRecord70::default();

        let mut last_index = slidebook7_class_decode(node, 0, "CImageRecord70", |name, value| {
            if let Sb7Node::Scalar(text) = value {
                let text = text.trim();
                match name {
                    "mWidth" => record.width = text.parse::<i64>().ok(),
                    "mHeight" => record.height = text.parse::<i64>().ok(),
                    "mNumPlanes" => record.num_planes = text.parse::<i64>().ok(),
                    "mNumChannels" => record.num_channels = text.parse::<i64>().ok(),
                    "mNumTimepoints" => record.num_timepoints = text.parse::<i64>().ok(),
                    "mNumMasks" => record.num_masks = text.parse::<i64>().ok(),
                    "mName" => record.name = Some(slidebook7_restore_special_characters(text)),
                    "mInfo" => record.info = Some(slidebook7_restore_special_characters(text)),
                    "mUniqueId" => {
                        record.unique_id = Some(slidebook7_restore_special_characters(text))
                    }
                    _ => {}
                }
            } else if name == "mThumbNail" {
                record.thumbnail = slidebook7_decode_number_array(value, true);
            }
        });

        last_index = slidebook7_class_decode(node, last_index, "CLensDef70", |name, value| {
            if let Sb7Node::Scalar(text) = value {
                let text = text.trim();
                match name {
                    "mName" => record.lens.name = Some(slidebook7_restore_special_characters(text)),
                    "mNA" => record.lens.na = text.parse::<f64>().ok(),
                    "mMicronPerPixel" => record.lens.micron_per_pixel = text.parse::<f64>().ok(),
                    "mActualMagnification" => {
                        record.lens.actual_magnification = text.parse::<f64>().ok()
                    }
                    "mCameraName" => {
                        record.lens.camera_name = Some(slidebook7_restore_special_characters(text))
                    }
                    _ => {}
                }
            }
        });

        last_index = slidebook7_class_decode(node, last_index, "COptovarDef70", |name, value| {
            if let Sb7Node::Scalar(text) = value {
                let text = text.trim();
                match name {
                    "mName" => {
                        record.optovar.name = Some(slidebook7_restore_special_characters(text))
                    }
                    "mMagnification" => record.optovar.magnification = text.parse::<f64>().ok(),
                    "mDefault" => record.optovar.default = Some(text == "true"),
                    _ => {}
                }
            }
        });

        slidebook7_class_decode(node, last_index, "CMainViewRecord70", |name, value| {
            if let Sb7Node::Scalar(text) = value {
                let text = text.trim();
                match name {
                    "mViewID" => record.main_view.view_id = text.parse::<i64>().ok(),
                    "mRedChannel" => record.main_view.red_channel = text.parse::<i64>().ok(),
                    "mGreenChannel" => record.main_view.green_channel = text.parse::<i64>().ok(),
                    "mBlueChannel" => record.main_view.blue_channel = text.parse::<i64>().ok(),
                    _ => {}
                }
            } else {
                match name {
                    "mLow" => record.main_view.low = slidebook7_decode_number_array(value, true),
                    "mHigh" => record.main_view.high = slidebook7_decode_number_array(value, true),
                    "mGamma" => {
                        record.main_view.gamma = slidebook7_decode_number_array(value, true)
                    }
                    _ => {}
                }
            }
        });

        record
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Sb7ExposureRecord70 {
    exposure_time: Option<i64>,
    interplane_spacing: Option<f64>,
    z_start_position: Option<f64>,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Sb7FluorDef70 {
    name: Option<String>,
    excitation_lambda: Option<f64>,
    lambda: Option<f64>,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Sb7ChannelDef70 {
    name: Option<String>,
    camera_name: Option<String>,
    fluor: Sb7FluorDef70,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Sb7ChannelRecord70 {
    num_planes: Option<i64>,
    data_type: Option<i64>,
    exposure: Sb7ExposureRecord70,
    channel_def: Sb7ChannelDef70,
}

impl Sb7ChannelDef70 {
    // Translation of Java `CChannelDef70.Decode`: own fields, then the chained
    // `CFluorDef70` (which inherits `ClassDecoder.Decode`, so its fields are
    // assigned through the generic walk).
    fn decode(node: &Sb7Node, start_index: usize) -> (Self, usize) {
        let mut record = Sb7ChannelDef70::default();
        let after_def =
            slidebook7_class_decode(node, start_index, "CChannelDef70", |name, value| {
                if let Sb7Node::Scalar(text) = value {
                    let text = text.trim();
                    match name {
                        "mName" => record.name = Some(slidebook7_restore_special_characters(text)),
                        "mCameraName" => {
                            record.camera_name = Some(slidebook7_restore_special_characters(text))
                        }
                        _ => {}
                    }
                }
            });
        let after_fluor = slidebook7_class_decode(node, after_def, "CFluorDef70", |name, value| {
            if let Sb7Node::Scalar(text) = value {
                let text = text.trim();
                match name {
                    "mName" => {
                        record.fluor.name = Some(slidebook7_restore_special_characters(text))
                    }
                    "mExcitationLambda" => {
                        record.fluor.excitation_lambda = text.parse::<f64>().ok()
                    }
                    "mLambda" => record.fluor.lambda = text.parse::<f64>().ok(),
                    _ => {}
                }
            }
        });
        (record, after_fluor)
    }
}

impl Sb7ChannelRecord70 {
    // Translation of Java `CChannelRecord70.Decode`: own fields, then the chained
    // `CExposureRecord70` (inherited decode) and `CChannelDef70`.
    fn decode(node: &Sb7Node, start_index: usize) -> (Self, usize) {
        let mut record = Sb7ChannelRecord70::default();
        let after_record =
            slidebook7_class_decode(node, start_index, "CChannelRecord70", |name, value| {
                if let Sb7Node::Scalar(text) = value {
                    let text = text.trim();
                    match name {
                        "mNumPlanes" => record.num_planes = text.parse::<i64>().ok(),
                        "mDataType" => record.data_type = text.parse::<i64>().ok(),
                        _ => {}
                    }
                }
            });
        let after_exposure =
            slidebook7_class_decode(node, after_record, "CExposureRecord70", |name, value| {
                if let Sb7Node::Scalar(text) = value {
                    let text = text.trim();
                    match name {
                        "mExposureTime" => record.exposure.exposure_time = text.parse::<i64>().ok(),
                        "mInterplaneSpacing" => {
                            record.exposure.interplane_spacing = text.parse::<f64>().ok()
                        }
                        "mZStartPosition" => {
                            record.exposure.z_start_position = text.parse::<f64>().ok()
                        }
                        _ => {}
                    }
                }
            });
        let (channel_def, next_index) = Sb7ChannelDef70::decode(node, after_exposure);
        record.channel_def = channel_def;
        (record, next_index)
    }
}

// Translation of Java `ClassDecoder.FindNextClass`: return the `ClassName` and
// index of the next `StartClass` block, or `("", -1)` at `EndClass`/end.
fn slidebook7_find_next_class(node: &Sb7Node, start_index: usize) -> (String, i64) {
    let Sb7Node::Mapping(tuples) = node else {
        return (String::new(), -1);
    };
    let mut class_index = start_index;
    while class_index < tuples.len() {
        let (key, value) = &tuples[class_index];
        if key == "EndClass" {
            break;
        }
        if key != "StartClass" {
            class_index += 1;
            continue;
        }
        if let Sb7Node::Mapping(attrs) = value {
            if let Some((attr_name, attr_value)) = attrs.first() {
                if attr_name != "ClassName" {
                    break;
                }
                if let Sb7Node::Scalar(name) = attr_value {
                    return (name.clone(), class_index as i64);
                }
            }
        }
        class_index += 1;
    }
    (String::new(), -1)
}

// Translation of Java `CImageGroup.LoadChannelRecord`'s decode loop: one
// `CChannelRecord70` per channel, then consume any trailing remap/manip/
// histogram classes until the next `CChannelRecord70` or end-of-data. (Java has
// no terminal `else` for an unrecognised intervening class, which would spin;
// the unknown-class arm breaks instead.)
fn slidebook7_load_channel_records(
    node: &Sb7Node,
    num_channels: usize,
) -> (Vec<Sb7ChannelRecord70>, Sb7ChannelExtras) {
    let mut channels = Vec::new();
    let mut extras = Sb7ChannelExtras::default();
    let mut last_index = 0usize;
    for _ in 0..num_channels {
        let (channel_record, next_index) = Sb7ChannelRecord70::decode(node, last_index);
        last_index = next_index;
        channels.push(channel_record);
        loop {
            let (class_name, pair_index) = slidebook7_find_next_class(node, last_index);
            if pair_index < 0 {
                break;
            }
            let pair_index = pair_index as usize;
            match class_name.as_str() {
                "CChannelRecord70" => {
                    last_index = pair_index;
                    break;
                }
                "CHistogramRecord70" => {
                    let (histogram, next) = Sb7HistogramRecord70::decode(node, pair_index);
                    extras.histograms.push(histogram);
                    last_index = next;
                }
                "CRemapChannelLUT70" => {
                    let (lut, next) = Sb7RemapChannelLut70::decode(node, pair_index);
                    extras.remap_luts.push(lut);
                    last_index = next;
                }
                "CAlignManipRecord70" => {
                    let (manip, next) = Sb7AlignManipRecord70::decode(node, pair_index);
                    extras.align_manips.push(manip);
                    last_index = next;
                }
                "CRatioManipRecord70" => {
                    let (manip, next) = Sb7RatioManipRecord70::decode(node, pair_index);
                    extras.ratio_manips.push(manip);
                    last_index = next;
                }
                "CFRETManipRecord70" => {
                    let (manip, next) = Sb7FretManipRecord70::decode(node, pair_index);
                    extras.fret_manips.push(manip);
                    last_index = next;
                }
                "CRemapManipRecord70" => {
                    let (manip, next) = Sb7RemapManipRecord70::decode(node, pair_index);
                    extras.remap_manips.push(manip);
                    last_index = next;
                }
                _ => break,
            }
        }
    }
    (channels, extras)
}

// Translation of Java `ClassDecoder.GetStringValue`: scan the mapping tuples
// from `start_index` for a scalar-valued `key_name`, returning its value and the
// index just past it (or `("", -1)` if not found).
fn slidebook7_get_string_value(
    node: &Sb7Node,
    start_index: usize,
    key_name: &str,
    restore: bool,
) -> (String, i64) {
    let Sb7Node::Mapping(tuples) = node else {
        return (String::new(), -1);
    };
    let mut index = start_index;
    while index < tuples.len() {
        let (key, value) = &tuples[index];
        if key == key_name {
            if let Sb7Node::Scalar(text) = value {
                let out = if restore {
                    slidebook7_restore_special_characters(text)
                } else {
                    text.clone()
                };
                return (out, index as i64 + 1);
            }
            return (String::new(), -1);
        }
        index += 1;
    }
    (String::new(), -1)
}

// Translation of Java `ClassDecoder.GetIntegerValue`: as above but parsed to an
// integer. Returns `(value, next_index)` or `(-1, -1)`.
fn slidebook7_get_integer_value(node: &Sb7Node, start_index: usize, key_name: &str) -> (i64, i64) {
    let (text, next) = slidebook7_get_string_value(node, start_index, key_name, false);
    if next >= 0 {
        if let Ok(value) = text.trim().parse::<i64>() {
            return (value, next);
        }
    }
    (-1, -1)
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Sb7MaskRecord70 {
    name: Option<String>,
    persistent_submasks: Option<i64>,
    centroid_feature: Option<String>,
    centroid_channel: Option<i64>,
}

impl Sb7MaskRecord70 {
    fn decode(node: &Sb7Node, start_index: usize) -> (Self, usize) {
        let mut record = Sb7MaskRecord70::default();
        let next = slidebook7_class_decode(node, start_index, "CMaskRecord70", |name, value| {
            if let Sb7Node::Scalar(text) = value {
                let text = text.trim();
                match name {
                    "mName" => record.name = Some(slidebook7_restore_special_characters(text)),
                    "mPersistentSubmasks" => record.persistent_submasks = text.parse::<i64>().ok(),
                    "mCentroidFeature" => {
                        record.centroid_feature = Some(slidebook7_restore_special_characters(text))
                    }
                    "mCentroidChannel" => record.centroid_channel = text.parse::<i64>().ok(),
                    _ => {}
                }
            }
        });
        (record, next)
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Sb7DataTableHeader70 {
    channel_index: Option<i64>,
    rows: Option<i64>,
    columns: Option<i64>,
    planes: Option<i64>,
    value_type: Option<i64>,
    table_type: Option<i64>,
}

impl Sb7DataTableHeader70 {
    fn decode(node: &Sb7Node, start_index: usize) -> (Self, usize) {
        let mut record = Sb7DataTableHeader70::default();
        let next = slidebook7_class_decode(
            node,
            start_index,
            "CDataTableHeaderRecord70",
            |name, value| {
                if let Sb7Node::Scalar(text) = value {
                    let text = text.trim();
                    match name {
                        "mChannelIndex" => record.channel_index = text.parse::<i64>().ok(),
                        "mRows" => record.rows = text.parse::<i64>().ok(),
                        "mColumns" => record.columns = text.parse::<i64>().ok(),
                        "mPlanes" => record.planes = text.parse::<i64>().ok(),
                        "mValueType" => record.value_type = text.parse::<i64>().ok(),
                        "mTableType" => record.table_type = text.parse::<i64>().ok(),
                        _ => {}
                    }
                }
            },
        );
        (record, next)
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Sb7Annotation70 {
    graphic_type: Option<i64>,
    dependency_type: Option<i64>,
    text: Option<String>,
    group_id: Option<i64>,
    plane_id: Option<i64>,
    sequence_id: Option<i64>,
    object_id: Option<i64>,
}

impl Sb7Annotation70 {
    fn decode(node: &Sb7Node, start_index: usize) -> (Self, usize) {
        let mut record = Sb7Annotation70::default();
        let next = slidebook7_class_decode(node, start_index, "CAnnotation70", |name, value| {
            if let Sb7Node::Scalar(text) = value {
                let text = text.trim();
                match name {
                    "mGraphicType70" => record.graphic_type = text.parse::<i64>().ok(),
                    "mDependencyType70" => record.dependency_type = text.parse::<i64>().ok(),
                    "mText" => record.text = Some(slidebook7_restore_special_characters(text)),
                    "mGroupId" => record.group_id = text.parse::<i64>().ok(),
                    "mPlaneId" => record.plane_id = text.parse::<i64>().ok(),
                    "mSequenceId" => record.sequence_id = text.parse::<i64>().ok(),
                    "mObjectId" => record.object_id = text.parse::<i64>().ok(),
                    _ => {}
                }
            }
        });
        (record, next)
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Sb7CubeAnnotation70 {
    is_background: Option<bool>,
    region_index: Option<i64>,
    is_frap: Option<bool>,
    frap_device: Option<String>,
    ann: Sb7Annotation70,
}

impl Sb7CubeAnnotation70 {
    // Translation of Java `CCubeAnnotation70.Decode`: own fields, then the chained
    // base `CAnnotation70`.
    fn decode(node: &Sb7Node, start_index: usize) -> (Self, usize) {
        let mut record = Sb7CubeAnnotation70::default();
        let after_cube =
            slidebook7_class_decode(node, start_index, "CCubeAnnotation70", |name, value| {
                if let Sb7Node::Scalar(text) = value {
                    let text = text.trim();
                    match name {
                        "mIsBackground" => record.is_background = Some(text == "true"),
                        "mRegionIndex" => record.region_index = text.parse::<i64>().ok(),
                        "mIsFRAP" => record.is_frap = Some(text == "true"),
                        "mFRAPDevice" => {
                            record.frap_device = Some(slidebook7_restore_special_characters(text))
                        }
                        _ => {}
                    }
                }
            });
        let (ann, next) = Sb7Annotation70::decode(node, after_cube);
        record.ann = ann;
        (record, next)
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Sb7FrapRegionAnnotation70 {
    xml: Option<String>,
    ann: Sb7Annotation70,
    region_count: usize,
}

impl Sb7FrapRegionAnnotation70 {
    // Translation of Java `CFRAPRegionAnnotation70.Decode`: own field (mXML), the
    // chained base `CAnnotation70`, then `theNumRegions` cube annotations.
    fn decode(node: &Sb7Node, start_index: usize) -> (Self, usize) {
        let mut record = Sb7FrapRegionAnnotation70::default();
        let after_frap = slidebook7_class_decode(
            node,
            start_index,
            "CFRAPRegionAnnotation70",
            |name, value| {
                if let Sb7Node::Scalar(text) = value {
                    if name == "mXML" {
                        record.xml = Some(slidebook7_restore_special_characters(text.trim()));
                    }
                }
            },
        );
        let (ann, after_ann) = Sb7Annotation70::decode(node, after_frap);
        record.ann = ann;
        let (num_regions, after_count) =
            slidebook7_get_integer_value(node, after_ann, "theNumRegions");
        let mut index = if after_count >= 0 {
            after_count as usize
        } else {
            after_ann
        };
        let count = num_regions.max(0) as usize;
        for _ in 0..count {
            let (_region, next) = Sb7CubeAnnotation70::decode(node, index);
            index = next;
        }
        record.region_count = count;
        (record, index)
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Sb7UnknownAnnotation70 {
    ann: Sb7Annotation70,
}

impl Sb7UnknownAnnotation70 {
    // Translation of Java `CUnknownAnnotation70.Decode`: empty own class, then the
    // chained base `CAnnotation70`.
    fn decode(node: &Sb7Node, start_index: usize) -> (Self, usize) {
        let after_unknown =
            slidebook7_class_decode(node, start_index, "CUnknownAnnotation70", |_, _| {});
        let (ann, next) = Sb7Annotation70::decode(node, after_unknown);
        (Sb7UnknownAnnotation70 { ann }, next)
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Sb7Annotations {
    cube: Vec<Sb7CubeAnnotation70>,
    base: Vec<Sb7Annotation70>,
    frap: Vec<Sb7FrapRegionAnnotation70>,
    unknown: Vec<Sb7UnknownAnnotation70>,
}

// Translation of Java `CImageGroup.LoadMaks`: a leading `theNumMasks` count, that
// many `CMaskRecord70`, then per-timepoint `theMaskCompressedSizes`/
// `theMaskFileOffsets` position tables. Returns the mask records and the number
// of position tables seen.
fn slidebook7_load_masks(node: &Sb7Node) -> (Vec<Sb7MaskRecord70>, usize) {
    let Sb7Node::Mapping(tuples) = node else {
        return (Vec::new(), 0);
    };
    let mut masks = Vec::new();
    let (num_masks, _) = slidebook7_get_integer_value(node, 0, "theNumMasks");
    if num_masks <= 0 {
        return (masks, 0);
    }
    let mut last_index = 1usize;
    for _ in 0..num_masks {
        let (mask, next) = Sb7MaskRecord70::decode(node, last_index);
        last_index = next;
        masks.push(mask);
    }
    let mut position_tables = 0usize;
    loop {
        let (_timepoint, next) =
            slidebook7_get_integer_value(node, last_index, "theTimepointIndex");
        if next < 0 {
            break;
        }
        let mut index = next as usize;
        match tuples.get(index) {
            Some((key, _)) if key == "theMaskCompressedSizes" => index += 1,
            _ => break,
        }
        match tuples.get(index) {
            Some((key, _)) if key == "theMaskFileOffsets" => index += 1,
            _ => break,
        }
        position_tables += 1;
        last_index = index;
    }
    (masks, position_tables)
}

// Translation of Java `CImageGroup.LoadAnnotations`: a leading
// `CDataTableHeaderRecord70`, then per-timepoint cube / base / FRAP-region /
// unknown annotation lists, each prefixed by its `*ListSize` count.
fn slidebook7_load_annotations(node: &Sb7Node) -> (Sb7DataTableHeader70, Vec<Sb7Annotations>) {
    let (header, after_header) = Sb7DataTableHeader70::decode(node, 0);
    let mut timepoints = Vec::new();
    let mut last_index: i64 = after_header as i64;
    loop {
        let (_timepoint, tp_next) =
            slidebook7_get_integer_value(node, last_index.max(0) as usize, "theTimepointIndex");
        if tp_next < 0 {
            break;
        }
        last_index = tp_next;
        let mut anno = Sb7Annotations::default();

        let (cube_size, next) =
            slidebook7_get_integer_value(node, last_index as usize, "theCubeAnnotation70ListSize");
        if next < 0 {
            break;
        }
        last_index = next;
        for _ in 0..cube_size.max(0) {
            let (cube, after) = Sb7CubeAnnotation70::decode(node, last_index as usize);
            anno.cube.push(cube);
            last_index = after as i64;
        }

        let (base_size, next) =
            slidebook7_get_integer_value(node, last_index as usize, "theAnnotation70ListSize");
        if next < 0 {
            break;
        }
        last_index = next;
        for _ in 0..base_size.max(0) {
            let (ann, after) = Sb7Annotation70::decode(node, last_index as usize);
            anno.base.push(ann);
            last_index = after as i64;
        }

        let (frap_size, next) = slidebook7_get_integer_value(
            node,
            last_index as usize,
            "theFRAPRegionAnnotation70ListSize",
        );
        if next < 0 {
            break;
        }
        last_index = next;
        for _ in 0..frap_size.max(0) {
            let (frap, after) = Sb7FrapRegionAnnotation70::decode(node, last_index as usize);
            anno.frap.push(frap);
            last_index = after as i64;
        }

        let (unknown_size, next) = slidebook7_get_integer_value(
            node,
            last_index as usize,
            "theUnknownAnnotation70ListSize",
        );
        if next < 0 {
            break;
        }
        last_index = next;
        for _ in 0..unknown_size.max(0) {
            let (unknown, after) = Sb7UnknownAnnotation70::decode(node, last_index as usize);
            anno.unknown.push(unknown);
            last_index = after as i64;
        }

        timepoints.push(anno);
    }
    (header, timepoints)
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Sb7RemapChannelLut70 {
    remap_type: Option<i64>,
    low_desired: Option<f64>,
    high_desired: Option<f64>,
    low_given: Option<i64>,
    high_given: Option<i64>,
    built_table: Option<bool>,
    equation_string: Option<String>,
}

impl Sb7RemapChannelLut70 {
    fn decode(node: &Sb7Node, start_index: usize) -> (Self, usize) {
        let mut record = Sb7RemapChannelLut70::default();
        let next =
            slidebook7_class_decode(node, start_index, "CRemapChannelLUT70", |name, value| {
                if let Sb7Node::Scalar(text) = value {
                    let text = text.trim();
                    match name {
                        "mRemapType" => record.remap_type = text.parse::<i64>().ok(),
                        "mLowDesired" => record.low_desired = text.parse::<f64>().ok(),
                        "mHighDesired" => record.high_desired = text.parse::<f64>().ok(),
                        "mLowGiven" => record.low_given = text.parse::<i64>().ok(),
                        "mHighGiven" => record.high_given = text.parse::<i64>().ok(),
                        "mBuiltTable" => record.built_table = Some(text == "true"),
                        "mEquationString" => {
                            record.equation_string =
                                Some(slidebook7_restore_special_characters(text))
                        }
                        _ => {}
                    }
                }
            });
        (record, next)
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Sb7AlignManipRecord70 {
    manip_id: Option<i64>,
    x_offset: Option<f64>,
    y_offset: Option<f64>,
    z_offset: Option<f64>,
}

impl Sb7AlignManipRecord70 {
    fn decode(node: &Sb7Node, start_index: usize) -> (Self, usize) {
        let mut record = Sb7AlignManipRecord70::default();
        let next =
            slidebook7_class_decode(node, start_index, "CAlignManipRecord70", |name, value| {
                if let Sb7Node::Scalar(text) = value {
                    let text = text.trim();
                    match name {
                        "mManipID" => record.manip_id = text.parse::<i64>().ok(),
                        "mXOffset" => record.x_offset = text.parse::<f64>().ok(),
                        "mYOffset" => record.y_offset = text.parse::<f64>().ok(),
                        "mZOffset" => record.z_offset = text.parse::<f64>().ok(),
                        _ => {}
                    }
                }
            });
        (record, next)
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Sb7RatioManipRecord70 {
    manip_id: Option<i64>,
    kd: Option<f64>,
    rmin: Option<f64>,
    rmax: Option<f64>,
    beta: Option<f64>,
}

impl Sb7RatioManipRecord70 {
    fn decode(node: &Sb7Node, start_index: usize) -> (Self, usize) {
        let mut record = Sb7RatioManipRecord70::default();
        let next =
            slidebook7_class_decode(node, start_index, "CRatioManipRecord70", |name, value| {
                if let Sb7Node::Scalar(text) = value {
                    let text = text.trim();
                    match name {
                        "mManipID" => record.manip_id = text.parse::<i64>().ok(),
                        "mKd" => record.kd = text.parse::<f64>().ok(),
                        "mRmin" => record.rmin = text.parse::<f64>().ok(),
                        "mRmax" => record.rmax = text.parse::<f64>().ok(),
                        "mBeta" => record.beta = text.parse::<f64>().ok(),
                        _ => {}
                    }
                }
            });
        (record, next)
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Sb7FretManipRecord70 {
    manip_id: Option<i64>,
    fret_paradigm: Option<i64>,
    fd_dd: Option<f64>,
    fa_aa: Option<f64>,
}

impl Sb7FretManipRecord70 {
    fn decode(node: &Sb7Node, start_index: usize) -> (Self, usize) {
        let mut record = Sb7FretManipRecord70::default();
        let next =
            slidebook7_class_decode(node, start_index, "CFRETManipRecord70", |name, value| {
                if let Sb7Node::Scalar(text) = value {
                    let text = text.trim();
                    match name {
                        "mManipID" => record.manip_id = text.parse::<i64>().ok(),
                        "mFRETParadigm" => record.fret_paradigm = text.parse::<i64>().ok(),
                        "mFdDd" => record.fd_dd = text.parse::<f64>().ok(),
                        "mFaAa" => record.fa_aa = text.parse::<f64>().ok(),
                        _ => {}
                    }
                }
            });
        (record, next)
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Sb7RemapManipRecord70 {
    manip_id: Option<i64>,
    remap_type: Option<i64>,
    num_calib_points: Option<i64>,
}

impl Sb7RemapManipRecord70 {
    fn decode(node: &Sb7Node, start_index: usize) -> (Self, usize) {
        let mut record = Sb7RemapManipRecord70::default();
        let next =
            slidebook7_class_decode(node, start_index, "CRemapManipRecord70", |name, value| {
                if let Sb7Node::Scalar(text) = value {
                    let text = text.trim();
                    match name {
                        "mManipID" => record.manip_id = text.parse::<i64>().ok(),
                        "mRemapType" => record.remap_type = text.parse::<i64>().ok(),
                        "mNumCalibPoints" => record.num_calib_points = text.parse::<i64>().ok(),
                        _ => {}
                    }
                }
            });
        (record, next)
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Sb7ChannelExtras {
    histograms: Vec<Sb7HistogramRecord70>,
    remap_luts: Vec<Sb7RemapChannelLut70>,
    align_manips: Vec<Sb7AlignManipRecord70>,
    ratio_manips: Vec<Sb7RatioManipRecord70>,
    fret_manips: Vec<Sb7FretManipRecord70>,
    remap_manips: Vec<Sb7RemapManipRecord70>,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Sb7HistogramRecord70 {
    min: Option<i64>,
    max: Option<i64>,
    mean: Option<f64>,
    num_bins: Option<i64>,
    channel_index: Option<i64>,
}

impl Sb7HistogramRecord70 {
    fn decode(node: &Sb7Node, start_index: usize) -> (Self, usize) {
        let mut record = Sb7HistogramRecord70::default();
        let next =
            slidebook7_class_decode(node, start_index, "CHistogramRecord70", |name, value| {
                if let Sb7Node::Scalar(text) = value {
                    let text = text.trim();
                    match name {
                        "mMin" => record.min = text.parse::<i64>().ok(),
                        "mMax" => record.max = text.parse::<i64>().ok(),
                        "mMean" => record.mean = text.parse::<f64>().ok(),
                        "mNumBins" => record.num_bins = text.parse::<i64>().ok(),
                        "mChannelIndex" => record.channel_index = text.parse::<i64>().ok(),
                        _ => {}
                    }
                }
            });
        (record, next)
    }
}

// Translation of Java `CImageGroup.LoadElapsedTimes`: the `theElapsedTimes` tuple
// holds a count-prefixed integer array.
fn slidebook7_load_elapsed_times(node: &Sb7Node) -> Vec<f64> {
    let Sb7Node::Mapping(tuples) = node else {
        return Vec::new();
    };
    if let Some((key, value)) = tuples.first() {
        if key == "theElapsedTimes" {
            if let Some(values) = slidebook7_decode_number_array(value, true) {
                return values;
            }
        }
    }
    Vec::new()
}

// Translation of Java `CImageGroup.LoadSAPositions`: a leading `theImageCount`,
// then that many count-prefixed `theSAPositions` arrays.
fn slidebook7_load_sa_positions(node: &Sb7Node) -> Vec<Vec<f64>> {
    let Sb7Node::Mapping(tuples) = node else {
        return Vec::new();
    };
    let (image_count, mut next) = slidebook7_get_integer_value(node, 0, "theImageCount");
    let mut positions = Vec::new();
    if next < 0 {
        return positions;
    }
    for _ in 0..image_count.max(0) {
        let index = next as usize;
        match tuples.get(index) {
            Some((key, value)) if key == "theSAPositions" => {
                positions.push(slidebook7_decode_number_array(value, true).unwrap_or_default());
                next = index as i64 + 1;
            }
            _ => break,
        }
    }
    positions
}

// Translation of Java `CImageGroup.LoadStagePosition`: a `StructArraySize` count,
// then a flat `StructArrayValues` float array (no size prefix) grouped into XYZ
// points.
fn slidebook7_load_stage_positions(node: &Sb7Node) -> Vec<(f64, f64, f64)> {
    let Sb7Node::Mapping(tuples) = node else {
        return Vec::new();
    };
    let (_size, next) = slidebook7_get_integer_value(node, 0, "StructArraySize");
    let mut points = Vec::new();
    if next < 0 {
        return points;
    }
    let index = next as usize;
    if let Some((key, value)) = tuples.get(index) {
        if key == "StructArrayValues" {
            if let Some(values) = slidebook7_decode_number_array(value, false) {
                let mut p = 0usize;
                while p + 2 < values.len() {
                    points.push((values[p], values[p + 1], values[p + 2]));
                    p += 3;
                }
            }
        }
    }
    points
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Sb7AuxTable {
    xml_descriptor: Option<String>,
    value_count: usize,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Sb7AuxXmlTable {
    xml_descriptor: Option<String>,
    xml_data: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Sb7AuxData {
    float_tables: Vec<Sb7AuxTable>,
    double_tables: Vec<Sb7AuxTable>,
    sint32_tables: Vec<Sb7AuxTable>,
    sint64_tables: Vec<Sb7AuxTable>,
    xml_tables: Vec<Sb7AuxXmlTable>,
}

// Translation of Java `CImageGroup.LoadAuxData`: five sequential typed-table
// sections (float / double / sint32 / sint64 / serialized-XML), each prefixed by
// a `theAux*DataTablesSize` count; every numeric table is a
// `CDataTableHeaderRecord70`, a `theXMLDescriptor`, and a count-prefixed
// `theAuxData` array. Java translates this as one method, so the sections are
// kept inline rather than factored into a helper.
fn slidebook7_load_aux_data(node: &Sb7Node) -> Sb7AuxData {
    let mut aux = Sb7AuxData::default();
    let Sb7Node::Mapping(tuples) = node else {
        return aux;
    };
    let mut last_index: i64 = 0;

    // FLOAT
    let (count, next) =
        slidebook7_get_integer_value(node, last_index as usize, "theAuxFloatDataTablesSize");
    if next < 0 {
        return aux;
    }
    last_index = next;
    for _ in 0..count.max(0) {
        let (_header, after_header) = Sb7DataTableHeader70::decode(node, last_index as usize);
        let (descriptor, after_desc) =
            slidebook7_get_string_value(node, after_header, "theXMLDescriptor", true);
        if after_desc < 0 {
            return aux;
        }
        let index = after_desc as usize;
        let value_count = match tuples.get(index) {
            Some((key, value)) if key == "theAuxData" => {
                slidebook7_decode_number_array(value, true).map_or(0, |v| v.len())
            }
            _ => return aux,
        };
        aux.float_tables.push(Sb7AuxTable {
            xml_descriptor: Some(descriptor).filter(|s| !s.is_empty()),
            value_count,
        });
        last_index = index as i64 + 1;
    }

    // DOUBLE
    let (count, next) =
        slidebook7_get_integer_value(node, last_index as usize, "theAuxDoubleDataTablesSize");
    if next < 0 {
        return aux;
    }
    last_index = next;
    for _ in 0..count.max(0) {
        let (_header, after_header) = Sb7DataTableHeader70::decode(node, last_index as usize);
        let (descriptor, after_desc) =
            slidebook7_get_string_value(node, after_header, "theXMLDescriptor", true);
        if after_desc < 0 {
            return aux;
        }
        let index = after_desc as usize;
        let value_count = match tuples.get(index) {
            Some((key, value)) if key == "theAuxData" => {
                slidebook7_decode_number_array(value, true).map_or(0, |v| v.len())
            }
            _ => return aux,
        };
        aux.double_tables.push(Sb7AuxTable {
            xml_descriptor: Some(descriptor).filter(|s| !s.is_empty()),
            value_count,
        });
        last_index = index as i64 + 1;
    }

    // SINT32
    let (count, next) =
        slidebook7_get_integer_value(node, last_index as usize, "theAuxSInt32DataTablesSize");
    if next < 0 {
        return aux;
    }
    last_index = next;
    for _ in 0..count.max(0) {
        let (_header, after_header) = Sb7DataTableHeader70::decode(node, last_index as usize);
        let (descriptor, after_desc) =
            slidebook7_get_string_value(node, after_header, "theXMLDescriptor", true);
        if after_desc < 0 {
            return aux;
        }
        let index = after_desc as usize;
        let value_count = match tuples.get(index) {
            Some((key, value)) if key == "theAuxData" => {
                slidebook7_decode_number_array(value, true).map_or(0, |v| v.len())
            }
            _ => return aux,
        };
        aux.sint32_tables.push(Sb7AuxTable {
            xml_descriptor: Some(descriptor).filter(|s| !s.is_empty()),
            value_count,
        });
        last_index = index as i64 + 1;
    }

    // SINT64
    let (count, next) =
        slidebook7_get_integer_value(node, last_index as usize, "theAuxSInt64DataTablesSize");
    if next < 0 {
        return aux;
    }
    last_index = next;
    for _ in 0..count.max(0) {
        let (_header, after_header) = Sb7DataTableHeader70::decode(node, last_index as usize);
        let (descriptor, after_desc) =
            slidebook7_get_string_value(node, after_header, "theXMLDescriptor", true);
        if after_desc < 0 {
            return aux;
        }
        let index = after_desc as usize;
        let value_count = match tuples.get(index) {
            Some((key, value)) if key == "theAuxData" => {
                slidebook7_decode_number_array(value, true).map_or(0, |v| v.len())
            }
            _ => return aux,
        };
        aux.sint64_tables.push(Sb7AuxTable {
            xml_descriptor: Some(descriptor).filter(|s| !s.is_empty()),
            value_count,
        });
        last_index = index as i64 + 1;
    }

    // XML / serialized
    let (count, next) =
        slidebook7_get_integer_value(node, last_index as usize, "theAuxSerializedDataTablesSize");
    if next < 0 {
        return aux;
    }
    last_index = next;
    for _ in 0..count.max(0) {
        let (_header, after_header) = Sb7DataTableHeader70::decode(node, last_index as usize);
        let (descriptor, after_desc) =
            slidebook7_get_string_value(node, after_header, "theXMLDescriptor", true);
        if after_desc < 0 {
            return aux;
        }
        let (_size, after_size) =
            slidebook7_get_integer_value(node, after_desc as usize, "theXmlAuxDataSize");
        if after_size < 0 {
            return aux;
        }
        let (xml_data, after_xml) =
            slidebook7_get_string_value(node, after_size as usize, "theXmlAuxData", true);
        if after_xml < 0 {
            return aux;
        }
        aux.xml_tables.push(Sb7AuxXmlTable {
            xml_descriptor: Some(descriptor).filter(|s| !s.is_empty()),
            xml_data: Some(xml_data).filter(|s| !s.is_empty()),
        });
        last_index = after_xml;
    }

    aux
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
    let mut probes = Vec::new();
    for kind in 0..3 {
        let mut decoded = Vec::new();
        let result = match kind {
            0 => flate2::read::GzDecoder::new(data).read_to_end(&mut decoded),
            1 => flate2::read::ZlibDecoder::new(data).read_to_end(&mut decoded),
            _ => flate2::read::DeflateDecoder::new(data).read_to_end(&mut decoded),
        };
        let label = match kind {
            0 => "gzip",
            1 => "zlib",
            _ => "deflate",
        };
        match &result {
            Ok(_) if decoded.starts_with(b"\x93NUMPY") => return Ok(decoded),
            Ok(_) => probes.push(format!(
                "{label}: inflated {} bytes but not a NPY payload",
                decoded.len()
            )),
            Err(err) => probes.push(format!("{label}: {err}")),
        }
    }
    let container = if data.starts_with(b"PK\x03\x04") {
        "ZIP container"
    } else if data.starts_with(b"\x1f\x8b") {
        "gzip container"
    } else {
        "unknown container"
    };
    let preview_len = data.len().min(8);
    let first_bytes = data[..preview_len]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(" ");
    Err(BioFormatsError::UnsupportedFormat(format!(
        "SlideBook 7 NPYZ image data is not a gzip/zlib/deflate-compressed NPY payload ({container}, first bytes [{first_bytes}]): {}; probes {}",
        path.display(),
        probes.join(", ")
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
        descr: descr.to_string(),
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
        // Real SlideBook 7 record files use the typed StartClass/EndClass layout
        // decoded by CImageRecord70 (the faithful upstream path). Files that do
        // not carry a CImageRecord70 class leave these fields unset, and the
        // bounded line-scan fallback recovers the dimensions instead.
        let typed = Sb7ImageRecord70::decode(&slidebook7_yaml_compose(&record));
        let typed_dim = |value: Option<i64>| -> Option<u32> {
            value.and_then(|v| u32::try_from(v).ok()).filter(|v| *v > 0)
        };
        let size_x = typed_dim(typed.width)
            .or_else(|| slidebook7_yaml_u32(&record, &["mWidth", "Width", "NumColumns"]))
            .ok_or_else(|| slidebook7_missing_record(&record_path, "mWidth"))?;
        let size_y = typed_dim(typed.height)
            .or_else(|| slidebook7_yaml_u32(&record, &["mHeight", "Height", "NumRows"]))
            .ok_or_else(|| slidebook7_missing_record(&record_path, "mHeight"))?;
        let size_z = typed_dim(typed.num_planes)
            .or_else(|| slidebook7_yaml_u32(&record, &["mNumPlanes", "NumPlanes", "Planes"]))
            .unwrap_or(1)
            .max(1);
        let declared_c = typed_dim(typed.num_channels)
            .or_else(|| slidebook7_yaml_u32(&record, &["mNumChannels", "NumChannels", "Channels"]))
            .unwrap_or(1)
            .max(1);
        let declared_t = typed_dim(typed.num_timepoints)
            .or_else(|| {
                slidebook7_yaml_u32(&record, &["mNumTimepoints", "NumTimepoints", "Timepoints"])
            })
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

        // CompressionDictionary.yaml maps each ImageData payload to its declared
        // compression; a `.npyz` payload must name a supported compressed
        // algorithm or the native group is rejected as inconsistent.
        let dictionary_path = group.join("CompressionDictionary.yaml");
        let mut dictionary_entries = 0usize;
        if let Ok(dictionary) = std::fs::read_to_string(&dictionary_path) {
            for line in dictionary.lines() {
                let trimmed = line.trim();
                if trimmed.is_empty() || trimmed.starts_with('#') {
                    continue;
                }
                let Some((name, compression)) = trimmed.split_once(':') else {
                    continue;
                };
                let name = name.trim();
                let compression = compression.trim();
                if name.is_empty() || compression.is_empty() {
                    continue;
                }
                dictionary_entries += 1;
                if name.ends_with(".npyz")
                    && !matches!(
                        compression.to_ascii_lowercase().as_str(),
                        "gzip" | "zlib" | "deflate"
                    )
                {
                    return Err(BioFormatsError::UnsupportedFormat(format!(
                        "SlideBook 7 compression dictionary {} declares unsupported compression {:?} for {}",
                        dictionary_path.display(),
                        compression,
                        name
                    )));
                }
            }
        }

        let first = parse_slidebook7_npy_header(&files[0].path)?;
        // Java SlideBook7Reader.initFile derives the reported pixel type from
        // CImageGroup.GetBytesPerPixel(), which is hard-coded to return 2, then
        // calls FormatTools.pixelTypeFromBytes(2, false, true) -> UINT16, and sets
        // ms.littleEndian = true unconditionally. So the reader always reports
        // unsigned little-endian 16-bit samples regardless of the NPY descriptor
        // (e.g. an "i2" descriptor is still surfaced as UINT16). The actual NPY
        // byte width still drives the on-disk plane stride (Java ReadPlane uses
        // mNpyHeader.mBytesPerPixel), which we preserve via `bytes_per_sample`.
        let pixel_type = PixelType::Uint16;
        let bits_per_pixel = (pixel_type.bytes_per_sample() * 8) as u8;
        let bytes_per_sample = first.pixel_type.bytes_per_sample();
        for file in &files[1..] {
            let npy = parse_slidebook7_npy_header(&file.path)?;
            if npy.pixel_type != first.pixel_type {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "SlideBook 7 mixed NPY pixel types are unsupported in {}: {} has descriptor {:?} ({:?}), {} has descriptor {:?} ({:?})",
                    group.display(),
                    files[0].path.display(),
                    first.descr,
                    first.pixel_type,
                    file.path.display(),
                    npy.descr,
                    npy.pixel_type,
                )));
            }
            if npy.little_endian != first.little_endian {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "SlideBook 7 mixed NPY byte orders are unsupported in {}: {} has descriptor {:?} ({}), {} has descriptor {:?} ({})",
                    group.display(),
                    files[0].path.display(),
                    first.descr,
                    if first.little_endian { "little-endian" } else { "big-endian" },
                    file.path.display(),
                    npy.descr,
                    if npy.little_endian { "little-endian" } else { "big-endian" },
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
            // Java sets ms.littleEndian = true unconditionally in initFile.
            is_little_endian: true,
            is_interleaved: true,
            ..ImageMetadata::default()
        };
        meta.series_metadata = slidebook7_image_record_metadata(&record);
        // Project the typed CImageRecord70 fields (and its chained lens / optovar
        // / main-view sub-records) when the record file used the StartClass layout.
        if typed.width.is_some() {
            let mut put_string = |key: &str, value: &Option<String>| {
                if let Some(value) = value {
                    meta.series_metadata.insert(
                        key.into(),
                        crate::common::metadata::MetadataValue::String(value.clone()),
                    );
                }
            };
            put_string("slidebook7.image_record.name", &typed.name);
            put_string("slidebook7.image_record.info", &typed.info);
            put_string("slidebook7.image_record.unique_id", &typed.unique_id);
            put_string("slidebook7.image_record.lens.name", &typed.lens.name);
            put_string(
                "slidebook7.image_record.lens.camera_name",
                &typed.lens.camera_name,
            );
            put_string("slidebook7.image_record.optovar.name", &typed.optovar.name);
            let mut put_float = |key: &str, value: Option<f64>| {
                if let Some(value) = value.filter(|v| v.is_finite()) {
                    meta.series_metadata.insert(
                        key.into(),
                        crate::common::metadata::MetadataValue::Float(value),
                    );
                }
            };
            put_float(
                "slidebook7.image_record.lens.numerical_aperture",
                typed.lens.na,
            );
            put_float(
                "slidebook7.image_record.lens.micron_per_pixel",
                typed.lens.micron_per_pixel,
            );
            put_float(
                "slidebook7.image_record.lens.magnification",
                typed.lens.actual_magnification,
            );
            put_float(
                "slidebook7.image_record.optovar.magnification",
                typed.optovar.magnification,
            );
            let mut put_int = |key: &str, value: Option<i64>| {
                if let Some(value) = value {
                    meta.series_metadata.insert(
                        key.into(),
                        crate::common::metadata::MetadataValue::Int(value),
                    );
                }
            };
            put_int("slidebook7.image_record.num_masks", typed.num_masks);
            put_int(
                "slidebook7.image_record.main_view.view_id",
                typed.main_view.view_id,
            );
            put_int(
                "slidebook7.image_record.main_view.red_channel",
                typed.main_view.red_channel,
            );
            put_int(
                "slidebook7.image_record.main_view.green_channel",
                typed.main_view.green_channel,
            );
            put_int(
                "slidebook7.image_record.main_view.blue_channel",
                typed.main_view.blue_channel,
            );
            if let Some(default) = typed.optovar.default {
                meta.series_metadata.insert(
                    "slidebook7.image_record.optovar.default".into(),
                    crate::common::metadata::MetadataValue::Bool(default),
                );
            }
            for (key, values) in [
                ("slidebook7.image_record.thumbnail", &typed.thumbnail),
                (
                    "slidebook7.image_record.main_view.low",
                    &typed.main_view.low,
                ),
                (
                    "slidebook7.image_record.main_view.high",
                    &typed.main_view.high,
                ),
                (
                    "slidebook7.image_record.main_view.gamma",
                    &typed.main_view.gamma,
                ),
            ] {
                if let Some(values) = values {
                    meta.series_metadata.insert(
                        format!("{key}.count"),
                        crate::common::metadata::MetadataValue::Int(values.len() as i64),
                    );
                }
            }
        }
        // Decoded plane-metadata inputs collected from the typed records below
        // and consumed by the OME plane builder.
        let mut channel_exposures: Vec<Option<i64>> = Vec::new();
        let mut interplane_spacing: Option<f64> = None;
        let mut elapsed_times: Vec<f64> = Vec::new();
        let mut stage_positions: Vec<(f64, f64, f64)> = Vec::new();

        // ChannelRecord.yaml carries one typed CChannelRecord70 per channel
        // (Java CImageGroup.LoadChannelRecord). When present, project the channel
        // name / camera / exposure / fluor wavelengths.
        let channel_record_path = group.join("ChannelRecord.yaml");
        if let Ok(channel_text) = std::fs::read_to_string(&channel_record_path) {
            let channel_node = slidebook7_yaml_compose(&channel_text);
            let (channel_records, extras) =
                slidebook7_load_channel_records(&channel_node, declared_c as usize);
            channel_exposures = channel_records
                .iter()
                .map(|channel| channel.exposure.exposure_time)
                .collect();
            // Java GetInterplaneSpacing reads channel 0's exposure record.
            interplane_spacing = channel_records
                .first()
                .and_then(|channel| channel.exposure.interplane_spacing)
                .filter(|v| v.is_finite());
            if !extras.histograms.is_empty() {
                meta.series_metadata.insert(
                    "slidebook7.histogram.count".into(),
                    crate::common::metadata::MetadataValue::Int(extras.histograms.len() as i64),
                );
                if let Some(histogram) = extras.histograms.first() {
                    for (key, value) in [
                        ("slidebook7.histogram.0.min", histogram.min),
                        ("slidebook7.histogram.0.max", histogram.max),
                        ("slidebook7.histogram.0.num_bins", histogram.num_bins),
                        (
                            "slidebook7.histogram.0.channel_index",
                            histogram.channel_index,
                        ),
                    ] {
                        if let Some(value) = value {
                            meta.series_metadata.insert(
                                key.into(),
                                crate::common::metadata::MetadataValue::Int(value),
                            );
                        }
                    }
                    if let Some(mean) = histogram.mean.filter(|v| v.is_finite()) {
                        meta.series_metadata.insert(
                            "slidebook7.histogram.0.mean".into(),
                            crate::common::metadata::MetadataValue::Float(mean),
                        );
                    }
                }
            }
            // Channel manipulation/LUT records (Java mRemapChannelLUTList /
            // mAlignManipRecordList / mRatioManipRecordList / mFRETManipRecList /
            // mRemapManipRecList).
            for (key, count) in [
                ("slidebook7.remap_lut.count", extras.remap_luts.len()),
                ("slidebook7.align_manip.count", extras.align_manips.len()),
                ("slidebook7.ratio_manip.count", extras.ratio_manips.len()),
                ("slidebook7.fret_manip.count", extras.fret_manips.len()),
                ("slidebook7.remap_manip.count", extras.remap_manips.len()),
            ] {
                if count > 0 {
                    meta.series_metadata.insert(
                        key.into(),
                        crate::common::metadata::MetadataValue::Int(count as i64),
                    );
                }
            }
            if let Some(lut) = extras.remap_luts.first() {
                if let Some(remap_type) = lut.remap_type {
                    meta.series_metadata.insert(
                        "slidebook7.remap_lut.0.remap_type".into(),
                        crate::common::metadata::MetadataValue::Int(remap_type),
                    );
                }
                if let Some(equation) = &lut.equation_string {
                    meta.series_metadata.insert(
                        "slidebook7.remap_lut.0.equation".into(),
                        crate::common::metadata::MetadataValue::String(equation.clone()),
                    );
                }
                for (key, value) in [
                    ("slidebook7.remap_lut.0.low_desired", lut.low_desired),
                    ("slidebook7.remap_lut.0.high_desired", lut.high_desired),
                ] {
                    if let Some(value) = value.filter(|v| v.is_finite()) {
                        meta.series_metadata.insert(
                            key.into(),
                            crate::common::metadata::MetadataValue::Float(value),
                        );
                    }
                }
                for (key, value) in [
                    ("slidebook7.remap_lut.0.low_given", lut.low_given),
                    ("slidebook7.remap_lut.0.high_given", lut.high_given),
                ] {
                    if let Some(value) = value {
                        meta.series_metadata.insert(
                            key.into(),
                            crate::common::metadata::MetadataValue::Int(value),
                        );
                    }
                }
                if let Some(built) = lut.built_table {
                    meta.series_metadata.insert(
                        "slidebook7.remap_lut.0.built_table".into(),
                        crate::common::metadata::MetadataValue::Bool(built),
                    );
                }
            }
            if let Some(align) = extras.align_manips.first() {
                for (key, value) in [("slidebook7.align_manip.0.manip_id", align.manip_id)] {
                    if let Some(value) = value {
                        meta.series_metadata.insert(
                            key.into(),
                            crate::common::metadata::MetadataValue::Int(value),
                        );
                    }
                }
                for (key, value) in [
                    ("slidebook7.align_manip.0.x_offset", align.x_offset),
                    ("slidebook7.align_manip.0.y_offset", align.y_offset),
                    ("slidebook7.align_manip.0.z_offset", align.z_offset),
                ] {
                    if let Some(value) = value.filter(|v| v.is_finite()) {
                        meta.series_metadata.insert(
                            key.into(),
                            crate::common::metadata::MetadataValue::Float(value),
                        );
                    }
                }
            }
            if let Some(ratio) = extras.ratio_manips.first() {
                if let Some(manip_id) = ratio.manip_id {
                    meta.series_metadata.insert(
                        "slidebook7.ratio_manip.0.manip_id".into(),
                        crate::common::metadata::MetadataValue::Int(manip_id),
                    );
                }
                for (key, value) in [
                    ("slidebook7.ratio_manip.0.kd", ratio.kd),
                    ("slidebook7.ratio_manip.0.rmin", ratio.rmin),
                    ("slidebook7.ratio_manip.0.rmax", ratio.rmax),
                    ("slidebook7.ratio_manip.0.beta", ratio.beta),
                ] {
                    if let Some(value) = value.filter(|v| v.is_finite()) {
                        meta.series_metadata.insert(
                            key.into(),
                            crate::common::metadata::MetadataValue::Float(value),
                        );
                    }
                }
            }
            if let Some(fret) = extras.fret_manips.first() {
                for (key, value) in [
                    ("slidebook7.fret_manip.0.manip_id", fret.manip_id),
                    ("slidebook7.fret_manip.0.fret_paradigm", fret.fret_paradigm),
                ] {
                    if let Some(value) = value {
                        meta.series_metadata.insert(
                            key.into(),
                            crate::common::metadata::MetadataValue::Int(value),
                        );
                    }
                }
                for (key, value) in [
                    ("slidebook7.fret_manip.0.fd_dd", fret.fd_dd),
                    ("slidebook7.fret_manip.0.fa_aa", fret.fa_aa),
                ] {
                    if let Some(value) = value.filter(|v| v.is_finite()) {
                        meta.series_metadata.insert(
                            key.into(),
                            crate::common::metadata::MetadataValue::Float(value),
                        );
                    }
                }
            }
            if let Some(remap) = extras.remap_manips.first() {
                for (key, value) in [
                    ("slidebook7.remap_manip.0.manip_id", remap.manip_id),
                    ("slidebook7.remap_manip.0.remap_type", remap.remap_type),
                    (
                        "slidebook7.remap_manip.0.num_calib_points",
                        remap.num_calib_points,
                    ),
                ] {
                    if let Some(value) = value {
                        meta.series_metadata.insert(
                            key.into(),
                            crate::common::metadata::MetadataValue::Int(value),
                        );
                    }
                }
            }
            for (index, channel) in channel_records.iter().enumerate() {
                let prefix = format!("slidebook7.channel.{index}");
                if let Some(name) = channel.channel_def.name.as_ref().or(channel
                    .channel_def
                    .fluor
                    .name
                    .as_ref())
                {
                    meta.series_metadata.insert(
                        format!("{prefix}.name"),
                        crate::common::metadata::MetadataValue::String(name.clone()),
                    );
                }
                if let Some(camera) = &channel.channel_def.camera_name {
                    meta.series_metadata.insert(
                        format!("{prefix}.camera_name"),
                        crate::common::metadata::MetadataValue::String(camera.clone()),
                    );
                }
                if let Some(exposure) = channel.exposure.exposure_time {
                    meta.series_metadata.insert(
                        format!("{prefix}.exposure_time"),
                        crate::common::metadata::MetadataValue::Int(exposure),
                    );
                }
                if let Some(value) = channel
                    .channel_def
                    .fluor
                    .excitation_lambda
                    .filter(|v| v.is_finite() && *v > 0.0)
                {
                    meta.series_metadata.insert(
                        format!("{prefix}.excitation_wavelength"),
                        crate::common::metadata::MetadataValue::Float(value),
                    );
                }
                if let Some(value) = channel
                    .channel_def
                    .fluor
                    .lambda
                    .filter(|v| v.is_finite() && *v > 0.0)
                {
                    meta.series_metadata.insert(
                        format!("{prefix}.emission_wavelength"),
                        crate::common::metadata::MetadataValue::Float(value),
                    );
                }
                if let Some(planes) = channel.num_planes {
                    meta.series_metadata.insert(
                        format!("{prefix}.num_planes"),
                        crate::common::metadata::MetadataValue::Int(planes),
                    );
                }
            }
        }
        // MaskRecord.yaml carries the typed CMaskRecord70 list plus per-timepoint
        // position tables (Java CImageGroup.LoadMaks).
        if let Ok(mask_text) = std::fs::read_to_string(group.join("MaskRecord.yaml")) {
            let (masks, position_tables) =
                slidebook7_load_masks(&slidebook7_yaml_compose(&mask_text));
            meta.series_metadata.insert(
                "slidebook7.mask.count".into(),
                crate::common::metadata::MetadataValue::Int(masks.len() as i64),
            );
            meta.series_metadata.insert(
                "slidebook7.mask.position_tables".into(),
                crate::common::metadata::MetadataValue::Int(position_tables as i64),
            );
            for (index, mask) in masks.iter().enumerate() {
                let prefix = format!("slidebook7.mask.{index}");
                if let Some(name) = &mask.name {
                    meta.series_metadata.insert(
                        format!("{prefix}.name"),
                        crate::common::metadata::MetadataValue::String(name.clone()),
                    );
                }
                if let Some(feature) = &mask.centroid_feature {
                    meta.series_metadata.insert(
                        format!("{prefix}.centroid_feature"),
                        crate::common::metadata::MetadataValue::String(feature.clone()),
                    );
                }
                if let Some(channel) = mask.centroid_channel {
                    meta.series_metadata.insert(
                        format!("{prefix}.centroid_channel"),
                        crate::common::metadata::MetadataValue::Int(channel),
                    );
                }
                if let Some(submasks) = mask.persistent_submasks {
                    meta.series_metadata.insert(
                        format!("{prefix}.persistent_submasks"),
                        crate::common::metadata::MetadataValue::Int(submasks),
                    );
                }
            }
        }

        // AnnotationRecord.yaml carries a CDataTableHeaderRecord70 plus the
        // per-timepoint cube/base/FRAP/unknown annotation graph (Java
        // CImageGroup.LoadAnnotations).
        if let Ok(annotation_text) = std::fs::read_to_string(group.join("AnnotationRecord.yaml")) {
            let (header, timepoints) =
                slidebook7_load_annotations(&slidebook7_yaml_compose(&annotation_text));
            meta.series_metadata.insert(
                "slidebook7.annotation.timepoint_count".into(),
                crate::common::metadata::MetadataValue::Int(timepoints.len() as i64),
            );
            for (key, value) in [
                (
                    "slidebook7.annotation.data_table.channel_index",
                    header.channel_index,
                ),
                ("slidebook7.annotation.data_table.rows", header.rows),
                ("slidebook7.annotation.data_table.columns", header.columns),
                ("slidebook7.annotation.data_table.planes", header.planes),
                (
                    "slidebook7.annotation.data_table.value_type",
                    header.value_type,
                ),
                (
                    "slidebook7.annotation.data_table.table_type",
                    header.table_type,
                ),
            ] {
                if let Some(value) = value {
                    meta.series_metadata.insert(
                        key.into(),
                        crate::common::metadata::MetadataValue::Int(value),
                    );
                }
            }
            let cube_total: usize = timepoints.iter().map(|tp| tp.cube.len()).sum();
            let base_total: usize = timepoints.iter().map(|tp| tp.base.len()).sum();
            let frap_total: usize = timepoints.iter().map(|tp| tp.frap.len()).sum();
            let unknown_total: usize = timepoints.iter().map(|tp| tp.unknown.len()).sum();
            for (key, value) in [
                ("slidebook7.annotation.cube_count", cube_total),
                ("slidebook7.annotation.base_count", base_total),
                ("slidebook7.annotation.frap_region_count", frap_total),
                ("slidebook7.annotation.unknown_count", unknown_total),
            ] {
                meta.series_metadata.insert(
                    key.into(),
                    crate::common::metadata::MetadataValue::Int(value as i64),
                );
            }
            // Bounded sample of the first base/cube/FRAP/unknown annotation.
            if let Some(base) = timepoints.iter().find_map(|tp| tp.base.first()) {
                if let Some(text) = &base.text {
                    meta.series_metadata.insert(
                        "slidebook7.annotation.0.text".into(),
                        crate::common::metadata::MetadataValue::String(text.clone()),
                    );
                }
                for (key, value) in [
                    ("slidebook7.annotation.0.graphic_type", base.graphic_type),
                    (
                        "slidebook7.annotation.0.dependency_type",
                        base.dependency_type,
                    ),
                    ("slidebook7.annotation.0.group_id", base.group_id),
                    ("slidebook7.annotation.0.plane_id", base.plane_id),
                    ("slidebook7.annotation.0.sequence_id", base.sequence_id),
                    ("slidebook7.annotation.0.object_id", base.object_id),
                ] {
                    if let Some(value) = value {
                        meta.series_metadata.insert(
                            key.into(),
                            crate::common::metadata::MetadataValue::Int(value),
                        );
                    }
                }
            }
            if let Some(cube) = timepoints.iter().find_map(|tp| tp.cube.first()) {
                if let Some(is_background) = cube.is_background {
                    meta.series_metadata.insert(
                        "slidebook7.annotation.cube.0.is_background".into(),
                        crate::common::metadata::MetadataValue::Bool(is_background),
                    );
                }
                if let Some(region_index) = cube.region_index {
                    meta.series_metadata.insert(
                        "slidebook7.annotation.cube.0.region_index".into(),
                        crate::common::metadata::MetadataValue::Int(region_index),
                    );
                }
                if let Some(is_frap) = cube.is_frap {
                    meta.series_metadata.insert(
                        "slidebook7.annotation.cube.0.is_frap".into(),
                        crate::common::metadata::MetadataValue::Bool(is_frap),
                    );
                }
                if let Some(device) = &cube.frap_device {
                    meta.series_metadata.insert(
                        "slidebook7.annotation.cube.0.frap_device".into(),
                        crate::common::metadata::MetadataValue::String(device.clone()),
                    );
                }
                if let Some(object_id) = cube.ann.object_id {
                    meta.series_metadata.insert(
                        "slidebook7.annotation.cube.0.annotation_object_id".into(),
                        crate::common::metadata::MetadataValue::Int(object_id),
                    );
                }
            }
            if let Some(frap) = timepoints.iter().find_map(|tp| tp.frap.first()) {
                if let Some(xml) = &frap.xml {
                    meta.series_metadata.insert(
                        "slidebook7.annotation.frap.0.xml".into(),
                        crate::common::metadata::MetadataValue::String(xml.clone()),
                    );
                }
                meta.series_metadata.insert(
                    "slidebook7.annotation.frap.0.region_count".into(),
                    crate::common::metadata::MetadataValue::Int(frap.region_count as i64),
                );
                if let Some(graphic_type) = frap.ann.graphic_type {
                    meta.series_metadata.insert(
                        "slidebook7.annotation.frap.0.annotation_graphic_type".into(),
                        crate::common::metadata::MetadataValue::Int(graphic_type),
                    );
                }
            }
            if let Some(unknown) = timepoints.iter().find_map(|tp| tp.unknown.first()) {
                if let Some(plane_id) = unknown.ann.plane_id {
                    meta.series_metadata.insert(
                        "slidebook7.annotation.unknown.0.annotation_plane_id".into(),
                        crate::common::metadata::MetadataValue::Int(plane_id),
                    );
                }
            }
        }

        // ElapsedTimes.yaml / SAPositionData.yaml / StagePositionData.yaml carry
        // count-prefixed numeric arrays (Java LoadElapsedTimes / LoadSAPositions /
        // LoadStagePosition).
        if let Ok(text) = std::fs::read_to_string(group.join("ElapsedTimes.yaml")) {
            elapsed_times = slidebook7_load_elapsed_times(&slidebook7_yaml_compose(&text));
            if !elapsed_times.is_empty() {
                meta.series_metadata.insert(
                    "slidebook7.elapsed_times.count".into(),
                    crate::common::metadata::MetadataValue::Int(elapsed_times.len() as i64),
                );
                if let Some(first) = elapsed_times.first().filter(|v| v.is_finite()) {
                    meta.series_metadata.insert(
                        "slidebook7.elapsed_times.0".into(),
                        crate::common::metadata::MetadataValue::Float(*first),
                    );
                }
            }
        }
        if let Ok(text) = std::fs::read_to_string(group.join("SAPositionData.yaml")) {
            let sa_positions = slidebook7_load_sa_positions(&slidebook7_yaml_compose(&text));
            if !sa_positions.is_empty() {
                meta.series_metadata.insert(
                    "slidebook7.sa_positions.image_count".into(),
                    crate::common::metadata::MetadataValue::Int(sa_positions.len() as i64),
                );
                meta.series_metadata.insert(
                    "slidebook7.sa_positions.0.count".into(),
                    crate::common::metadata::MetadataValue::Int(sa_positions[0].len() as i64),
                );
            }
        }
        if let Ok(text) = std::fs::read_to_string(group.join("StagePositionData.yaml")) {
            stage_positions = slidebook7_load_stage_positions(&slidebook7_yaml_compose(&text));
            if let Some((x, y, z)) = stage_positions.first().copied() {
                meta.series_metadata.insert(
                    "slidebook7.stage_positions.count".into(),
                    crate::common::metadata::MetadataValue::Int(stage_positions.len() as i64),
                );
                for (key, value) in [
                    ("slidebook7.stage_positions.0.x", x),
                    ("slidebook7.stage_positions.0.y", y),
                    ("slidebook7.stage_positions.0.z", z),
                ] {
                    if value.is_finite() {
                        meta.series_metadata.insert(
                            key.into(),
                            crate::common::metadata::MetadataValue::Float(value),
                        );
                    }
                }
            }
        }

        // AuxData.yaml carries the typed multi-section aux data tables (Java
        // LoadAuxData).
        if let Ok(text) = std::fs::read_to_string(group.join("AuxData.yaml")) {
            let aux = slidebook7_load_aux_data(&slidebook7_yaml_compose(&text));
            for (key, count) in [
                ("slidebook7.aux.float_tables", aux.float_tables.len()),
                ("slidebook7.aux.double_tables", aux.double_tables.len()),
                ("slidebook7.aux.sint32_tables", aux.sint32_tables.len()),
                ("slidebook7.aux.sint64_tables", aux.sint64_tables.len()),
                ("slidebook7.aux.xml_tables", aux.xml_tables.len()),
            ] {
                if count > 0 {
                    meta.series_metadata.insert(
                        key.into(),
                        crate::common::metadata::MetadataValue::Int(count as i64),
                    );
                }
            }
            if let Some(table) = aux.float_tables.first() {
                meta.series_metadata.insert(
                    "slidebook7.aux.float.0.value_count".into(),
                    crate::common::metadata::MetadataValue::Int(table.value_count as i64),
                );
                if let Some(descriptor) = &table.xml_descriptor {
                    meta.series_metadata.insert(
                        "slidebook7.aux.float.0.descriptor".into(),
                        crate::common::metadata::MetadataValue::String(descriptor.clone()),
                    );
                }
            }
            for (key, table) in [
                (
                    "slidebook7.aux.double.0.value_count",
                    aux.double_tables.first(),
                ),
                (
                    "slidebook7.aux.sint32.0.value_count",
                    aux.sint32_tables.first(),
                ),
                (
                    "slidebook7.aux.sint64.0.value_count",
                    aux.sint64_tables.first(),
                ),
            ] {
                if let Some(table) = table {
                    meta.series_metadata.insert(
                        key.into(),
                        crate::common::metadata::MetadataValue::Int(table.value_count as i64),
                    );
                }
            }
            if let Some(table) = aux.xml_tables.first() {
                if let Some(descriptor) = &table.xml_descriptor {
                    meta.series_metadata.insert(
                        "slidebook7.aux.xml.0.descriptor".into(),
                        crate::common::metadata::MetadataValue::String(descriptor.clone()),
                    );
                }
                if let Some(xml_data) = &table.xml_data {
                    meta.series_metadata.insert(
                        "slidebook7.aux.xml.0.data".into(),
                        crate::common::metadata::MetadataValue::String(xml_data.clone()),
                    );
                }
            }
        }

        if dictionary_entries > 0 {
            meta.series_metadata.insert(
                "slidebook7.compression_dictionary.entries".into(),
                crate::common::metadata::MetadataValue::Int(dictionary_entries as i64),
            );
        }
        Ok(SlideBook7Series {
            meta,
            files,
            plane_len,
            elapsed_times,
            channel_exposures,
            stage_positions,
            interplane_spacing,
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
                    // Promote the decoded typed-record timing/position into OME
                    // planes (Java initFile plane loop: DeltaT per timepoint,
                    // ExposureTime per channel, PositionX/Y from the stage point
                    // and PositionZ offset by the interplane spacing). The plane
                    // index follows this reader's z/c/t order. Java units are
                    // milliseconds (time) and micrometres (position); time is
                    // converted to seconds for the OME plane fields.
                    let has_plane_data = !series.elapsed_times.is_empty()
                        || series.channel_exposures.iter().any(|e| e.is_some())
                        || !series.stage_positions.is_empty();
                    if has_plane_data {
                        if let Some(ome_image) = image.images.get_mut(0) {
                            let size_z = series.meta.size_z.max(1);
                            let size_c = series.meta.size_c.max(1);
                            let image_count = series.meta.image_count.max(1);
                            let stage = series.stage_positions.first().copied();
                            let mut planes = Vec::with_capacity(image_count as usize);
                            for p in 0..image_count {
                                let z = p % size_z;
                                let c = (p / size_z) % size_c;
                                let t = p / (size_z * size_c);
                                let delta_t = series
                                    .elapsed_times
                                    .get(t as usize)
                                    .copied()
                                    .filter(|v| v.is_finite())
                                    .map(|ms| ms / 1000.0);
                                let exposure_time = series
                                    .channel_exposures
                                    .get(c as usize)
                                    .copied()
                                    .flatten()
                                    .map(|ms| ms as f64 / 1000.0);
                                planes.push(crate::common::ome_metadata::OmePlane {
                                    the_z: z,
                                    the_c: c,
                                    the_t: t,
                                    delta_t,
                                    exposure_time,
                                    position_x: stage.map(|(x, _, _)| x),
                                    position_y: stage.map(|(_, y, _)| y),
                                    position_z: stage.map(|(_, _, sz)| {
                                        sz + series.interplane_spacing.unwrap_or(0.0) * z as f64
                                    }),
                                });
                            }
                            ome_image.planes = planes;
                        }
                    }
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
    readers: Vec<crate::formats::tiff_wrappers::NdpiReader>,
    ndpi_files: Vec<PathBuf>,
    /// Per-channel resolved channel name (from NDPI tag 65434), if present.
    channel_names: Vec<Option<String>>,
    /// Java NDPISReader `bandUsed`: selected RGB band for each NDPI companion.
    band_used: Vec<usize>,
    metas: Vec<ImageMetadata>,
    pyramid_series: Vec<bool>,
    current_series: usize,
}

const NDPI_TAG_CHANNEL: u16 = 65434;
const NDPI_TAG_EMISSION_WAVELENGTH: u16 = 65451;
const NDPI_TAG_METADATA: u16 = 65449;

fn ndpis_channel_info(path: &Path) -> (Option<String>, usize) {
    let Ok(file) = File::open(path) else {
        return (None, 0);
    };
    let Ok(mut parser) = crate::tiff::parser::TiffParser::new(file) else {
        return (None, 0);
    };
    let Ok((ifd, _)) = parser.read_ifd(parser.first_ifd_offset) else {
        return (None, 0);
    };
    let mut name = ifd.get_str(NDPI_TAG_CHANNEL).map(str::to_owned);
    let mut band_used = 0usize;
    let wavelength = ifd
        .get(NDPI_TAG_EMISSION_WAVELENGTH)
        .and_then(|v| v.as_vec_f64().first().copied());

    if ifd.samples_per_pixel() >= 3 {
        if let Some(wavelength) = wavelength {
            if wavelength > 380.0 && wavelength <= 490.0 {
                band_used = 2;
            } else if wavelength > 490.0 && wavelength <= 580.0 {
                band_used = 1;
            } else if wavelength > 580.0 && wavelength <= 780.0 {
                band_used = 0;
            }
        }

        if let Some(extra) = ifd.get_str(NDPI_TAG_METADATA) {
            for line in extra.split("\r\n") {
                if !line.trim().starts_with(";NDP Shading Data") {
                    continue;
                }
                for pair in line.split(';') {
                    let Some(eq) = pair.find('=') else { continue };
                    let key = pair[..eq].trim();
                    let value = pair[eq + 1..].trim();
                    if wavelength.is_none() && key.starts_with("Transmittance") && value != "-" {
                        if let Some(ch) = key.chars().last() {
                            if let Some(index) = "RGB".find(ch) {
                                band_used = index;
                            }
                        }
                    }
                    if key == "Name" && !value.is_empty() {
                        name = Some(value.to_string());
                    }
                }
            }
        }
    }

    (name, band_used)
}

impl NdpisReader {
    pub fn new() -> Self {
        NdpisReader {
            readers: Vec::new(),
            ndpi_files: Vec::new(),
            channel_names: Vec::new(),
            band_used: Vec::new(),
            metas: Vec::new(),
            pyramid_series: Vec::new(),
            current_series: 0,
        }
    }

    fn zct_channel_plane(&self, p: u32) -> Result<(usize, u32)> {
        let meta = self
            .metas
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        if p >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(p));
        }
        let size_z = meta.size_z.max(1);
        let size_c = meta.size_c.max(1);
        let z = (p / size_c) % size_z;
        let c = p % size_c;
        let t = p / (size_c * size_z);
        Ok((c as usize, t * size_z + z))
    }

    fn select_ndpi_band(
        &self,
        reader: &crate::formats::tiff_wrappers::NdpiReader,
        channel: usize,
        data: Vec<u8>,
        width: u32,
        height: u32,
    ) -> Vec<u8> {
        let meta = reader.metadata();
        if !meta.is_rgb {
            return data;
        }
        let rgb_channels = meta.size_c.max(1) as usize;
        if rgb_channels <= 1 {
            return data;
        }
        let band = self.band_used.get(channel).copied().unwrap_or(0);
        let band = if band < rgb_channels { band } else { 0 };
        let bytes_per_sample = meta.pixel_type.bytes_per_sample();
        let plane_len = width as usize * height as usize * bytes_per_sample;
        let pixel_count = width as usize * height as usize;
        let expected_interleaved_len = pixel_count
            .saturating_mul(rgb_channels)
            .saturating_mul(bytes_per_sample);
        if expected_interleaved_len <= data.len() {
            let mut out = Vec::with_capacity(plane_len);
            for pixel in 0..pixel_count {
                let start = (pixel * rgb_channels + band) * bytes_per_sample;
                out.extend_from_slice(&data[start..start + bytes_per_sample]);
            }
            return out;
        }
        let start = band.saturating_mul(plane_len);
        let end = start.saturating_add(plane_len);
        if end <= data.len() {
            data[start..end].to_vec()
        } else {
            data
        }
    }

    fn is_pyramid_series(&self) -> bool {
        self.pyramid_series
            .get(self.current_series)
            .copied()
            .unwrap_or(false)
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
        self.close()?;

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
        let mut band_used = Vec::with_capacity(files.len());
        for file in &files {
            let mut r = crate::formats::tiff_wrappers::NdpiReader::new();
            r.set_id(file)?;
            // Channel name and RGB band selection from the first IFD.
            let (name, band) = ndpis_channel_info(file);
            channel_names.push(name);
            band_used.push(band);
            readers.push(r);
        }

        // Build merged metadata from the first reader's series, setting sizeC to
        // the number of channel files and recomputing the plane count.
        let mut metas: Vec<ImageMetadata> = Vec::new();
        let pyramid_height = readers[0].pyramid_height() as usize;
        let series_count = readers[0].series_count();
        for s in 0..series_count {
            readers[0].set_series(s)?;
            metas.push(readers[0].metadata().clone());
        }
        readers[0].set_series(0)?;
        let nchannels = files.len() as u32;
        let mut pyramid_series = vec![false; metas.len()];
        // Java NDPISReader only adjusts channel and image counts for pyramid
        // resolutions. Macro/mask/label extras are read from the first NDPI file.
        for (series_index, m) in metas.iter_mut().enumerate() {
            if series_index < pyramid_height {
                pyramid_series[series_index] = true;
                m.size_c = nchannels;
                m.is_rgb = false;
                m.image_count = m.size_c * m.size_z.max(1) * m.size_t.max(1);
            }
        }

        self.readers = readers;
        self.ndpi_files = files;
        self.channel_names = channel_names;
        self.band_used = band_used;
        self.metas = metas;
        self.pyramid_series = pyramid_series;
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
        self.band_used.clear();
        self.metas.clear();
        self.pyramid_series.clear();
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
        self.metas
            .get(self.current_series)
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }
    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        if self.readers.is_empty() {
            return Err(BioFormatsError::NotInitialized);
        }
        if !self.is_pyramid_series() {
            self.readers[0].set_series(self.current_series)?;
            return self.readers[0].open_bytes(p);
        }
        let (channel, inner_plane) = self.zct_channel_plane(p)?;
        self.readers[channel].set_series(self.current_series)?;
        let (width, height) = {
            let meta = self.readers[channel].metadata();
            (meta.size_x, meta.size_y)
        };
        let data = self.readers[channel].open_bytes(inner_plane)?;
        Ok(self.select_ndpi_band(&self.readers[channel], channel, data, width, height))
    }
    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        if self.readers.is_empty() {
            return Err(BioFormatsError::NotInitialized);
        }
        if !self.is_pyramid_series() {
            self.readers[0].set_series(self.current_series)?;
            return self.readers[0].open_bytes_region(p, x, y, w, h);
        }
        let (channel, inner_plane) = self.zct_channel_plane(p)?;
        self.readers[channel].set_series(self.current_series)?;
        let data = self.readers[channel].open_bytes_region(inner_plane, x, y, w, h)?;
        Ok(self.select_ndpi_band(&self.readers[channel], channel, data, w, h))
    }
    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        if self.readers.is_empty() {
            return Err(BioFormatsError::NotInitialized);
        }
        if !self.is_pyramid_series() {
            self.readers[0].set_series(self.current_series)?;
            return self.readers[0].open_thumb_bytes(p);
        }
        let (channel, inner_plane) = self.zct_channel_plane(p)?;
        self.readers[channel].set_series(self.current_series)?;
        let thumb = self.readers[channel].open_thumb_bytes(inner_plane)?;
        let meta = self.readers[channel].metadata();
        let width = meta.size_x.min(256);
        let height = meta.size_y.min(256);
        Ok(self.select_ndpi_band(&self.readers[channel], channel, thumb, width, height))
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
    meta.is_rgb = size_c > 1;
    meta.is_interleaved = size_c > 1;
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

    if let Some(image) = ome.images.first() {
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
    }

    // The iVision XML is an Apple plist of <key>iplab:Foo</key><value/> pairs,
    // not OME-XML, so the OME image above is usually empty.  Mirror the Java
    // IvisionHandler/initFile, which scrapes the iplab:* acquisition keys and
    // writes them into the MetadataStore (objective, detector, planes, dates).
    let acq = ivision_parse_acquisition_metadata(&xml);
    acq.apply(meta, &mut ome);

    Some(ome)
}

/// iVision acquisition metadata scraped from the trailing plist XML.
///
/// Mirrors the `iplab:*` keys that the Java `IvisionReader.IvisionHandler`
/// recognises and the `initFile` code that pushes them into the MetadataStore.
#[derive(Default)]
struct IvisionAcquisitionMetadata {
    bin_x: Option<String>,
    bin_y: Option<String>,
    creation_date: Option<String>,
    exposure_time: Option<String>,
    gain: Option<String>,
    offset: Option<String>,
    delta_t: Option<String>,
    magnification: Option<f64>,
    lens_na: Option<f64>,
    refractive_index: Option<f64>,
    wavelength: Option<String>,
}

/// Parse the iVision plist XML, extracting the recognised `iplab:*` keys.
///
/// One Java function: `IvisionHandler` (`startElement`/`characters`/`endElement`).
/// The plist serialises each entry as `<key>iplab:Foo</key><string>value</string>`
/// (or `<real>`, `<integer>`, …); we pair each recognised key with the following
/// non-`key` value element, matching the SAX handler's `key`/`value` bookkeeping.
fn ivision_parse_acquisition_metadata(xml: &str) -> IvisionAcquisitionMetadata {
    let mut acq = IvisionAcquisitionMetadata::default();

    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(false);

    let mut current_element: Option<String> = None;
    let mut key: Option<String> = None;
    let mut current_text = String::new();

    loop {
        match reader.read_event() {
            Ok(quick_xml::events::Event::Start(element)) => {
                current_element = Some(ivision_xml_component_name(element.name().as_ref()));
                current_text.clear();
            }
            Ok(quick_xml::events::Event::Empty(_)) => {
                // Self-closing element carries no characters; ignore like the
                // SAX handler, which only acts on character data.
                current_element = None;
                current_text.clear();
            }
            Ok(quick_xml::events::Event::Text(text)) => {
                current_text
                    .push_str(&crate::common::xml::decode_xml_text(&text).unwrap_or_default());
            }
            Ok(quick_xml::events::Event::GeneralRef(reference)) => {
                current_text
                    .push_str(&crate::common::xml::decode_xml_ref(&reference).unwrap_or_default());
            }
            Ok(quick_xml::events::Event::CData(cdata)) => {
                if let Ok(value) = cdata.xml_content(quick_xml::XmlVersion::Implicit1_0) {
                    current_text.push_str(&value);
                }
            }
            Ok(quick_xml::events::Event::End(_)) => {
                let value = current_text.trim();
                if !value.is_empty() {
                    // BaseHandler keys on the *raw* element name ("key"); our
                    // component-name strips the namespace but preserves "key".
                    if current_element.as_deref() == Some("key") {
                        key = Some(value.to_string());
                    } else if let Some(k) = key.take() {
                        ivision_assign_acquisition_field(&mut acq, &k, value);
                    }
                }
                current_element = None;
                current_text.clear();
            }
            Ok(quick_xml::events::Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }

    acq
}

/// Mirror of `IvisionHandler.endElement`: map one recognised `iplab:*` key to a
/// field. Invalid numbers are silently dropped exactly as the Java try/catch.
fn ivision_assign_acquisition_field(acq: &mut IvisionAcquisitionMetadata, key: &str, value: &str) {
    match key {
        "iplab:Bin_X" => acq.bin_x = Some(value.to_string()),
        "iplab:Bin_Y" => acq.bin_y = Some(value.to_string()),
        "iplab:Capture_Date" => acq.creation_date = Some(value.to_string()),
        "iplab:Exposure" => acq.exposure_time = Some(value.to_string()),
        "iplab:Gain" => acq.gain = Some(value.to_string()),
        "iplab:Offset" => acq.offset = Some(value.to_string()),
        "iplab:Interval_T" => acq.delta_t = Some(value.to_string()),
        "iplab:Objective_Mag" => acq.magnification = value.parse::<f64>().ok(),
        "iplab:Objective_NA" => acq.lens_na = value.parse::<f64>().ok(),
        "iplab:Objective_RI" => acq.refractive_index = value.parse::<f64>().ok(),
        "iplab:Wavelength" => acq.wavelength = Some(value.to_string()),
        _ => {}
    }
}

impl IvisionAcquisitionMetadata {
    /// Surface the scraped acquisition fields into `series_metadata` and the OME
    /// store, mirroring `IvisionReader.initFile`'s MetadataStore calls:
    ///   * `Capture_Date` → image acquisition date,
    ///   * `Interval_T` → Pixels TimeIncrement + per-plane DeltaT,
    ///   * `Exposure` → per-plane ExposureTime,
    ///   * `Objective_Mag`/`Objective_NA` → Objective + settings refractive index,
    ///   * `Gain`/`Bin_X`/`Bin_Y` → DetectorSettings gain + binning.
    fn apply(&self, meta: &mut ImageMetadata, ome: &mut crate::common::ome_metadata::OmeMetadata) {
        use crate::common::ome_metadata::{
            create_lsid, OmeChannel, OmeDetector, OmeImage, OmeInstrument, OmeObjective, OmePlane,
        };

        // addGlobalMeta(key, value): the handler records every recognised key.
        let put_str = |meta: &mut ImageMetadata, k: &str, v: &Option<String>| {
            if let Some(v) = v.as_ref().filter(|s| !s.trim().is_empty()) {
                meta.series_metadata
                    .insert(k.to_string(), MetadataValue::String(v.clone()));
            }
        };
        put_str(meta, "iplab:Bin_X", &self.bin_x);
        put_str(meta, "iplab:Bin_Y", &self.bin_y);
        put_str(meta, "iplab:Capture_Date", &self.creation_date);
        put_str(meta, "iplab:Exposure", &self.exposure_time);
        put_str(meta, "iplab:Gain", &self.gain);
        put_str(meta, "iplab:Offset", &self.offset);
        put_str(meta, "iplab:Interval_T", &self.delta_t);
        put_str(meta, "iplab:Wavelength", &self.wavelength);
        if let Some(v) = self.magnification {
            meta.series_metadata
                .insert("iplab:Objective_Mag".into(), MetadataValue::Float(v));
        }
        if let Some(v) = self.lens_na {
            meta.series_metadata
                .insert("iplab:Objective_NA".into(), MetadataValue::Float(v));
        }
        if let Some(v) = self.refractive_index {
            meta.series_metadata
                .insert("iplab:Objective_RI".into(), MetadataValue::Float(v));
        }

        // Ensure there is exactly one OME image to attach acquisition data to.
        if ome.images.is_empty() {
            ome.images.push(OmeImage::default());
        }

        // creationDate → store.setImageAcquisitionDate(...).
        if let Some(date) = self.creation_date.as_ref().filter(|s| !s.trim().is_empty()) {
            let date = date.trim().to_string();
            ome.images[0].acquisition_date = Some(date.clone());
            meta.series_metadata
                .insert("acquisition_date".into(), MetadataValue::String(date));
        }

        // deltaT → store.setPixelsTimeIncrement(...).
        let delta_t = self
            .delta_t
            .as_ref()
            .and_then(|v| v.trim().parse::<f64>().ok())
            .filter(|v| v.is_finite());
        if let Some(increment) = delta_t {
            ome.images[0].time_increment = Some(increment);
        }

        // exposureTime → per-plane ExposureTime (seconds).
        let exposure = self
            .exposure_time
            .as_ref()
            .and_then(|v| v.trim().parse::<f64>().ok())
            .filter(|v| v.is_finite());

        // Build one OmePlane per image plane so DeltaT/ExposureTime have a home.
        if (delta_t.is_some() || exposure.is_some()) && ome.images[0].planes.is_empty() {
            let plane_count = meta.image_count.max(1);
            for index in 0..plane_count {
                let the_z = if meta.size_z > 0 {
                    index % meta.size_z
                } else {
                    0
                };
                let the_t = if meta.size_z > 0 {
                    index / meta.size_z
                } else {
                    0
                };
                ome.images[0].planes.push(OmePlane {
                    the_z,
                    the_c: 0,
                    the_t,
                    delta_t,
                    exposure_time: exposure,
                    ..OmePlane::default()
                });
            }
        }

        // Objective (lensNA / magnification) and Detector (gain / binning) live
        // on a single Instrument, referenced by the image.
        let mut instrument = OmeInstrument {
            id: Some(create_lsid("Instrument", &[0])),
            ..Default::default()
        };

        let has_objective = self.lens_na.is_some() || self.magnification.is_some();
        if has_objective || self.refractive_index.is_some() {
            instrument.objectives.push(OmeObjective {
                id: Some(create_lsid("Objective", &[0, 0])),
                correction: Some("Other".into()),
                immersion: Some("Other".into()),
                lens_na: self.lens_na,
                nominal_magnification: self.magnification,
                ..Default::default()
            });
            ome.images[0].objective_ref = Some(0);
        }

        // binX/binY → DetectorSettings binning string "<x>x<y>".
        let binning = match (self.bin_x.as_ref(), self.bin_y.as_ref()) {
            (Some(x), Some(y)) => Some(format!("{}x{}", x.trim(), y.trim())),
            _ => None,
        };
        let gain = self
            .gain
            .as_ref()
            .and_then(|v| v.trim().parse::<f64>().ok())
            .filter(|v| v.is_finite());

        instrument.detectors.push(OmeDetector {
            id: Some(create_lsid("Detector", &[0, 0])),
            detector_type: Some("Other".into()),
            gain,
            ..Default::default()
        });

        // DetectorSettings (gain + binning) hang off the first channel.
        if gain.is_some() || binning.is_some() {
            if ome.images[0].channels.is_empty() {
                ome.images[0].channels.push(OmeChannel {
                    samples_per_pixel: meta.size_c.max(1),
                    ..OmeChannel::default()
                });
            }
            let channel = &mut ome.images[0].channels[0];
            channel.detector_settings_gain = gain;
            channel.detector_settings_binning = binning;
            channel.detector_ref = Some(create_lsid("Detector", &[0, 0]));
        }

        let instrument_index = ome.instruments.len();
        ome.instruments.push(instrument);
        ome.images[0].instrument_ref = Some(instrument_index);

        // refractiveIndex → ObjectiveSettings RefractiveIndex; OmeMetadata has no
        // dedicated field, so surface it as series metadata only (Java keeps it on
        // the store, which we also mirror via the iplab:Objective_RI key above).
        if let Some(wavelength) = self.wavelength.as_ref().filter(|s| !s.trim().is_empty()) {
            meta.series_metadata.insert(
                "iVision XML Wavelength".into(),
                MetadataValue::String(wavelength.clone()),
            );
        }
    }
}

fn ivision_flatten_xml_metadata(xml: &str, meta: &mut ImageMetadata) -> usize {
    const MAX_FIELDS: usize = 128;
    const MAX_VALUE_LEN: usize = 512;

    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(false);
    let mut stack: Vec<String> = Vec::new();
    let mut current_text = String::new();
    let mut inserted = 0usize;

    loop {
        match reader.read_event() {
            Ok(quick_xml::events::Event::Start(element)) => {
                ivision_flush_xml_text(
                    &stack,
                    &mut current_text,
                    meta,
                    &mut inserted,
                    MAX_FIELDS,
                    MAX_VALUE_LEN,
                );
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
                ivision_flush_xml_text(
                    &stack,
                    &mut current_text,
                    meta,
                    &mut inserted,
                    MAX_FIELDS,
                    MAX_VALUE_LEN,
                );
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
                if let Some(value) = crate::common::xml::decode_xml_text(&text) {
                    current_text.push_str(&value);
                }
            }
            Ok(quick_xml::events::Event::GeneralRef(reference)) => {
                if let Some(value) = crate::common::xml::decode_xml_ref(&reference) {
                    current_text.push_str(&value);
                }
            }
            Ok(quick_xml::events::Event::CData(cdata)) => {
                if let Ok(value) = cdata.xml_content(quick_xml::XmlVersion::Implicit1_0) {
                    current_text.push_str(&value);
                }
            }
            Ok(quick_xml::events::Event::End(_)) => {
                ivision_flush_xml_text(
                    &stack,
                    &mut current_text,
                    meta,
                    &mut inserted,
                    MAX_FIELDS,
                    MAX_VALUE_LEN,
                );
                stack.pop();
            }
            Ok(quick_xml::events::Event::Eof) => {
                ivision_flush_xml_text(
                    &stack,
                    &mut current_text,
                    meta,
                    &mut inserted,
                    MAX_FIELDS,
                    MAX_VALUE_LEN,
                );
                break;
            }
            Err(_) => break,
            _ => {}
        }
        if inserted >= MAX_FIELDS {
            break;
        }
    }

    inserted
}

fn ivision_flush_xml_text(
    stack: &[String],
    current_text: &mut String,
    meta: &mut ImageMetadata,
    inserted: &mut usize,
    max_fields: usize,
    max_value_len: usize,
) {
    if *inserted >= max_fields || current_text.trim().is_empty() {
        current_text.clear();
        return;
    }
    let value = current_text.trim();
    if value.len() <= max_value_len {
        let key = ivision_flatten_xml_key(stack, None);
        ivision_insert_flattened_xml_value(meta, key, value);
        *inserted += 1;
    }
    current_text.clear();
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
        let Ok(value) =
            attr.decoded_and_normalized_value(quick_xml::XmlVersion::Implicit1_0, reader.decoder())
        else {
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
pub struct AfiReader {
    readers: Vec<crate::formats::svs::SvsReader>,
    channel_names: Vec<Option<String>>,
    metas: Vec<ImageMetadata>,
    current_series: usize,
}

impl AfiReader {
    pub fn new() -> Self {
        AfiReader {
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

    fn open_assembled_plane(
        &mut self,
        p: u32,
        region: Option<(u32, u32, u32, u32)>,
    ) -> Result<Vec<u8>> {
        let extra = 2usize.min(self.metas.len());
        if self.current_series + extra >= self.metas.len() {
            self.readers[0].set_series(self.current_series)?;
            return match region {
                Some((x, y, w, h)) => self.readers[0].open_bytes_region(p, x, y, w, h),
                None => self.readers[0].open_bytes(p),
            };
        }

        let nchannels = self.readers.len() as u32;
        let channel = (p % nchannels.max(1)) as usize;
        let inner_plane = p / nchannels.max(1);
        let dest_type = self.metas[self.current_series].pixel_type;
        let dest_bytes = dest_type.bytes_per_sample();
        let little_endian = self.metas[self.current_series].is_little_endian;

        self.readers[channel].set_series(self.current_series)?;
        let src_type = self.readers[channel].metadata().pixel_type;
        let src_bytes = src_type.bytes_per_sample();
        let tmp = match region {
            Some((x, y, w, h)) => {
                self.readers[channel].open_bytes_region(inner_plane, x, y, w, h)?
            }
            None => self.readers[channel].open_bytes(inner_plane)?,
        };

        match dest_bytes.cmp(&src_bytes) {
            std::cmp::Ordering::Equal => Ok(tmp),
            std::cmp::Ordering::Less => Err(BioFormatsError::UnsupportedFormat(
                "AFI downsampling images is not supported".into(),
            )),
            std::cmp::Ordering::Greater => {
                afi_widen_plane_bytes(&tmp, src_bytes, dest_bytes, little_endian)
            }
        }
    }

    fn is_extra_series(&self) -> bool {
        let extra = 2usize.min(self.metas.len());
        self.current_series + extra >= self.metas.len()
    }
}

fn afi_widen_plane_bytes(
    src: &[u8],
    src_bytes: usize,
    dest_bytes: usize,
    little_endian: bool,
) -> Result<Vec<u8>> {
    if src_bytes == 0 || dest_bytes <= src_bytes || dest_bytes > 8 {
        return Err(BioFormatsError::Format(
            "AFI invalid byte-depth conversion".into(),
        ));
    }
    if src.len() % src_bytes != 0 {
        return Err(BioFormatsError::InvalidData(
            "AFI source plane byte count is not sample-aligned".into(),
        ));
    }

    let max = if dest_bytes == 8 {
        u64::MAX
    } else {
        (1u64 << (dest_bytes * 8)) - 1
    };
    let mut out = vec![0u8; (src.len() / src_bytes) * dest_bytes];
    for (i, sample) in src.chunks_exact(src_bytes).enumerate() {
        let mut value = 0u64;
        if little_endian {
            for (shift, byte) in sample.iter().enumerate() {
                value |= (*byte as u64) << (shift * 8);
            }
        } else {
            for byte in sample {
                value = (value << 8) | (*byte as u64);
            }
        }

        // Java copies the source sample into a wider destination sample, reads
        // it as the destination type, then scales value / 255 to the full
        // destination range. Preserve that formula, including its 8-bit divisor.
        let scaled = ((value as f64 / 255.0) * max as f64) as u64;
        let dest = &mut out[i * dest_bytes..(i + 1) * dest_bytes];
        if little_endian {
            let bytes = scaled.to_le_bytes();
            dest.copy_from_slice(&bytes[..dest_bytes]);
        } else {
            let bytes = scaled.to_be_bytes();
            dest.copy_from_slice(&bytes[8 - dest_bytes..]);
        }
    }
    Ok(out)
}

impl Default for AfiReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for AfiReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("afi"))
    }
    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.len() >= 4
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
            let mut r = crate::formats::svs::SvsReader::new();
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
        let pyramid_pixel_type = metas.first().map(|m| m.pixel_type);
        for (i, m) in metas.iter_mut().enumerate() {
            if i + extra < total {
                m.size_c = nchannels;
                m.is_rgb = false;
                m.image_count = m.size_c * m.size_z.max(1) * m.size_t.max(1);
                if i > 0 {
                    if let Some(pixel_type) = pyramid_pixel_type {
                        m.pixel_type = pixel_type;
                        m.bits_per_pixel = (pixel_type.bytes_per_sample() * 8) as u8;
                    }
                }
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
        self.open_assembled_plane(p, None)
    }
    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        self.open_assembled_plane(p, Some((x, y, w, h)))
    }
    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        if self.is_extra_series() {
            self.readers[0].set_series(self.current_series)?;
            return self.readers[0].open_thumb_bytes(p);
        }

        // Java switches to the last pyramid series and calls back through this
        // reader, so the thumbnail is assembled through AFI channel logic rather
        // than delegated to the first SVS file.
        let saved = self.current_series;
        let target = self.metas.len().saturating_sub(2).saturating_sub(1);
        self.current_series = target;
        let meta = self.metadata().clone();
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        let result = self.open_bytes_region(p, tx, ty, tw, th);
        self.current_series = saved;
        result
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
/// We port the comment parsing, dimension assignment (`sizeC` = number of IFDs),
/// and the per-IFD strip→Z-plane reshape that the Java reader performs before
/// reading pixel planes.
pub struct ImarisTiffReader {
    inner: crate::tiff::TiffReader,
    path: Option<PathBuf>,
    /// Reshaped plane table: one entry per (channel-IFD, strip) pair, in the
    /// same order Java builds `tmp` (outer loop over IFDs, inner over strips).
    /// `None` when the file does not look like a tiled-strip Imaris stack and the
    /// reader falls back to plain `TiffReader` plane semantics.
    planes: Option<Vec<ImarisPlane>>,
}

/// One reshaped Imaris plane: a single strip of one channel IFD, decoded as a
/// full sizeX×sizeY image (Java moves each strip into its own one-strip IFD via
/// `TILE_OFFSETS`/`TILE_BYTE_COUNTS`).
#[derive(Clone)]
struct ImarisPlane {
    offset: u64,
    byte_count: usize,
    compression: Compression,
    predictor: u16,
    samples_per_pixel: u16,
    bits_per_sample: u16,
    width: u32,
    /// Rows covered by this strip (RowsPerStrip, clamped to the remaining rows).
    rows: u32,
}

impl ImarisTiffReader {
    pub fn new() -> Self {
        ImarisTiffReader {
            inner: crate::tiff::TiffReader::new(),
            path: None,
            planes: None,
        }
    }

    /// Port of `ImarisTiffReader.initFile`'s IFD-reshape + core-metadata block.
    ///
    /// Imaris TIFFs store a thumbnail in the first IFD (already dropped by the
    /// minimal-TIFF init) and then one IFD per channel, each IFD being a stack of
    /// strips where every strip is a full Z plane. Java explodes the IFD list so
    /// that each strip becomes its own one-strip IFD (`tmp`), then sets
    /// `sizeC = #originalIFDs`, `sizeZ = tmp.size()/sizeC`, `sizeT = 1`.
    fn reshape_ifds(&mut self) {
        let ifd_count = self.inner.ifd_count();
        if ifd_count == 0 {
            return;
        }
        // Build `tmp`: outer loop over channel IFDs, inner over their strips.
        let mut planes: Vec<ImarisPlane> = Vec::new();
        let mut size_x = 0u32;
        let mut size_y = 0u32;
        let mut ok = true;

        for i in 0..ifd_count {
            let Some(ifd) = self.inner.ifd(i) else {
                ok = false;
                break;
            };
            // Java IFD.getStripByteCounts/getStripOffsets: strips, not tiles.
            let offsets = ifd.get_vec_u64(tag::STRIP_OFFSETS);
            let byte_counts = ifd.get_vec_u64(tag::STRIP_BYTE_COUNTS);
            if offsets.is_empty() || offsets.len() != byte_counts.len() {
                ok = false;
                break;
            }
            let width = ifd.image_width().unwrap_or(0);
            let length = ifd.image_length().unwrap_or(0);
            let bps_vec = ifd.bits_per_sample();
            let bps = *bps_vec.first().unwrap_or(&8);
            let spp = ifd.samples_per_pixel();
            let compression = ifd.compression();
            let predictor = ifd.predictor();
            // RowsPerStrip (default = full image height = single strip).
            let rows_per_strip = ifd.get_u32(tag::ROWS_PER_STRIP).unwrap_or(length).max(1);

            if i == 0 {
                size_x = width;
                size_y = length;
            }

            for (s, (&offset, &byte_count)) in offsets.iter().zip(byte_counts.iter()).enumerate() {
                let row_start = (s as u32).saturating_mul(rows_per_strip);
                if row_start >= length && length != 0 {
                    break;
                }
                let rows = if length == 0 {
                    rows_per_strip
                } else {
                    rows_per_strip.min(length - row_start)
                };
                planes.push(ImarisPlane {
                    offset,
                    byte_count: byte_count as usize,
                    compression,
                    predictor,
                    samples_per_pixel: spp,
                    bits_per_sample: bps,
                    width,
                    rows,
                });
            }
        }

        if !ok || planes.is_empty() {
            return;
        }

        let size_c = ifd_count as u32;
        let total = planes.len() as u32;
        // Java: sizeZ = tmp.size() / sizeC. If strips do not divide evenly across
        // channels the file is not a clean Imaris stack; fall back to TiffReader.
        if size_c == 0 || total % size_c != 0 {
            return;
        }
        let size_z = total / size_c;

        if let Some(s) = self.inner.series_list_mut().first_mut() {
            let m = &mut s.metadata;
            m.size_c = size_c;
            m.size_z = size_z;
            m.size_t = 1;
            if size_x > 0 {
                m.size_x = size_x;
            }
            if size_y > 0 {
                m.size_y = size_y;
            }
            m.image_count = size_c * size_z;
            m.dimension_order = crate::common::metadata::DimensionOrder::XYZCT;
            m.is_interleaved = false;
            // Java: rgb = imageCount != sizeZ*sizeC*sizeT, which is false here.
            m.is_rgb = false;
        }

        self.planes = Some(planes);
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
                "Name"
                    if !self
                        .path
                        .as_ref()
                        .and_then(|path| path.file_name())
                        .and_then(|name| name.to_str())
                        .map(|name| name.ends_with(value))
                        .unwrap_or(false) =>
                {
                    channel_names.push(value.to_string())
                }
                "RecordingDate" => {
                    let v = value.replace(' ', "T");
                    let trimmed = v.split('.').next().unwrap_or(&v).to_string();
                    creation_date = Some(trimmed);
                }
                _ => {}
            }
        }

        let ifd_count = self.inner.ifd_count() as u32;
        let reshaped = self.planes.is_some();
        if let Some(s) = self.inner.series_list_mut().first_mut() {
            // sizeC equals the number of IFDs (channels), per Java. When the IFDs
            // were already reshaped into per-strip planes, sizeC/sizeZ are set by
            // `reshape_ifds`; only patch them here for the non-reshaped fallback.
            if ifd_count > 0 && !reshaped {
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

    /// Read one reshaped plane (a single strip of one channel IFD) and decode it
    /// into a full sizeX×sizeY image. Mirrors how Java reads each per-strip IFD:
    /// the strip's compressed bytes are decompressed with the IFD's compression,
    /// predictor and sample layout to yield exactly one XY plane.
    fn open_reshaped_plane(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let plane = self
            .planes
            .as_ref()
            .and_then(|planes| planes.get(plane_index as usize))
            .cloned()
            .ok_or(BioFormatsError::PlaneOutOfRange(plane_index))?;
        let path = self.path.clone().ok_or(BioFormatsError::NotInitialized)?;
        let little_endian = self.inner.is_little_endian();

        let mut reader = BufReader::new(File::open(&path).map_err(BioFormatsError::Io)?);
        let raw = read_bytes_at(&mut reader, plane.offset, plane.byte_count)?;

        let bytes_per_sample = (plane.bits_per_sample.div_ceil(8)).max(1) as usize;
        let expected_len = (plane.width as usize)
            .saturating_mul(plane.rows as usize)
            .saturating_mul(plane.samples_per_pixel as usize)
            .saturating_mul(bytes_per_sample);

        // Imaris 3 TIFF strips use the baseline TIFF compressions. JPEG-compressed
        // Imaris stacks are not known to occur; reject them clearly rather than
        // mis-decode.
        let mut out = match plane.compression {
            Compression::None => raw,
            Compression::Lzw => decompress_lzw(&raw)?,
            Compression::Deflate | Compression::DeflateOld => decompress_deflate(&raw)?,
            Compression::PackBits => decompress_packbits(&raw)?,
            other => {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "Imaris TIFF strip compression {other:?} is not supported"
                )));
            }
        };

        // Predictor (TIFF tag 317): 1 = none, 2 = horizontal differencing.
        match plane.predictor {
            1 => {}
            2 => imaris_undo_horizontal_differencing(
                &mut out,
                plane.width as usize,
                plane.samples_per_pixel as usize,
                plane.bits_per_sample,
                little_endian,
            )?,
            other => {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "Imaris TIFF predictor {other} is not supported"
                )));
            }
        }

        if out.len() > expected_len {
            out.truncate(expected_len);
        }
        Ok(out)
    }
}

/// TIFF horizontal differencing predictor (tag 317 == 2), replicated locally for
/// the Imaris reshape path. Mirrors `TiffCompression.undifference`: each sample is
/// a `bytes_per_sample`-wide integer (honouring endianness); the same-channel
/// sample one pixel to the left is added with wrapping overflow, per row.
fn imaris_undo_horizontal_differencing(
    data: &mut [u8],
    row_width: usize,
    samples_per_pixel: usize,
    bits_per_sample: u16,
    little_endian: bool,
) -> Result<()> {
    if row_width == 0 || samples_per_pixel == 0 {
        return Ok(());
    }
    let bytes_per_sample = match bits_per_sample {
        8 => 1usize,
        16 => 2usize,
        32 => 4usize,
        64 => 8usize,
        other => {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "Imaris TIFF horizontal predictor for {other}-bit samples not supported"
            )));
        }
    };
    let channel_stride = samples_per_pixel;
    let row_stride_bytes = row_width * samples_per_pixel * bytes_per_sample;
    if row_stride_bytes == 0 {
        return Ok(());
    }
    for row in data.chunks_mut(row_stride_bytes) {
        let sample_count = row.len() / bytes_per_sample;
        let usable = sample_count / samples_per_pixel * samples_per_pixel;
        for i in channel_stride..usable {
            let cur = i * bytes_per_sample;
            let prev = (i - channel_stride) * bytes_per_sample;
            imaris_add_sample(row, cur, prev, bytes_per_sample, little_endian);
        }
    }
    Ok(())
}

/// Adds the `bytes_per_sample`-wide integers at `cur` and `prev` with wrapping
/// overflow and stores the result at `cur`, honouring the IFD byte order.
fn imaris_add_sample(
    row: &mut [u8],
    cur: usize,
    prev: usize,
    bytes_per_sample: usize,
    little_endian: bool,
) {
    let read = |off: usize| -> u64 {
        let mut buf = [0u8; 8];
        if little_endian {
            buf[..bytes_per_sample].copy_from_slice(&row[off..off + bytes_per_sample]);
            u64::from_le_bytes(buf)
        } else {
            buf[8 - bytes_per_sample..].copy_from_slice(&row[off..off + bytes_per_sample]);
            u64::from_be_bytes(buf)
        }
    };
    let value = read(cur).wrapping_add(read(prev));
    let bytes = if little_endian {
        value.to_le_bytes()
    } else {
        value.to_be_bytes()
    };
    if little_endian {
        row[cur..cur + bytes_per_sample].copy_from_slice(&bytes[..bytes_per_sample]);
    } else {
        row[cur..cur + bytes_per_sample].copy_from_slice(&bytes[8 - bytes_per_sample..]);
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
    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.len() >= 4
            && ((header[0..2] == [0x49, 0x49] && header[2..4] == [42, 0])
                || (header[0..2] == [0x4d, 0x4d] && header[2..4] == [0, 42]))
    }
    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.inner.set_id(path)?;
        self.path = Some(path.to_path_buf());
        self.reshape_ifds();
        self.enrich_metadata();
        Ok(())
    }
    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.planes = None;
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
        if self.planes.is_some() {
            return self.open_reshaped_plane(p);
        }
        self.inner.open_bytes(p)
    }
    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        if self.planes.is_some() {
            let full = self.open_reshaped_plane(p)?;
            let meta = self.inner.metadata();
            let ch = if meta.is_rgb { meta.size_c as usize } else { 1 };
            return crop_full_plane("Imaris TIFF", &full, meta, ch, x, y, w, h);
        }
        self.inner.open_bytes_region(p, x, y, w, h)
    }
    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        if self.planes.is_some() {
            let meta = self.inner.metadata();
            let tw = meta.size_x.min(256);
            let th = meta.size_y.min(256);
            let tx = (meta.size_x - tw) / 2;
            let ty = (meta.size_y - th) / 2;
            return self.open_bytes_region(p, tx, ty, tw, th);
        }
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
// 7b. Imaris (classic native RAW) — Bitplane Imaris
// ---------------------------------------------------------------------------
/// Read `n` raw bytes and decode them as a string, mirroring Java
/// `RandomAccessInputStream.readString(int)`. Imaris pads fixed-width string
/// fields with NULs; we trim the trailing NULs so the logical value matches the
/// Java reader's interned strings.
fn imaris_read_string<R: Read>(r: &mut R, n: usize) -> Result<String> {
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
    Ok(crate::common::io::read_cstring(&buf))
}

/// Skip `n` bytes, mirroring Java `RandomAccessInputStream.skipBytes(int)`.
fn imaris_skip_bytes<R: Read>(r: &mut R, n: usize) -> Result<()> {
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
    Ok(())
}

/// Bitplane Imaris classic RAW format reader (`.ims`).
///
/// Faithful port of the Java `ImarisReader` (loci.formats.in.ImarisReader).
/// This is the *original* native binary Imaris format, identified by the magic
/// integer `5021964` (big-endian) at the start of the file. It is distinct from
/// the HDF5-based Imaris (`ImarisHdfReader`) and the Imaris 3 TIFF variant
/// (`ImarisTiffReader`), both of which also use the `.ims` extension; the magic
/// number disambiguates them.
///
/// The header carries width/height/depth/channel-count, an image name, a
/// comment/description, physical pixel sizes, and per-channel comment/gain/
/// offset/pinhole records. Pixel data is raw `UINT8`, big-endian (trivially so
/// for 8-bit), stored channel-major then Z, and each plane is written Y-flipped
/// (Java reads rows bottom-to-top in `openBytes`).
pub struct ImarisReader {
    path: Option<PathBuf>,
    meta: ImageMetadata,
    /// Offsets (in bytes from the start of the file) to each image plane,
    /// indexed by plane number `no` in `XYZCT` order (mirrors Java `offsets`).
    offsets: Vec<u64>,
    /// Image name (Java `imageName`).
    image_name: String,
    /// Image comment/description (Java `description`).
    description: String,
    /// Physical pixel sizes in micrometres (Java `dx`, `dy`, `dz`).
    physical_size_x: f32,
    physical_size_y: f32,
    physical_size_z: f32,
    /// Per-channel pinhole sizes (µm), Java `pinholes`.
    pinholes: Vec<f32>,
    /// Per-channel detector gains, Java `gains`.
    gains: Vec<f32>,
}

impl ImarisReader {
    /// Magic number present in all classic Imaris files (Java
    /// `IMARIS_MAGIC_BYTES`). Read big-endian.
    const IMARIS_MAGIC_BYTES: i32 = 5021964;
    /// Endianness of the format (Java `IS_LITTLE = false`).
    const IS_LITTLE: bool = false;

    pub fn new() -> Self {
        ImarisReader {
            path: None,
            meta: ImageMetadata::default(),
            offsets: Vec::new(),
            image_name: String::new(),
            description: String::new(),
            physical_size_x: 0.0,
            physical_size_y: 0.0,
            physical_size_z: 0.0,
            pinholes: Vec::new(),
            gains: Vec::new(),
        }
    }

    /// Port of Java `ImarisReader.initFile`.
    ///
    /// Parses the fixed-layout classic-Imaris header, computes per-plane file
    /// offsets, and populates core + global metadata.
    fn init_file(&mut self, path: &Path) -> Result<()> {
        let mut f = BufReader::new(File::open(path).map_err(BioFormatsError::Io)?);

        use crate::common::endian::{read_f32, read_i16, read_i32};
        const LE: bool = ImarisReader::IS_LITTLE;

        // Verify magic (big-endian int).
        let magic = read_i32(&mut f, LE)?;
        if magic != Self::IMARIS_MAGIC_BYTES {
            return Err(BioFormatsError::UnsupportedFormat(
                "Imaris magic number not found.".into(),
            ));
        }

        // Header.
        let version = read_i32(&mut f, LE)?;
        imaris_skip_bytes(&mut f, 4)?;

        let image_name = imaris_read_string(&mut f, 128)?;

        let size_x = read_i16(&mut f, LE)? as u32;
        let size_y = read_i16(&mut f, LE)? as u32;
        let size_z = read_i16(&mut f, LE)? as u32;

        imaris_skip_bytes(&mut f, 2)?;

        let size_c = read_i32(&mut f, LE)? as u32;
        imaris_skip_bytes(&mut f, 2)?;

        let date = imaris_read_string(&mut f, 32)?;

        let dx = read_f32(&mut f, LE)?;
        let dy = read_f32(&mut f, LE)?;
        let dz = read_f32(&mut f, LE)?;
        let _mag = read_i16(&mut f, LE)?;

        let description = imaris_read_string(&mut f, 128)?;
        let is_survey = read_i32(&mut f, LE)?;

        // Calculating image offsets.
        let image_count = size_z.saturating_mul(size_c);
        let mut offsets = vec![0u64; image_count as usize];

        let mut gains = vec![0f32; size_c as usize];
        let mut detector_offsets = vec![0f32; size_c as usize];
        let mut pinholes = vec![0f32; size_c as usize];

        // Per-channel comment/gain/offset/pinhole records (164 bytes each).
        // Java guards this with MetadataLevel != MINIMUM; we always parse so the
        // file pointer position matches Java's non-minimal path (offsets below
        // assume these records were consumed).
        for i in 0..size_c as usize {
            let comment = imaris_read_string(&mut f, 128)?;
            self.meta.series_metadata.insert(
                format!("Channel #{i} Comment"),
                MetadataValue::String(comment),
            );
            gains[i] = read_f32(&mut f, LE)?;
            detector_offsets[i] = read_f32(&mut f, LE)?;
            pinholes[i] = read_f32(&mut f, LE)?;
            imaris_skip_bytes(&mut f, 24)?;
        }

        // Per-plane offsets (Java offset block).
        let mut offset = 336u64 + 164u64 * size_c as u64;
        for i in 0..size_c as u64 {
            for j in 0..size_z as u64 {
                offsets[(i * size_z as u64 + j) as usize] =
                    offset + j * size_x as u64 * size_y as u64;
            }
            offset += size_x as u64 * size_y as u64 * size_z as u64;
        }

        // Global metadata.
        self.meta
            .series_metadata
            .insert("Version".into(), MetadataValue::Int(version as i64));
        self.meta.series_metadata.insert(
            "Image name".into(),
            MetadataValue::String(image_name.clone()),
        );
        self.meta.series_metadata.insert(
            "Image comment".into(),
            MetadataValue::String(description.clone()),
        );
        self.meta.series_metadata.insert(
            "Survey performed".into(),
            MetadataValue::Bool(is_survey == 0),
        );
        self.meta
            .series_metadata
            .insert("Original date".into(), MetadataValue::String(date));

        // Populating metadata (core).
        let size_t = if size_c * size_z == 0 {
            1
        } else {
            image_count / (size_c * size_z)
        };
        self.meta.size_x = size_x;
        self.meta.size_y = size_y;
        self.meta.size_z = size_z;
        self.meta.size_c = size_c;
        self.meta.size_t = size_t;
        self.meta.image_count = image_count;
        self.meta.dimension_order = crate::common::metadata::DimensionOrder::XYZCT;
        self.meta.is_rgb = false;
        self.meta.is_interleaved = false;
        self.meta.is_little_endian = Self::IS_LITTLE;
        self.meta.is_indexed = false;
        self.meta.pixel_type = PixelType::Uint8;
        self.meta.bits_per_pixel = 8;

        self.offsets = offsets;
        self.image_name = image_name;
        self.description = description;
        self.physical_size_x = dx;
        self.physical_size_y = dy;
        self.physical_size_z = dz;
        self.pinholes = pinholes;
        self.gains = gains;

        Ok(())
    }

    /// Port of Java `ImarisReader.openBytes`.
    ///
    /// Reads one raw UINT8 plane, flipping it vertically (Java reads file rows
    /// bottom-to-top into the output buffer top-to-bottom).
    fn open_bytes_impl(&self, no: u32) -> Result<Vec<u8>> {
        let size_x = self.meta.size_x as usize;
        let size_y = self.meta.size_y as usize;
        let no = no as usize;
        if no >= self.offsets.len() {
            return Err(BioFormatsError::PlaneOutOfRange(no as u32));
        }

        let mut f = BufReader::new(
            File::open(self.path.as_ref().ok_or_else(|| {
                BioFormatsError::UnsupportedFormat("Imaris reader not initialised".into())
            })?)
            .map_err(BioFormatsError::Io)?,
        );

        // Java: x=0, y=0, w=sizeX, h=sizeY for a full-plane read.
        // in.seek(offsets[no] + sizeX * (sizeY - y - h)); with y=0,h=sizeY -> 0.
        let base = self.offsets[no];
        let mut buf = vec![0u8; size_x * size_y];
        // Read rows bottom-to-top: row index counts down, output row counts down
        // too, but the source is read sequentially from `base`, which flips Y.
        for row in (0..size_y).rev() {
            let src = read_bytes_at(&mut f, base + ((size_y - 1 - row) * size_x) as u64, size_x)?;
            buf[row * size_x..(row + 1) * size_x].copy_from_slice(&src);
        }
        Ok(buf)
    }
}

impl Default for ImarisReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for ImarisReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("ims"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // Java isThisType: validStream(4) then readInt() == magic (big-endian).
        if header.len() < 4 {
            return false;
        }
        let magic = i32::from_be_bytes([header[0], header[1], header[2], header[3]]);
        magic == Self::IMARIS_MAGIC_BYTES
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.path = Some(path.to_path_buf());
        self.init_file(path)
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.offsets = Vec::new();
        Ok(())
    }

    fn series_count(&self) -> usize {
        1
    }

    fn set_series(&mut self, series: usize) -> Result<()> {
        if series == 0 {
            Ok(())
        } else {
            Err(BioFormatsError::SeriesOutOfRange(series))
        }
    }

    fn series(&self) -> usize {
        0
    }

    fn metadata(&self) -> &ImageMetadata {
        &self.meta
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        self.open_bytes_impl(plane_index)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let full = self.open_bytes_impl(plane_index)?;
        crop_full_plane("Imaris", &full, &self.meta, 1, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let tw = self.meta.size_x.min(256);
        let th = self.meta.size_y.min(256);
        let tx = (self.meta.size_x - tw) / 2;
        let ty = (self.meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        use crate::common::ome_metadata::{OmeChannel, OmeImage, OmeMetadata};
        if self.path.is_none() {
            return None;
        }
        let mut image = OmeImage {
            name: Some(self.image_name.clone()),
            description: Some(self.description.clone()),
            time_increment: Some(1.0),
            ..OmeImage::default()
        };
        // FormatTools.getPhysicalSize* returns null for non-positive values.
        if self.physical_size_x > 0.0 {
            image.physical_size_x = Some(self.physical_size_x as f64);
        }
        if self.physical_size_y > 0.0 {
            image.physical_size_y = Some(self.physical_size_y as f64);
        }
        if self.physical_size_z > 0.0 {
            image.physical_size_z = Some(self.physical_size_z as f64);
        }
        for i in 0..self.meta.size_c as usize {
            let mut ch = OmeChannel {
                samples_per_pixel: 1,
                ..OmeChannel::default()
            };
            if self.pinholes.get(i).copied().unwrap_or(0.0) > 0.0 {
                ch.pinhole_size = Some(self.pinholes[i] as f64);
            }
            if self.gains.get(i).copied().unwrap_or(0.0) > 0.0 {
                ch.detector_settings_gain = Some(self.gains[i] as f64);
            }
            image.channels.push(ch);
        }
        Some(OmeMetadata {
            images: vec![image],
            ..OmeMetadata::default()
        })
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
    multi_images: Vec<XlefMultiImage>,
    lms_metadata: Vec<ImageMetadata>,
    lms_pixels: Vec<XlefLmsPixelLeaf>,
    series_map: Vec<XlefSeriesRef>,
    project_metadata: Vec<ImageMetadata>,
    current_series: usize,
    /// LMS metadata derived from the XLEF/XLIF document graph (faithful
    /// translation of LMSMetadataExtractor), keyed by referenced image file path.
    /// Used to overlay dims/order/pixel-type onto delegate-derived metadata.
    xlif_lms_by_image: std::collections::HashMap<PathBuf, ImageMetadata>,
}

struct XlefDelegate {
    reader: Box<dyn FormatReader>,
    path: PathBuf,
}

struct XlefMultiImage {
    delegates: Vec<usize>,
}

#[derive(Clone)]
struct XlefLmsPixelLeaf {
    storage_path: PathBuf,
    x_stride: usize,
    row_stride: usize,
    z_stride: usize,
    c_stride: usize,
    t_stride: usize,
}

#[derive(Clone)]
enum XlefSeriesRef {
    Delegate {
        delegate: usize,
        series: usize,
        /// Tile index within the referencing XLIF tilescan (0 when not a tilescan).
        tile: usize,
    },
    Lms {
        metadata: usize,
        pixels: Option<usize>,
    },
    MultiImage {
        multi: usize,
    },
}

impl XlefReader {
    pub fn new() -> Self {
        XlefReader {
            delegates: Vec::new(),
            multi_images: Vec::new(),
            lms_metadata: Vec::new(),
            lms_pixels: Vec::new(),
            series_map: Vec::new(),
            project_metadata: Vec::new(),
            current_series: 0,
            xlif_lms_by_image: std::collections::HashMap::new(),
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

    /// Faithful translation of the XLEF document-graph traversal
    /// (XlefDocument/XlcfDocument/XlifDocument + LMSMetadataExtractor): build the
    /// LMS-derived ImageMetadata for each XLIF-referenced image, keyed by the
    /// (canonicalised) image file path so it can be overlaid onto the matching
    /// pixel delegate's metadata.
    fn build_xlif_lms_map(&mut self, project_path: &Path) {
        self.xlif_lms_by_image.clear();
        let Ok(project) = crate::formats::leica_lms::XlefDocument::new(project_path) else {
            return;
        };
        for xlif in project.get_xlifs() {
            let Ok(meta) = crate::formats::leica_lms::image_metadata_from_xlif(xlif) else {
                continue;
            };
            // An xlif may reference one image (TIF/LOF) or several frames; apply
            // the same image-level LMS metadata to each referenced image path.
            for image_path in &xlif.image_paths {
                let key = std::fs::canonicalize(image_path).unwrap_or_else(|_| image_path.clone());
                self.xlif_lms_by_image.insert(key, meta.clone());
            }
        }
    }

    /// Overlay the LMS-derived core dimensions (sizes / dimension order / pixel
    /// type / physical sizes / channel colours) onto a delegate's metadata when
    /// the delegate's source image is referenced by an XLIF in the project graph.
    /// Mirrors XLEFReader using XLIF metadata for its referenced images rather
    /// than the delegate's own guesses.
    fn overlay_xlif_lms(&self, meta: &mut ImageMetadata, source_path: &str) {
        let path = PathBuf::from(source_path);
        let key = std::fs::canonicalize(&path).unwrap_or(path);
        let Some(lms) = self.xlif_lms_by_image.get(&key) else {
            return;
        };
        if lms.size_x > 0 {
            meta.size_x = lms.size_x;
        }
        if lms.size_y > 0 {
            meta.size_y = lms.size_y;
        }
        meta.size_z = lms.size_z.max(1);
        meta.size_c = lms.size_c.max(1);
        meta.size_t = lms.size_t.max(1);
        meta.dimension_order = lms.dimension_order;
        meta.pixel_type = lms.pixel_type;
        meta.bits_per_pixel = lms.bits_per_pixel;
        meta.is_rgb = lms.is_rgb;
        meta.is_interleaved = lms.is_interleaved;
        meta.is_indexed = lms.is_indexed;
        meta.image_count = lms.image_count.max(1);
        for (key, value) in &lms.series_metadata {
            meta.series_metadata
                .entry(key.clone())
                .or_insert_with(|| value.clone());
        }
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
            XlefSeriesRef::Lms {
                pixels: Some(_), ..
            } => Err(BioFormatsError::Format(
                "Leica XLEF LMS pixel leaf is not a delegate reader".into(),
            )),
            XlefSeriesRef::Lms { pixels: None, .. } => Err(BioFormatsError::UnsupportedFormat(
                "Leica XLEF LMS metadata series has no pixel delegate yet".into(),
            )),
            XlefSeriesRef::MultiImage { .. } => Err(BioFormatsError::Format(
                "Leica XLEF multi-image series is not a single delegate reader".into(),
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
            XlefSeriesRef::MultiImage { multi } => self
                .multi_images
                .get(*multi)
                .and_then(|multi| multi.delegates.first())
                .and_then(|delegate| self.delegates.get(*delegate))
                .map(|delegate| delegate.reader.as_ref()),
        }
    }

    fn current_lms_metadata(&self) -> Option<&ImageMetadata> {
        match self.series_map.get(self.current_series)? {
            XlefSeriesRef::Delegate { .. } => None,
            XlefSeriesRef::Lms { metadata, .. } => self.lms_metadata.get(*metadata),
            XlefSeriesRef::MultiImage { .. } => None,
        }
    }

    fn current_lms_pixel_leaf(&self) -> Option<&XlefLmsPixelLeaf> {
        match self.series_map.get(self.current_series)? {
            XlefSeriesRef::Delegate { .. } => None,
            XlefSeriesRef::Lms {
                pixels: Some(pixels),
                ..
            } => self.lms_pixels.get(*pixels),
            XlefSeriesRef::Lms { pixels: None, .. } => None,
            XlefSeriesRef::MultiImage { .. } => None,
        }
    }

    /// Builds the unsupported-pixel error for an LMS metadata-only series. Returns
    /// `None` for pixel-delegate series (which can decode normally). When the LMS
    /// leaf declared a raw pixel layout, the message enumerates the declaration
    /// (Java LMSMainXmlNodes: ChannelDescription/DimensionDescription BytesInc plus
    /// Memory/Storage block nodes) so callers know exactly what was unsupported.
    fn lms_pixel_delegate_error(&self) -> Option<BioFormatsError> {
        if self.current_lms_pixel_leaf().is_some() {
            return None;
        }
        let meta = self.current_lms_metadata()?;
        let int = |key: &str| match meta.series_metadata.get(key) {
            Some(crate::common::metadata::MetadataValue::Int(value)) => *value,
            _ => 0,
        };
        let declared = matches!(
            meta.series_metadata.get("xlef.lms.pixel_layout.status"),
            Some(crate::common::metadata::MetadataValue::String(status)) if status == "declared_unsupported"
        );
        let path = xlef_lms_metadata_string(meta, "xlef.lms.path").unwrap_or_default();
        let message = if declared {
            format!(
                "Leica XLEF LMS metadata series has no pixel delegate yet: unsupported LMS pixel layout declared by {path} ({} ChannelDescription BytesInc strides, {} DimensionDescription BytesInc strides, {} memory nodes, {} storage nodes)",
                int("xlef.lms.pixel_layout.channel_bytes_inc_count"),
                int("xlef.lms.pixel_layout.dimension_bytes_inc_count"),
                int("xlef.lms.pixel_layout.memory_count"),
                int("xlef.lms.pixel_layout.storage_count"),
            )
        } else {
            "Leica XLEF LMS metadata series has no pixel delegate yet".into()
        };
        Some(BioFormatsError::UnsupportedFormat(message))
    }

    fn add_delegate(
        &mut self,
        reference: &Path,
        tile_count: u32,
        tile_index_base: usize,
        mut reader: Box<dyn FormatReader>,
    ) -> Result<()> {
        reader.set_id(reference)?;
        self.add_initialized_delegate(reference, tile_count, tile_index_base, reader)
    }

    fn add_initialized_delegate(
        &mut self,
        reference: &Path,
        tile_count: u32,
        tile_index_base: usize,
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
        // Tilescans are exposed as one series per tile (Java: LIFReader/XLEF
        // getReaderIndex semantics). The delegate itself only knows one physical
        // series; each tile re-reads the same delegate series.
        let tile_count = tile_count.max(1) as usize;
        for series in 0..series_count {
            for tile in 0..tile_count {
                self.series_map.push(XlefSeriesRef::Delegate {
                    delegate: delegate_index,
                    series,
                    tile: tile_index_base + tile,
                });
            }
        }
        self.delegates.push(XlefDelegate {
            reader,
            path: reference.to_path_buf(),
        });
        Ok(())
    }

    fn add_multi_image(&mut self, references: &[PathBuf]) -> Result<()> {
        if references.is_empty() {
            return Ok(());
        }
        let mut delegates = Vec::with_capacity(references.len());
        for reference in references {
            let mut reader = xlef_delegate_for_reference(reference);
            reader.set_id(reference)?;
            if reader.series_count() == 0 {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "Leica XLEF referenced image {} exposes no readable series",
                    reference.display()
                )));
            }
            let delegate_index = self.delegates.len();
            self.delegates.push(XlefDelegate {
                reader,
                path: reference.clone(),
            });
            delegates.push(delegate_index);
        }
        let multi = self.multi_images.len();
        self.multi_images.push(XlefMultiImage { delegates });
        self.series_map.push(XlefSeriesRef::MultiImage { multi });
        Ok(())
    }

    fn rebuild_project_metadata(&mut self, project_path: &Path) -> Result<()> {
        let series_count = self.series_map.len();
        let mut metadata = Vec::with_capacity(series_count);

        for series_index in 0..series_count {
            let mapping = self.series_map[series_index].clone();
            let (mut meta, source_path, source_kind, tile) = match mapping {
                XlefSeriesRef::Delegate {
                    delegate,
                    series,
                    tile,
                } => {
                    let delegate = self
                        .delegates
                        .get_mut(delegate)
                        .ok_or(BioFormatsError::NotInitialized)?;
                    delegate.reader.set_series(series)?;
                    (
                        delegate.reader.metadata().clone(),
                        delegate.path.display().to_string(),
                        "pixel_delegate",
                        Some(tile),
                    )
                }
                XlefSeriesRef::Lms { metadata, .. } => {
                    let meta = self
                        .lms_metadata
                        .get(metadata)
                        .ok_or(BioFormatsError::NotInitialized)?
                        .clone();
                    let source_path =
                        xlef_lms_metadata_string(&meta, "xlef.lms.path").unwrap_or_default();
                    let source_kind = if self.current_lms_pixel_for_mapping(mapping).is_some() {
                        "lms_pixel"
                    } else {
                        "lms_metadata"
                    };
                    (meta, source_path, source_kind, None)
                }
                XlefSeriesRef::MultiImage { multi } => {
                    let multi = self
                        .multi_images
                        .get(multi)
                        .ok_or(BioFormatsError::NotInitialized)?;
                    let first_delegate = *multi
                        .delegates
                        .first()
                        .ok_or(BioFormatsError::NotInitialized)?;
                    let delegate = self
                        .delegates
                        .get_mut(first_delegate)
                        .ok_or(BioFormatsError::NotInitialized)?;
                    delegate.reader.set_series(0)?;
                    let mut meta = delegate.reader.metadata().clone();
                    meta.image_count = multi.delegates.len() as u32;
                    (
                        meta,
                        delegate.path.display().to_string(),
                        "pixel_delegate",
                        None,
                    )
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
                MetadataValue::String(source_path.clone()),
            );
            meta.series_metadata.insert(
                "xlef.project.source_kind".into(),
                MetadataValue::String(source_kind.into()),
            );
            // For pixel-delegate series, prefer LMS metadata graph dimensions
            // over the delegate's guesses (XLEFReader uses xlif metadata).
            if source_kind == "pixel_delegate" {
                self.overlay_xlif_lms(&mut meta, &source_path);
            }
            if let Some(tile) = tile {
                meta.series_metadata.insert(
                    "xlef.project.tile_index".into(),
                    MetadataValue::Int(tile as i64),
                );
            }
            metadata.push(meta);
        }

        self.project_metadata = metadata;
        Ok(())
    }

    fn current_lms_pixel_for_mapping(&self, mapping: XlefSeriesRef) -> Option<&XlefLmsPixelLeaf> {
        match mapping {
            XlefSeriesRef::Lms {
                pixels: Some(pixels),
                ..
            } => self.lms_pixels.get(pixels),
            _ => None,
        }
    }

    fn open_multi_image_bytes(&mut self, multi: usize, plane_index: u32) -> Result<Vec<u8>> {
        let delegate = {
            let multi = self
                .multi_images
                .get(multi)
                .ok_or(BioFormatsError::NotInitialized)?;
            *multi
                .delegates
                .get(plane_index as usize)
                .ok_or(BioFormatsError::PlaneOutOfRange(plane_index))?
        };
        let delegate = self
            .delegates
            .get_mut(delegate)
            .ok_or(BioFormatsError::NotInitialized)?;
        delegate.reader.set_series(0)?;
        delegate.reader.open_bytes(0)
    }

    fn open_multi_image_region(
        &mut self,
        multi: usize,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let delegate = {
            let multi = self
                .multi_images
                .get(multi)
                .ok_or(BioFormatsError::NotInitialized)?;
            *multi
                .delegates
                .get(plane_index as usize)
                .ok_or(BioFormatsError::PlaneOutOfRange(plane_index))?
        };
        let delegate = self
            .delegates
            .get_mut(delegate)
            .ok_or(BioFormatsError::NotInitialized)?;
        delegate.reader.set_series(0)?;
        delegate.reader.open_bytes_region(0, x, y, w, h)
    }

    fn open_multi_image_thumb(&mut self, multi: usize, plane_index: u32) -> Result<Vec<u8>> {
        let delegate = {
            let multi = self
                .multi_images
                .get(multi)
                .ok_or(BioFormatsError::NotInitialized)?;
            *multi
                .delegates
                .get(plane_index as usize)
                .ok_or(BioFormatsError::PlaneOutOfRange(plane_index))?
        };
        let delegate = self
            .delegates
            .get_mut(delegate)
            .ok_or(BioFormatsError::NotInitialized)?;
        delegate.reader.set_series(0)?;
        delegate.reader.open_thumb_bytes(0)
    }

    fn open_lms_pixel_bytes(&self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self
            .current_lms_metadata()
            .ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        self.open_lms_pixel_region(plane_index, 0, 0, meta.size_x, meta.size_y)
    }

    fn open_lms_pixel_region(
        &self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let meta = self
            .current_lms_metadata()
            .ok_or(BioFormatsError::NotInitialized)?;
        let pixels = self
            .current_lms_pixel_leaf()
            .ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        if x > meta.size_x
            || y > meta.size_y
            || w > meta.size_x.saturating_sub(x)
            || h > meta.size_y.saturating_sub(y)
        {
            return Err(BioFormatsError::Format(format!(
                "Leica XLEF LMS region ({x}, {y}, {w}, {h}) exceeds image bounds {}x{}",
                meta.size_x, meta.size_y
            )));
        }
        let sample_bytes = meta.pixel_type.bytes_per_sample();
        let row_bytes = (w as usize)
            .checked_mul(sample_bytes)
            .ok_or_else(|| BioFormatsError::Format("Leica XLEF LMS row size overflows".into()))?;
        let out_len = row_bytes.checked_mul(h as usize).ok_or_else(|| {
            BioFormatsError::Format("Leica XLEF LMS region size overflows".into())
        })?;
        let plane_offset = xlef_lms_plane_offset(meta, pixels, plane_index)?;
        let x_offset = (x as usize)
            .checked_mul(pixels.x_stride)
            .ok_or_else(|| BioFormatsError::Format("Leica XLEF LMS X offset overflows".into()))?;
        let y_offset = (y as usize)
            .checked_mul(pixels.row_stride)
            .ok_or_else(|| BioFormatsError::Format("Leica XLEF LMS Y offset overflows".into()))?;
        let start_offset = plane_offset
            .checked_add(y_offset)
            .and_then(|offset| offset.checked_add(x_offset))
            .ok_or_else(|| {
                BioFormatsError::Format("Leica XLEF LMS byte offset overflows".into())
            })?;
        let mut out = vec![0; out_len];
        let mut file = File::open(&pixels.storage_path).map_err(BioFormatsError::Io)?;
        for row in 0..h as usize {
            let src_offset = start_offset
                .checked_add(row.checked_mul(pixels.row_stride).ok_or_else(|| {
                    BioFormatsError::Format("Leica XLEF LMS row offset overflows".into())
                })?)
                .ok_or_else(|| {
                    BioFormatsError::Format("Leica XLEF LMS row offset overflows".into())
                })?;
            let dst_offset = row.checked_mul(row_bytes).ok_or_else(|| {
                BioFormatsError::Format("Leica XLEF LMS output offset overflows".into())
            })?;
            let row_data = read_bytes_at(&mut file, src_offset as u64, row_bytes)?;
            out[dst_offset..dst_offset + row_bytes].copy_from_slice(&row_data);
        }
        Ok(out)
    }

    fn set_delegate_series_for_current(&mut self) -> Result<()> {
        if let Some(XlefSeriesRef::Delegate {
            delegate, series, ..
        }) = self.series_map.get(self.current_series).cloned()
        {
            self.delegates[delegate].reader.set_series(series)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum XlefReference {
    Image {
        path: PathBuf,
        tile_count: u32,
        tile_index_base: usize,
    },
    ImageSet(Vec<PathBuf>),
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
    // Tilescans: an XLIF document declares its tile count via the DimensionDescription
    // whose DimID is "10" (Java XlifDocument.getTileCount). Images referenced directly
    // by this XLIF inherit that tile count; non-XLIF documents default to a single tile.
    let doc_tile_count = if path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
        == Some("xlif")
    {
        xlef_xlif_tile_count(&xml)
    } else {
        1
    };
    let references = xlef_referenced_paths(&xml, path);
    let direct_supported_images = references
        .iter()
        .filter(|reference| xlef_is_supported_image_reference(reference))
        .count();
    let direct_xlif_tile_images =
        doc_tile_count > 1 && direct_supported_images > 1 && xlef_is_xlif_path(path);
    let mut tile_image_index = 0usize;
    let mut images: Vec<XlefReference> = Vec::new();
    let group_direct_xlif_images = xlef_is_xlif_path(path)
        && !direct_xlif_tile_images
        && direct_supported_images > 1
        && xlef_supported_references_are_same_format(&references);
    if group_direct_xlif_images {
        let paths: Vec<PathBuf> = references
            .iter()
            .filter(|reference| xlef_is_supported_image_reference(reference))
            .cloned()
            .collect();
        if !paths.is_empty() {
            images.push(XlefReference::ImageSet(paths));
        }
    }
    for reference in references {
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
            if group_direct_xlif_images {
                continue;
            }
            let tile_index_base = if direct_xlif_tile_images {
                let index = tile_image_index;
                tile_image_index += 1;
                index
            } else {
                0
            };
            let image = XlefReference::Image {
                path: reference,
                // Java XLEFReader creates one MultipleImagesReader per XLIF. When
                // a tilescan XLIF lists one image file per tile, those files are
                // the tile series themselves; multiplying each file by DimID 10
                // would overcount project series.
                tile_count: if direct_xlif_tile_images {
                    1
                } else {
                    doc_tile_count
                },
                tile_index_base,
            };
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

fn xlef_supported_references_are_same_format(paths: &[PathBuf]) -> bool {
    let mut format: Option<&'static str> = None;
    for path in paths
        .iter()
        .filter(|path| xlef_is_supported_image_reference(path))
    {
        let Some(next) = xlef_supported_reference_format(path) else {
            continue;
        };
        if let Some(format) = format {
            if format != next {
                return false;
            }
        } else {
            format = Some(next);
        }
    }
    format.is_some()
}

fn xlef_supported_reference_format(path: &Path) -> Option<&'static str> {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("tif") | Some("tiff") => Some("tif"),
        Some("lof") => Some("lof"),
        Some("jpg") | Some("jpeg") => Some("jpeg"),
        Some("png") => Some("png"),
        Some("bmp") => Some("bmp"),
        _ => None,
    }
}

fn xlef_is_xlif_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("xlif"))
        .unwrap_or(false)
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

// Translation of Leica XlifDocument.getTileCount(): scan DimensionDescription
// nodes and return the NumberOfElements of the one whose DimID is "10",
// defaulting to a single tile.
fn xlef_xlif_tile_count(xml: &str) -> u32 {
    for (name, attrs) in scn_scan_tags(xml) {
        if name != "DimensionDescription" {
            continue;
        }
        if attrs.get("DimID").map(|id| id.as_str()) == Some("10") {
            return attrs
                .get("NumberOfElements")
                .and_then(|value| value.trim().parse::<u32>().ok())
                .unwrap_or(1);
        }
    }
    1
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
        Some("lof") => Box::new(crate::formats::extended::LofReader::new()),
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
    // Pixel-layout diagnostics: Leica LMS metadata leaves declare their raw pixel
    // payout through per-channel/per-dimension BytesInc strides plus optional
    // Memory/Storage block nodes. We capture the shape of that declaration (Java
    // LMSMainXmlNodes: ChannelDescription/DimensionDescription/Memory/Storage) so
    // the unsupported-layout diagnostics can describe exactly what was declared.
    let mut channel_bytes_inc_count = 0i64;
    let mut dimension_bytes_inc_count = 0i64;
    let mut x_dimension_bytes_inc: Option<i64> = None;
    let mut memory_count = 0i64;
    let mut storage_count = 0i64;
    // ROI alias capture index (Java LeicaMicrosystemsMetadata ROIs/ROI extractor).
    let mut roi_index = 0u32;
    meta.series_metadata.insert(
        "xlef.lms.path".into(),
        crate::common::metadata::MetadataValue::String(reference.display().to_string()),
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
            if attrs.contains_key("BytesInc") {
                channel_bytes_inc_count += 1;
            }
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
            if let Some(bytes_inc) = attrs
                .get("BytesInc")
                .and_then(|v| v.trim().parse::<i64>().ok())
            {
                dimension_bytes_inc_count += 1;
                if dim_id == 1 {
                    x_dimension_bytes_inc = Some(bytes_inc);
                }
                meta.series_metadata.insert(
                    format!("xlef.lms.dimension.{dim_id}.bytes_inc"),
                    crate::common::metadata::MetadataValue::Int(bytes_inc),
                );
            }
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
        } else if name.eq_ignore_ascii_case("ROI") {
            // Translation of LeicaMicrosystemsMetadata ROI extraction: capture the
            // shape-defining attribute aliases (Rectangle X/Y/Width/Height,
            // Line X1/Y1/X2/Y2/TheZ, Ellipse CenterX/CenterY/RadiusX/RadiusY/TheC/TheT,
            // Point X/Y/IndexC) under xlef.lms.roi.N.* so they can later project to
            // OME shapes. The graph-count pass below counts the same ROI nodes.
            let prefix = format!("xlef.lms.roi.{roi_index}");
            roi_index += 1;
            for (attr, suffix) in [("ID", "id"), ("Name", "name")] {
                if let Some(value) = attrs.get(attr).filter(|v| !v.trim().is_empty()) {
                    meta.series_metadata.insert(
                        format!("{prefix}.{suffix}"),
                        crate::common::metadata::MetadataValue::String(value.trim().to_string()),
                    );
                }
            }
            for (attr, suffix) in [
                ("X", "x"),
                ("Y", "y"),
                ("Width", "width"),
                ("Height", "height"),
                ("X1", "x1"),
                ("Y1", "y1"),
                ("X2", "x2"),
                ("Y2", "y2"),
                ("CenterX", "center_x"),
                ("CenterY", "center_y"),
                ("RadiusX", "radius_x"),
                ("RadiusY", "radius_y"),
            ] {
                if let Some(value) = attrs.get(attr).and_then(|v| xlef_parse_f64(v)) {
                    meta.series_metadata.insert(
                        format!("{prefix}.{suffix}"),
                        crate::common::metadata::MetadataValue::Float(value),
                    );
                }
            }
            for (attr, suffix) in [
                ("TheZ", "the_z"),
                ("TheC", "the_c"),
                ("TheT", "the_t"),
                ("IndexC", "index_c"),
            ] {
                if let Some(value) = attrs.get(attr).and_then(|v| v.trim().parse::<i64>().ok()) {
                    meta.series_metadata.insert(
                        format!("{prefix}.{suffix}"),
                        crate::common::metadata::MetadataValue::Int(value),
                    );
                }
            }
            if let Some(shape) = attrs.get("Shape").filter(|v| !v.trim().is_empty()) {
                meta.series_metadata.insert(
                    format!("{prefix}.shape"),
                    crate::common::metadata::MetadataValue::String(shape.trim().to_string()),
                );
            }
        } else if name.eq_ignore_ascii_case("Memory") {
            memory_count += 1;
            if memory_count == 1 {
                if let Some(value) = attrs.get("Compression").filter(|v| !v.trim().is_empty()) {
                    meta.series_metadata.insert(
                        "xlef.lms.pixel_layout.compression".into(),
                        crate::common::metadata::MetadataValue::String(value.trim().to_string()),
                    );
                }
                if let Some(value) = attrs.get("MemoryBlockID").filter(|v| !v.trim().is_empty()) {
                    meta.series_metadata.insert(
                        "xlef.lms.pixel_layout.memory_block_id".into(),
                        crate::common::metadata::MetadataValue::String(value.trim().to_string()),
                    );
                }
            }
        } else if name.eq_ignore_ascii_case("Storage") {
            storage_count += 1;
            if storage_count == 1 {
                if let Some(value) = attrs.get("FileName").filter(|v| !v.trim().is_empty()) {
                    meta.series_metadata.insert(
                        "xlef.lms.pixel_layout.storage_reference".into(),
                        crate::common::metadata::MetadataValue::String(value.trim().to_string()),
                    );
                }
            }
        }
    }

    // Pixel-layout payload classification. A layout is "declared" when the leaf
    // exposes any raw-stride or block reference (Java LMSMainXmlNodes). We surface
    // the declared-but-unsupported state plus the diagnostic counts; metadata-only
    // leaves with no such declaration keep the plain "unsupported" payload marker.
    let pixel_layout_declared = channel_bytes_inc_count > 0
        || dimension_bytes_inc_count > 0
        || memory_count > 0
        || storage_count > 0;
    if pixel_layout_declared {
        meta.series_metadata.insert(
            "xlef.lms.pixel_payload".into(),
            crate::common::metadata::MetadataValue::String("declared_unsupported".into()),
        );
        meta.series_metadata.insert(
            "xlef.lms.pixel_layout.status".into(),
            crate::common::metadata::MetadataValue::String("declared_unsupported".into()),
        );
        meta.series_metadata.insert(
            "xlef.lms.pixel_layout.channel_bytes_inc_count".into(),
            crate::common::metadata::MetadataValue::Int(channel_bytes_inc_count),
        );
        meta.series_metadata.insert(
            "xlef.lms.pixel_layout.dimension_bytes_inc_count".into(),
            crate::common::metadata::MetadataValue::Int(dimension_bytes_inc_count),
        );
        meta.series_metadata.insert(
            "xlef.lms.pixel_layout.memory_count".into(),
            crate::common::metadata::MetadataValue::Int(memory_count),
        );
        meta.series_metadata.insert(
            "xlef.lms.pixel_layout.storage_count".into(),
            crate::common::metadata::MetadataValue::Int(storage_count),
        );
    } else {
        meta.series_metadata.insert(
            "xlef.lms.pixel_payload".into(),
            crate::common::metadata::MetadataValue::String("unsupported".into()),
        );
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
    if let Some(bytes_inc) = x_dimension_bytes_inc.filter(|v| *v > 0) {
        // Java LMSMetadataExtractor.setCoreDimensionSizes first treats an X
        // stride divisible by 3 as RGB and divides the stride, then
        // setPixelType uses FormatTools.pixelTypeFromBytes(bytes, false, true).
        let sample_bytes = if bytes_inc % 3 == 0 {
            meta.is_rgb = true;
            meta.is_interleaved = true;
            bytes_inc / 3
        } else {
            bytes_inc
        };
        if let Some(pixel_type) = xlef_lms_pixel_type_from_sample_bytes(sample_bytes) {
            meta.pixel_type = pixel_type;
            meta.bits_per_pixel = (pixel_type.bytes_per_sample() * 8) as u8;
        }
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

fn xlef_lms_pixel_type_from_sample_bytes(bytes: i64) -> Option<PixelType> {
    match bytes {
        1 => Some(PixelType::Uint8),
        2 => Some(PixelType::Uint16),
        4 => Some(PixelType::Float32),
        8 => Some(PixelType::Float64),
        _ => None,
    }
}

fn xlef_lms_pixel_leaf_for_metadata(
    lms_path: &Path,
    meta: &ImageMetadata,
) -> Result<Option<XlefLmsPixelLeaf>> {
    let status = xlef_lms_metadata_string(meta, "xlef.lms.pixel_layout.status");
    if status.as_deref() != Some("declared_unsupported") {
        return Ok(None);
    }
    if xlef_lms_metadata_int(meta, "xlef.lms.pixel_layout.memory_count").unwrap_or(0) != 0 {
        return Ok(None);
    }
    if xlef_lms_metadata_int(meta, "xlef.lms.pixel_layout.storage_count").unwrap_or(0) != 1 {
        return Ok(None);
    }
    if xlef_lms_metadata_int(meta, "xlef.lms.pixel_layout.dimension_bytes_inc_count").unwrap_or(0)
        < 2
    {
        return Ok(None);
    }
    if meta.is_rgb || meta.is_interleaved || meta.size_x == 0 || meta.size_y == 0 {
        return Ok(None);
    }
    let Some(storage_reference) =
        xlef_lms_metadata_string(meta, "xlef.lms.pixel_layout.storage_reference")
    else {
        return Ok(None);
    };
    let storage_path = xlef_lms_resolve_storage_path(lms_path, &storage_reference);
    if !storage_path.exists() {
        return Ok(None);
    }
    let bytes_per_sample = meta.pixel_type.bytes_per_sample();
    if bytes_per_sample == 0 {
        return Ok(None);
    }
    let Some(x_stride) = xlef_lms_positive_stride(meta, "xlef.lms.dimension.1.bytes_inc") else {
        return Ok(None);
    };
    let Some(row_stride) = xlef_lms_positive_stride(meta, "xlef.lms.dimension.2.bytes_inc") else {
        return Ok(None);
    };
    if x_stride != bytes_per_sample {
        return Ok(None);
    }
    let row_bytes = (meta.size_x as usize)
        .checked_mul(bytes_per_sample)
        .ok_or_else(|| BioFormatsError::Format("Leica XLEF LMS row stride overflows".into()))?;
    if row_stride < row_bytes {
        return Ok(None);
    }
    let z_stride = xlef_lms_optional_dimension_stride(meta, 3)?;
    let c_stride = xlef_lms_optional_dimension_stride(meta, 5)?;
    let t_stride = xlef_lms_optional_dimension_stride(meta, 4)?;
    let last_offset =
        xlef_lms_extent_last_byte(meta, x_stride, row_stride, z_stride, c_stride, t_stride)?;
    let actual = std::fs::metadata(&storage_path)
        .map_err(BioFormatsError::Io)?
        .len();
    if actual < last_offset as u64 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "Leica XLEF LMS storage file {} is shorter than declared strided pixel layout",
            storage_path.display()
        )));
    }
    Ok(Some(XlefLmsPixelLeaf {
        storage_path,
        x_stride,
        row_stride,
        z_stride,
        c_stride,
        t_stride,
    }))
}

fn xlef_lms_positive_stride(meta: &ImageMetadata, key: &str) -> Option<usize> {
    xlef_lms_metadata_int(meta, key)
        .and_then(|value| usize::try_from(value).ok().filter(|v| *v > 0))
}

fn xlef_lms_optional_dimension_stride(meta: &ImageMetadata, dim_id: u32) -> Result<usize> {
    let size = match dim_id {
        3 => meta.size_z.max(1),
        4 => meta.size_t.max(1),
        5 => meta.size_c.max(1),
        _ => 1,
    };
    let key = format!("xlef.lms.dimension.{dim_id}.bytes_inc");
    match xlef_lms_metadata_int(meta, &key) {
        Some(value) if value > 0 => usize::try_from(value).map_err(|_| {
            BioFormatsError::Format("Leica XLEF LMS dimension stride overflows".into())
        }),
        Some(_) if size > 1 => Err(BioFormatsError::UnsupportedFormat(format!(
            "Leica XLEF LMS dimension {dim_id} has multiple elements but no positive BytesInc"
        ))),
        _ => Ok(0),
    }
}

fn xlef_lms_extent_last_byte(
    meta: &ImageMetadata,
    x_stride: usize,
    row_stride: usize,
    z_stride: usize,
    c_stride: usize,
    t_stride: usize,
) -> Result<usize> {
    let mut offset = 0usize;
    for (size, stride) in [
        (meta.size_x, x_stride),
        (meta.size_y, row_stride),
        (meta.size_z.max(1), z_stride),
        (meta.size_c.max(1), c_stride),
        (meta.size_t.max(1), t_stride),
    ] {
        if size > 0 {
            offset = offset
                .checked_add((size as usize - 1).checked_mul(stride).ok_or_else(|| {
                    BioFormatsError::Format("Leica XLEF LMS pixel extent overflows".into())
                })?)
                .ok_or_else(|| {
                    BioFormatsError::Format("Leica XLEF LMS pixel extent overflows".into())
                })?;
        }
    }
    offset
        .checked_add(meta.pixel_type.bytes_per_sample())
        .ok_or_else(|| BioFormatsError::Format("Leica XLEF LMS pixel extent overflows".into()))
}

fn xlef_lms_plane_offset(
    meta: &ImageMetadata,
    pixels: &XlefLmsPixelLeaf,
    plane_index: u32,
) -> Result<usize> {
    let z = meta.size_z.max(1);
    let c = meta.size_c.max(1);
    let t = meta.size_t.max(1);
    let (zi, ci, ti) = match meta.dimension_order {
        DimensionOrder::XYZCT => {
            let zi = plane_index % z;
            let ci = (plane_index / z) % c;
            let ti = plane_index / z / c;
            (zi, ci, ti)
        }
        DimensionOrder::XYZTC => {
            let zi = plane_index % z;
            let ti = (plane_index / z) % t;
            let ci = plane_index / z / t;
            (zi, ci, ti)
        }
        DimensionOrder::XYCZT => {
            let ci = plane_index % c;
            let zi = (plane_index / c) % z;
            let ti = plane_index / c / z;
            (zi, ci, ti)
        }
        DimensionOrder::XYCTZ => {
            let ci = plane_index % c;
            let ti = (plane_index / c) % t;
            let zi = plane_index / c / t;
            (zi, ci, ti)
        }
        DimensionOrder::XYTCZ => {
            let ti = plane_index % t;
            let ci = (plane_index / t) % c;
            let zi = plane_index / t / c;
            (zi, ci, ti)
        }
        DimensionOrder::XYTZC => {
            let ti = plane_index % t;
            let zi = (plane_index / t) % z;
            let ci = plane_index / t / z;
            (zi, ci, ti)
        }
    };
    let mut offset = 0usize;
    for (index, stride) in [
        (zi, pixels.z_stride),
        (ci, pixels.c_stride),
        (ti, pixels.t_stride),
    ] {
        offset = offset
            .checked_add((index as usize).checked_mul(stride).ok_or_else(|| {
                BioFormatsError::Format("Leica XLEF LMS plane offset overflows".into())
            })?)
            .ok_or_else(|| {
                BioFormatsError::Format("Leica XLEF LMS plane offset overflows".into())
            })?;
    }
    Ok(offset)
}

fn xlef_lms_resolve_storage_path(lms_path: &Path, storage_reference: &str) -> PathBuf {
    let dir = lms_path.parent().unwrap_or_else(|| Path::new(""));
    crate::formats::leica_lms::parse_file_path(dir, storage_reference.trim())
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
        "microscopedescription" | "microscope" => Some("microscope"),
        "filterdescription" | "filter" => Some("filter"),
        "dichroicdescription" | "dichroic" => Some("dichroic"),
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
        "CalibratedMagnification",
        "NumericalAperture",
        "NA",
        "WorkingDistance",
        "Immersion",
        "Correction",
        "Medium",
        "Gain",
        "Offset",
        "FilterType",
        "CutIn",
        "CutOut",
        "ExcitationWavelength",
        "EmissionWavelength",
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
            | "CalibratedMagnification"
            | "NumericalAperture"
            | "NA"
            | "WorkingDistance"
            | "Gain"
            | "Offset"
            | "CutIn"
            | "CutOut"
            | "ExcitationWavelength"
            | "EmissionWavelength"
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
    // Channel colour (Java LeicaMicrosystemsMetadata.Channel.setColor). Leica
    // encodes the channel LUT either as an HTML hex colour ("#RRGGBB") or as a
    // comma-separated "R,G,B" triple. We retain the original string and also pack
    // it into OME's signed-i32 RGBA word (R<<24 | G<<16 | B<<8 | 0xFF).
    if let Some(raw) = attrs
        .get("Color")
        .or_else(|| attrs.get("ColorRGB"))
        .filter(|v| !v.trim().is_empty())
    {
        let raw = raw.trim();
        meta.series_metadata.insert(
            format!("{prefix}.color"),
            crate::common::metadata::MetadataValue::String(raw.to_string()),
        );
        let rgb = if let Some(hex) = raw.strip_prefix('#') {
            (hex.len() == 6)
                .then(|| u32::from_str_radix(hex, 16).ok())
                .flatten()
                .map(|packed| {
                    (
                        ((packed >> 16) & 0xff) as i64,
                        ((packed >> 8) & 0xff) as i64,
                        (packed & 0xff) as i64,
                    )
                })
        } else {
            let parts: Vec<&str> = raw.split(',').map(|p| p.trim()).collect();
            if parts.len() == 3 {
                match (
                    parts[0].parse::<i64>(),
                    parts[1].parse::<i64>(),
                    parts[2].parse::<i64>(),
                ) {
                    (Ok(r), Ok(g), Ok(b)) => Some((r & 0xff, g & 0xff, b & 0xff)),
                    _ => None,
                }
            } else {
                None
            }
        };
        if let Some((r, g, b)) = rgb {
            let ome_color = ((r << 24) | (g << 16) | (b << 8) | 0xff) as i32 as i64;
            meta.series_metadata.insert(
                format!("{prefix}.ome_color"),
                crate::common::metadata::MetadataValue::Int(ome_color),
            );
        }
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

fn xlef_lms_metadata_int(meta: &ImageMetadata, key: &str) -> Option<i64> {
    match meta.series_metadata.get(key) {
        Some(crate::common::metadata::MetadataValue::Int(value)) => Some(*value),
        Some(crate::common::metadata::MetadataValue::Float(value)) if value.is_finite() => {
            Some(*value as i64)
        }
        Some(crate::common::metadata::MetadataValue::String(value)) => {
            value.trim().parse::<i64>().ok()
        }
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
            // OME channel colour: the LMS extractor pre-packed the channel LUT into
            // the signed RGBA word (xlef.lms.channel.N.ome_color).
            if let Some(crate::common::metadata::MetadataValue::Int(packed)) =
                meta.series_metadata.get(&format!("{prefix}.ome_color"))
            {
                channel.color = Some(*packed as i32);
            }

            // Per-channel light-source/detector settings (Java initLasers /
            // initDetectorModels per-channel assignment loops).
            channel.light_source_settings_id =
                xlef_lms_metadata_string(meta, &format!("{prefix}.light_source_settings_id"));
            channel.light_source_settings_attenuation = xlef_lms_metadata_float(
                meta,
                &format!("{prefix}.light_source_settings_attenuation"),
            );
            channel.detector_ref =
                xlef_lms_metadata_string(meta, &format!("{prefix}.detector_ref"));
            channel.detector_settings_gain =
                xlef_lms_metadata_float(meta, &format!("{prefix}.detector_settings_gain"));
            channel.detector_settings_offset =
                xlef_lms_metadata_float(meta, &format!("{prefix}.detector_settings_offset"));
            channel.pinhole_size = xlef_lms_metadata_float(meta, &format!("{prefix}.pinhole_size"));
        }

        // Per-channel emission-filter light paths (Java initDetectorModels'
        // live setLightPathEmissionFilterRef call).
        let channel_count = image.channels.len();
        let mut light_paths: Vec<crate::common::ome_metadata::OmeLightPath> = Vec::new();
        let mut any_light_path = false;
        for channel_index in 0..channel_count {
            let prefix = format!("xlef.lms.channel.{channel_index}");
            let mut light_path = crate::common::ome_metadata::OmeLightPath::default();
            if let Some(filter_ref) =
                xlef_lms_metadata_string(meta, &format!("{prefix}.emission_filter_ref"))
            {
                light_path.emission_filter_ids.push(filter_ref);
                any_light_path = true;
            }
            light_paths.push(light_path);
        }
        if any_light_path {
            image.light_paths = light_paths;
        }

        // Per-plane metadata (Java initImageDetails plane loop): positions, deltaT,
        // exposure times. One OmePlane per image plane, in plane index order.
        let mut plane_index = 0usize;
        while meta
            .series_metadata
            .keys()
            .any(|k| k.starts_with(&format!("xlef.lms.plane.{plane_index}.")))
        {
            plane_index += 1;
        }
        if plane_index > 0 {
            let mut planes = Vec::with_capacity(plane_index);
            for index in 0..plane_index {
                let prefix = format!("xlef.lms.plane.{index}");
                planes.push(crate::common::ome_metadata::OmePlane {
                    the_z: 0,
                    the_c: 0,
                    the_t: 0,
                    delta_t: xlef_lms_metadata_float(meta, &format!("{prefix}.delta_t")),
                    exposure_time: xlef_lms_metadata_float(
                        meta,
                        &format!("{prefix}.exposure_time"),
                    ),
                    position_x: xlef_lms_metadata_float(meta, &format!("{prefix}.position_x")),
                    position_y: xlef_lms_metadata_float(meta, &format!("{prefix}.position_y")),
                    position_z: xlef_lms_metadata_float(meta, &format!("{prefix}.position_z")),
                });
            }
            image.planes = planes;
        }
    }

    // Instrument graph (Java LeicaMicrosystemsMetadata hardware extractors). The
    // generic OME projection deliberately skips the xlef.lms.* namespace, so we
    // build the single LMS instrument explicitly here from the captured graph keys.
    let mut instrument = crate::common::ome_metadata::OmeInstrument {
        id: Some(crate::common::ome_metadata::create_lsid("Instrument", &[0])),
        microscope_model: xlef_lms_metadata_string(meta, "xlef.lms.microscope.0.name"),
        microscope_manufacturer: xlef_lms_metadata_string(
            meta,
            "xlef.lms.microscope.0.manufacturer",
        ),
        ..Default::default()
    };
    let objective_model = xlef_lms_metadata_string(meta, "xlef.lms.objective.0.name");
    if objective_model.is_some()
        || meta
            .series_metadata
            .keys()
            .any(|k| k.starts_with("xlef.lms.objective.0."))
    {
        instrument
            .objectives
            .push(crate::common::ome_metadata::OmeObjective {
                id: Some(crate::common::ome_metadata::create_lsid(
                    "Objective",
                    &[0, 0],
                )),
                model: objective_model,
                manufacturer: xlef_lms_metadata_string(meta, "xlef.lms.objective.0.manufacturer"),
                nominal_magnification: xlef_lms_metadata_float(
                    meta,
                    "xlef.lms.objective.0.magnification",
                ),
                calibrated_magnification: xlef_lms_metadata_float(
                    meta,
                    "xlef.lms.objective.0.calibrated_magnification",
                ),
                lens_na: xlef_lms_metadata_float(meta, "xlef.lms.objective.0.numerical_aperture"),
                immersion: xlef_lms_metadata_string(meta, "xlef.lms.objective.0.immersion"),
                correction: xlef_lms_metadata_string(meta, "xlef.lms.objective.0.correction"),
                working_distance: xlef_lms_metadata_float(
                    meta,
                    "xlef.lms.objective.0.working_distance",
                ),
            });
    }
    // Instrument-level detector array (MetadataStoreInitializer.initDetectorModels):
    // one OmeDetector per `xlef.lms.detector.N.*` group, N = 0, 1, 2, ...
    {
        let mut n = 0usize;
        while meta
            .series_metadata
            .keys()
            .any(|k| k.starts_with(&format!("xlef.lms.detector.{n}.")))
        {
            instrument
                .detectors
                .push(crate::common::ome_metadata::OmeDetector {
                    id: Some(crate::common::ome_metadata::create_lsid(
                        "Detector",
                        &[0, n],
                    )),
                    model: xlef_lms_metadata_string(meta, &format!("xlef.lms.detector.{n}.name")),
                    manufacturer: xlef_lms_metadata_string(
                        meta,
                        &format!("xlef.lms.detector.{n}.manufacturer"),
                    ),
                    detector_type: xlef_lms_metadata_string(
                        meta,
                        &format!("xlef.lms.detector.{n}.type"),
                    ),
                    gain: xlef_lms_metadata_float(meta, &format!("xlef.lms.detector.{n}.gain")),
                    offset: xlef_lms_metadata_float(meta, &format!("xlef.lms.detector.{n}.offset")),
                });
            n += 1;
        }
    }
    // Instrument-level laser array (MetadataStoreInitializer.initLasers):
    // one OmeLightSource per `xlef.lms.laser.N.*` group, N = 0, 1, 2, ...
    {
        let mut n = 0usize;
        while meta
            .series_metadata
            .keys()
            .any(|k| k.starts_with(&format!("xlef.lms.laser.{n}.")))
        {
            instrument
                .light_sources
                .push(crate::common::ome_metadata::OmeLightSource {
                    id: Some(crate::common::ome_metadata::create_lsid(
                        "LightSource",
                        &[0, n],
                    )),
                    model: xlef_lms_metadata_string(meta, &format!("xlef.lms.laser.{n}.name")),
                    manufacturer: xlef_lms_metadata_string(
                        meta,
                        &format!("xlef.lms.laser.{n}.manufacturer"),
                    ),
                    light_source_type: Some("Laser".into()),
                    power: xlef_lms_metadata_float(meta, &format!("xlef.lms.laser.{n}.power")),
                    wavelength: xlef_lms_metadata_float(
                        meta,
                        &format!("xlef.lms.laser.{n}.wavelength"),
                    ),
                });
            n += 1;
        }
    }
    if meta
        .series_metadata
        .keys()
        .any(|k| k.starts_with("xlef.lms.filter.0."))
    {
        instrument
            .filters
            .push(crate::common::ome_metadata::OmeFilter {
                id: Some(crate::common::ome_metadata::create_lsid("Filter", &[0, 0])),
                model: xlef_lms_metadata_string(meta, "xlef.lms.filter.0.name"),
                manufacturer: xlef_lms_metadata_string(meta, "xlef.lms.filter.0.manufacturer"),
                filter_type: xlef_lms_metadata_string(meta, "xlef.lms.filter.0.filter_type"),
                cut_in: xlef_lms_metadata_float(meta, "xlef.lms.filter.0.cut_in"),
                cut_out: xlef_lms_metadata_float(meta, "xlef.lms.filter.0.cut_out"),
            });
    }
    if meta
        .series_metadata
        .keys()
        .any(|k| k.starts_with("xlef.lms.dichroic.0."))
    {
        instrument
            .dichroics
            .push(crate::common::ome_metadata::OmeDichroic {
                id: Some(crate::common::ome_metadata::create_lsid(
                    "Dichroic",
                    &[0, 0],
                )),
                model: xlef_lms_metadata_string(meta, "xlef.lms.dichroic.0.name"),
                manufacturer: xlef_lms_metadata_string(meta, "xlef.lms.dichroic.0.manufacturer"),
            });
    }
    if instrument.microscope_model.is_some()
        || instrument.microscope_manufacturer.is_some()
        || !instrument.objectives.is_empty()
        || !instrument.detectors.is_empty()
        || !instrument.light_sources.is_empty()
        || !instrument.filters.is_empty()
        || !instrument.dichroics.is_empty()
    {
        if let Some(image) = ome.images.get_mut(0) {
            image.instrument_ref = Some(0);
            if !instrument.objectives.is_empty() {
                image.objective_ref = Some(0);
            }
        }
        ome.instruments = vec![instrument];
    }

    // ROIs (Java LeicaMicrosystemsMetadata ROIs/ROI extractor). Project the captured
    // xlef.lms.roi.N.* alias keys into OME ROI shapes, dispatching on the recorded
    // Shape (Line/Ellipse/Point) or, when absent, on which geometry aliases are set.
    let mut roi_count = 0usize;
    while meta
        .series_metadata
        .keys()
        .any(|k| k.starts_with(&format!("xlef.lms.roi.{roi_count}.")))
    {
        roi_count += 1;
    }
    if roi_count > 0 {
        let mut rois = Vec::with_capacity(roi_count);
        for index in 0..roi_count {
            let prefix = format!("xlef.lms.roi.{index}");
            let the_z = xlef_lms_metadata_float(meta, &format!("{prefix}.the_z")).map(|v| v as u32);
            let the_t = xlef_lms_metadata_float(meta, &format!("{prefix}.the_t")).map(|v| v as u32);
            // OME Point/Ellipse channel index: Leica Point ROIs carry IndexC, while
            // Ellipse ROIs carry TheC; both project onto OME the_c.
            let the_c = xlef_lms_metadata_float(meta, &format!("{prefix}.the_c"))
                .or_else(|| xlef_lms_metadata_float(meta, &format!("{prefix}.index_c")))
                .map(|v| v as u32);
            let shape_kind = xlef_lms_metadata_string(meta, &format!("{prefix}.shape"));
            let x = xlef_lms_metadata_float(meta, &format!("{prefix}.x"));
            let y = xlef_lms_metadata_float(meta, &format!("{prefix}.y"));
            let center_x = xlef_lms_metadata_float(meta, &format!("{prefix}.center_x"));
            let center_y = xlef_lms_metadata_float(meta, &format!("{prefix}.center_y"));
            let x1 = xlef_lms_metadata_float(meta, &format!("{prefix}.x1"));
            let y1 = xlef_lms_metadata_float(meta, &format!("{prefix}.y1"));
            let x2 = xlef_lms_metadata_float(meta, &format!("{prefix}.x2"));
            let y2 = xlef_lms_metadata_float(meta, &format!("{prefix}.y2"));
            let width = xlef_lms_metadata_float(meta, &format!("{prefix}.width"));
            let height = xlef_lms_metadata_float(meta, &format!("{prefix}.height"));
            let radius_x = xlef_lms_metadata_float(meta, &format!("{prefix}.radius_x"));
            let radius_y = xlef_lms_metadata_float(meta, &format!("{prefix}.radius_y"));

            let points_raw = xlef_lms_metadata_string(meta, &format!("{prefix}.points"));
            let is_polygon = shape_kind
                .as_deref()
                .map(|s| s.eq_ignore_ascii_case("Polygon"))
                .unwrap_or(false)
                || points_raw.is_some();
            let is_line = shape_kind
                .as_deref()
                .map(|s| s.eq_ignore_ascii_case("Line"))
                .unwrap_or(false)
                || (x1.is_some() && y1.is_some() && x2.is_some() && y2.is_some());
            let is_ellipse = shape_kind
                .as_deref()
                .map(|s| s.eq_ignore_ascii_case("Ellipse"))
                .unwrap_or(false)
                || (radius_x.is_some() && radius_y.is_some());

            let shape = if is_polygon {
                let points: Vec<(f64, f64)> = points_raw
                    .as_deref()
                    .map(|raw| {
                        raw.split_whitespace()
                            .filter_map(|pair| {
                                let mut it = pair.split(',');
                                let px = it.next()?.parse::<f64>().ok()?;
                                let py = it.next()?.parse::<f64>().ok()?;
                                Some((px, py))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                if points.is_empty() {
                    None
                } else {
                    Some(crate::common::ome_metadata::OmeShape::Polygon {
                        points,
                        the_z,
                        the_t,
                        the_c,
                    })
                }
            } else if is_line {
                match (x1, y1, x2, y2) {
                    (Some(x1), Some(y1), Some(x2), Some(y2)) => {
                        Some(crate::common::ome_metadata::OmeShape::Line {
                            x1,
                            y1,
                            x2,
                            y2,
                            the_z,
                            the_t,
                            the_c,
                        })
                    }
                    _ => None,
                }
            } else if is_ellipse {
                match (center_x.or(x), center_y.or(y), radius_x, radius_y) {
                    (Some(x), Some(y), Some(radius_x), Some(radius_y)) => {
                        Some(crate::common::ome_metadata::OmeShape::Ellipse {
                            x,
                            y,
                            radius_x,
                            radius_y,
                            the_z,
                            the_t,
                            the_c,
                        })
                    }
                    _ => None,
                }
            } else if let (Some(x), Some(y), Some(width), Some(height)) = (x, y, width, height) {
                Some(crate::common::ome_metadata::OmeShape::Rectangle {
                    x,
                    y,
                    width,
                    height,
                    the_z,
                    the_t,
                    the_c,
                })
            } else if let (Some(x), Some(y)) = (x, y) {
                Some(crate::common::ome_metadata::OmeShape::Point {
                    x,
                    y,
                    the_z,
                    the_t,
                    the_c,
                })
            } else {
                None
            };

            let id = xlef_lms_metadata_string(meta, &format!("{prefix}.id"))
                .or_else(|| Some(crate::common::ome_metadata::create_lsid("ROI", &[index])));
            let name = xlef_lms_metadata_string(meta, &format!("{prefix}.name"));
            rois.push(crate::common::ome_metadata::OmeROI {
                id,
                name,
                shapes: shape.into_iter().collect(),
            });
        }
        ome.rois = rois;
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
                XlefReference::Image {
                    path,
                    tile_count,
                    tile_index_base,
                } => {
                    self.add_delegate(
                        &path,
                        tile_count,
                        tile_index_base,
                        xlef_delegate_for_reference(&path),
                    )?;
                }
                XlefReference::ImageSet(paths) => {
                    self.add_multi_image(&paths)?;
                }
                XlefReference::Lms(reference) => {
                    if let Some(reader) = xlef_lms_delegate_for_reference(&reference)? {
                        self.add_initialized_delegate(&reference, 1, 0, reader)?;
                    } else {
                        let mut metadata = xlef_lms_metadata_for_reference(&reference)?;
                        let metadata_index = self.lms_metadata.len();
                        let pixels = xlef_lms_pixel_leaf_for_metadata(&reference, &metadata)?;
                        let pixel_index = pixels.map(|pixels| {
                            metadata.series_metadata.insert(
                                "xlef.lms.pixel_payload".into(),
                                MetadataValue::String("raw_storage".into()),
                            );
                            metadata.series_metadata.insert(
                                "xlef.lms.pixel_layout.status".into(),
                                MetadataValue::String("supported_raw_storage".into()),
                            );
                            metadata.series_metadata.insert(
                                "xlef.lms.pixel_layout.storage_path".into(),
                                MetadataValue::String(pixels.storage_path.display().to_string()),
                            );
                            let index = self.lms_pixels.len();
                            self.lms_pixels.push(pixels);
                            index
                        });
                        self.lms_metadata.push(metadata);
                        self.series_map.push(XlefSeriesRef::Lms {
                            metadata: metadata_index,
                            pixels: pixel_index,
                        });
                    }
                }
            }
        }
        self.current_series = 0;
        self.build_xlif_lms_map(path);
        self.rebuild_project_metadata(path)?;
        self.set_delegate_series_for_current()?;
        Ok(())
    }
    fn close(&mut self) -> Result<()> {
        for delegate in &mut self.delegates {
            delegate.reader.close()?;
        }
        self.delegates.clear();
        self.multi_images.clear();
        self.lms_metadata.clear();
        self.lms_pixels.clear();
        self.series_map.clear();
        self.project_metadata.clear();
        self.xlif_lms_by_image.clear();
        self.current_series = 0;
        Ok(())
    }
    fn series_count(&self) -> usize {
        self.series_map.len()
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        let mapping = self
            .series_map
            .get(s)
            .ok_or(BioFormatsError::SeriesOutOfRange(s))?
            .clone();
        if let XlefSeriesRef::Delegate {
            delegate, series, ..
        } = mapping
        {
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
        if let Some(error) = self.lms_pixel_delegate_error() {
            return Err(error);
        }
        if let Some(XlefSeriesRef::MultiImage { multi }) =
            self.series_map.get(self.current_series).cloned()
        {
            return self.open_multi_image_bytes(multi, p);
        }
        if self.current_lms_pixel_leaf().is_some() {
            return self.open_lms_pixel_bytes(p);
        }
        self.current_delegate_mut()?.open_bytes(p)
    }
    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        if let Some(error) = self.lms_pixel_delegate_error() {
            return Err(error);
        }
        if let Some(XlefSeriesRef::MultiImage { multi }) =
            self.series_map.get(self.current_series).cloned()
        {
            return self.open_multi_image_region(multi, p, x, y, w, h);
        }
        if self.current_lms_pixel_leaf().is_some() {
            return self.open_lms_pixel_region(p, x, y, w, h);
        }
        self.current_delegate_mut()?
            .open_bytes_region(p, x, y, w, h)
    }
    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        if let Some(XlefSeriesRef::MultiImage { multi }) =
            self.series_map.get(self.current_series).cloned()
        {
            return self.open_multi_image_thumb(multi, p);
        }
        if self.current_lms_pixel_leaf().is_some() {
            let meta = self.metadata();
            let tw = meta.size_x.min(256);
            let th = meta.size_y.min(256);
            let tx = (meta.size_x - tw) / 2;
            let ty = (meta.size_y - th) / 2;
            return self.open_lms_pixel_region(p, tx, ty, tw, th);
        }
        self.current_delegate_mut()?.open_thumb_bytes(p)
    }
    fn resolution_count(&self) -> usize {
        self.current_delegate()
            .map(|reader| reader.resolution_count())
            .unwrap_or(1)
    }
    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if let Some(XlefSeriesRef::MultiImage { multi }) =
            self.series_map.get(self.current_series).cloned()
        {
            let delegates = self
                .multi_images
                .get(multi)
                .ok_or(BioFormatsError::NotInitialized)?
                .delegates
                .clone();
            for delegate in delegates {
                let delegate = self
                    .delegates
                    .get_mut(delegate)
                    .ok_or(BioFormatsError::NotInitialized)?;
                delegate.reader.set_series(0)?;
                delegate.reader.set_resolution(level)?;
            }
            return Ok(());
        }
        self.current_delegate_mut()?.set_resolution(level)
    }
    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        match self.series_map.get(self.current_series)? {
            XlefSeriesRef::Delegate { delegate, .. } => {
                self.delegates.get(*delegate)?.reader.ome_metadata()
            }
            XlefSeriesRef::Lms { metadata, .. } => {
                self.lms_metadata.get(*metadata).map(xlef_lms_ome_metadata)
            }
            XlefSeriesRef::MultiImage { .. } => {
                Some(crate::common::ome_metadata::OmeMetadata::from_image_metadata(self.metadata()))
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
    /// Physical files containing native pixel blocks, matching Java
    /// `getSeriesUsedFiles(false)`.
    used_files: Vec<PathBuf>,
}

/// Internal state of an initialized [`OirReader`].
enum OirState {
    /// Native `OLYMPUSRAWFORMAT` container.
    Native(Box<OirNative>),
    /// `.oir`-named file that is actually a TIFF; delegated to `TiffReader`.
    /// Carries an overridden metadata copy (e.g. ImageJ channel count).
    Tiff(Box<crate::tiff::TiffReader>, ImageMetadata, PathBuf),
}

/// Olympus OIR format reader (`.oir`).
pub struct OirReader {
    state: Option<OirState>,
}

impl OirReader {
    pub fn new() -> Self {
        OirReader { state: None }
    }

    pub fn series_used_files(&self) -> Vec<PathBuf> {
        match &self.state {
            Some(OirState::Native(n)) => n.used_files.clone(),
            Some(OirState::Tiff(_, _, path)) => vec![path.clone()],
            None => Vec::new(),
        }
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
                if let Some(s) = crate::common::xml::decode_xml_text(&t) {
                    text.push_str(&s);
                }
            }
            Ok(Event::GeneralRef(r)) if capture && depth_match > 0 => {
                if let Some(s) = crate::common::xml::decode_xml_ref(&r) {
                    text.push_str(&s);
                }
            }
            Ok(Event::CData(c)) if capture && depth_match > 0 => {
                if let Ok(s) = c.xml_content(quick_xml::XmlVersion::Implicit1_0) {
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
fn oir_resolve_main_and_companions(path: &Path) -> (PathBuf, String, Vec<PathBuf>) {
    let parent = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    let mut stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();
    let is_numbered_companion = name.rsplit_once('_').is_some_and(|(prefix, suffix)| {
        !prefix.is_empty() && suffix.len() == 5 && suffix.parse::<u32>().is_ok()
    });
    if is_numbered_companion && !check_extension(path, "oir") {
        if let Some((prefix, _)) = name.rsplit_once('_') {
            stem = prefix.to_string();
        }
    }

    let mut main = path.to_path_buf();
    if is_numbered_companion && !check_extension(path, "oir") {
        if let Ok(entries) = std::fs::read_dir(&parent) {
            for entry in entries.flatten() {
                let candidate = entry.file_name().to_string_lossy().into_owned();
                if candidate.starts_with(&stem)
                    && candidate.eq_ignore_ascii_case(&format!("{stem}.oir"))
                {
                    main = entry.path();
                    break;
                }
            }
        }
    }

    let mut files: Vec<PathBuf> = vec![main.clone()];
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
    (main, stem, files)
}

fn check_extension(path: &Path, ext: &str) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case(ext))
}

fn parse_oir_native(path: &Path) -> Result<OirNative> {
    // Resolve companion files: <base>_00001, <base>_00002, ... in the same dir.
    // Java also accepts initialization from one of those companion files and
    // resets `currentId` to the matching `<base>.oir` before scanning.
    let (_main, stem, files) = oir_resolve_main_and_companions(path);

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

    meta.dimension_order = oir_dimension_order(&stem, meta.size_z, meta.size_t);

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

    let mut used_files: Vec<PathBuf> = pixel_blocks
        .iter()
        .map(|(_, block)| block.file.clone())
        .collect();
    used_files.sort();
    used_files.dedup();

    Ok(OirNative {
        meta,
        czt_blocks,
        used_files,
    })
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
    let mut active_text_for: Option<&'static str> = None;
    let mut active_text = String::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let name = e.name();
                let local = local_of(name.as_ref());
                if local == "axis" {
                    // could be the wrapper (dimensionAxis) or the inner name node
                    in_axis_wrapper += 1;
                    active_text_for = Some("axisname");
                    active_text.clear();
                } else if local == "maxSize" {
                    active_text_for = Some("maxsize");
                    active_text.clear();
                } else {
                    active_text_for = None;
                    active_text.clear();
                }
            }
            Ok(Event::Text(t)) => {
                if active_text_for.is_some() {
                    if let Some(s) = crate::common::xml::decode_xml_text(&t) {
                        active_text.push_str(&s);
                    }
                }
            }
            Ok(Event::GeneralRef(r)) => {
                if active_text_for.is_some() {
                    if let Some(s) = crate::common::xml::decode_xml_ref(&r) {
                        active_text.push_str(&s);
                    }
                }
            }
            Ok(Event::CData(t)) => {
                if active_text_for.is_some() {
                    if let Ok(s) = t.xml_content(quick_xml::XmlVersion::Implicit1_0) {
                        active_text.push_str(&s);
                    }
                }
            }
            Ok(Event::End(e)) => {
                let name = e.name();
                let local = local_of(name.as_ref());
                let closes_active = matches!(
                    (active_text_for, local),
                    (Some("axisname"), "axis") | (Some("maxsize"), "maxSize")
                );
                if closes_active {
                    let s = active_text.trim();
                    if active_text_for == Some("axisname") && !s.is_empty() {
                        cur_axis_name = Some(s.to_string());
                    } else if active_text_for == Some("maxsize") {
                        cur_max_size = s.parse::<u32>().ok();
                    }
                    active_text_for = None;
                    active_text.clear();
                }
                if cur_axis_name.is_some() && cur_max_size.is_some() {
                    let name = cur_axis_name.take().unwrap();
                    let size = cur_max_size.take().unwrap();
                    oir_apply_one_axis(&name, size, meta);
                }
                if local == "axis" {
                    in_axis_wrapper -= 1;
                    if in_axis_wrapper < 0 {
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

fn oir_dimension_order(
    base_name: &str,
    size_z: u32,
    size_t: u32,
) -> crate::common::metadata::DimensionOrder {
    if size_z <= 1 || size_t <= 1 {
        return crate::common::metadata::DimensionOrder::XYCZT;
    }
    let lower = base_name.to_ascii_lowercase();
    let z_index = lower.find('z').unwrap_or(usize::MAX);
    let t_index = lower.find('t').unwrap_or(usize::MAX);
    if z_index < t_index {
        crate::common::metadata::DimensionOrder::XYCTZ
    } else {
        crate::common::metadata::DimensionOrder::XYCZT
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

struct OirPendingChannel {
    id: String,
    index: Option<usize>,
    has_element_channels: bool,
}

fn oir_xml_attr_value(e: &quick_xml::events::BytesStart<'_>, name: &[u8]) -> Option<String> {
    e.attributes().flatten().find_map(|a| {
        if a.key.as_ref() == name {
            a.normalized_value(quick_xml::XmlVersion::Implicit1_0)
                .ok()
                .map(|v| v.into_owned())
        } else {
            None
        }
    })
}

fn oir_xml_order_attr(e: &quick_xml::events::BytesStart<'_>) -> Option<usize> {
    oir_xml_attr_value(e, b"order").and_then(|v| v.parse::<usize>().ok()?.checked_sub(1))
}

fn oir_insert_channel_id(slots: &mut Vec<Option<String>>, index: Option<usize>, id: String) {
    if id.is_empty() {
        return;
    }
    if slots
        .iter()
        .flatten()
        .any(|existing| existing.as_str() == id.as_str())
    {
        return;
    }
    if let Some(index) = index {
        if slots.len() <= index {
            slots.resize_with(index + 1, || None);
        }
        slots[index] = Some(id);
    } else {
        slots.push(Some(id));
    }
}

/// Collect channel ids from `commonphase:channel` / `commonphase:elementChannel`
/// nodes (the `id` attribute), respecting Java's `order`-based insertion.
fn oir_apply_channels(xml: &str, channel_ids: &mut Vec<String>) {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut pending: Vec<OirPendingChannel> = Vec::new();
    let mut slots: Vec<Option<String>> = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) => {
                let name = e.name();
                let local = local_of(name.as_ref());
                if local == "channel" {
                    pending.push(OirPendingChannel {
                        id: oir_xml_attr_value(e, b"id").unwrap_or_default(),
                        index: oir_xml_order_attr(e),
                        has_element_channels: false,
                    });
                } else if local == "elementChannel" {
                    if let Some(parent) = pending.last_mut() {
                        parent.has_element_channels = true;
                        let component_index = oir_xml_order_attr(e);
                        let index = parent
                            .index
                            .and_then(|parent_index| component_index.map(|c| parent_index * 3 + c));
                        if let Some(id) = oir_xml_attr_value(e, b"id") {
                            oir_insert_channel_id(&mut slots, index, id);
                        }
                    } else if let Some(id) = oir_xml_attr_value(e, b"id") {
                        oir_insert_channel_id(&mut slots, oir_xml_order_attr(e), id);
                    }
                }
            }
            Ok(Event::Empty(ref e)) => {
                let name = e.name();
                let local = local_of(name.as_ref());
                if local == "channel" {
                    if let Some(id) = oir_xml_attr_value(e, b"id") {
                        oir_insert_channel_id(&mut slots, oir_xml_order_attr(e), id);
                    }
                } else if local == "elementChannel" {
                    if let Some(parent) = pending.last_mut() {
                        parent.has_element_channels = true;
                        let component_index = oir_xml_order_attr(e);
                        let index = parent
                            .index
                            .and_then(|parent_index| component_index.map(|c| parent_index * 3 + c));
                        if let Some(id) = oir_xml_attr_value(e, b"id") {
                            oir_insert_channel_id(&mut slots, index, id);
                        }
                    } else if let Some(id) = oir_xml_attr_value(e, b"id") {
                        oir_insert_channel_id(&mut slots, oir_xml_order_attr(e), id);
                    }
                }
            }
            Ok(Event::End(ref e)) => {
                let name = e.name();
                if local_of(name.as_ref()) == "channel" {
                    if let Some(parent) = pending.pop() {
                        if !parent.has_element_channels {
                            oir_insert_channel_id(&mut slots, parent.index, parent.id);
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }
    for id in slots.into_iter().flatten() {
        if !channel_ids.contains(&id) {
            channel_ids.push(id);
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

        let companion_main = if header.starts_with(OIR_IDENTIFIER) {
            None
        } else {
            let (main, _, _) = oir_resolve_main_and_companions(path);
            if main != path {
                Some(main)
            } else {
                None
            }
        };

        if header.starts_with(OIR_IDENTIFIER) || companion_main.is_some() {
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
            self.state = Some(OirState::Tiff(Box::new(tiff), meta, path.to_path_buf()));
            return Ok(());
        }

        Err(BioFormatsError::UnsupportedFormat(
            "Olympus OIR file is neither OLYMPUSRAWFORMAT nor a TIFF export".into(),
        ))
    }

    fn close(&mut self) -> Result<()> {
        if let Some(OirState::Tiff(tiff, _, _)) = &mut self.state {
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
            Some(OirState::Tiff(_, meta, _)) => meta,
            None => crate::common::reader::uninitialized_metadata(),
        }
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        match &mut self.state {
            Some(OirState::Native(n)) => oir_open_plane(n, plane_index),
            Some(OirState::Tiff(tiff, _, _)) => {
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
            Some(OirState::Tiff(tiff, _, _)) => {
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
    /// Stage origin position (micrometres) from RWC_FRAME_ORIGIN
    /// (CellSensReader.java:1859-1863).
    origin_x: Option<f64>,
    origin_y: Option<f64>,
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
    /// Generic named-tag original metadata: `(tagPrefix + getTagName(tag))`
    /// keyed raw string values, mirroring Java's
    /// `addMetaList(tagPrefix + tagName, value, ...)` /
    /// `addGlobalMetaList(...)` (CellSensReader.java:1995-2002). Kept as an
    /// ordered list so repeated keys accumulate (like Java's Vector-valued
    /// metadata entries). Surfaced alongside the typed `cellsens.ets.*` keys.
    named_tags: Vec<(String, String)>,
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
        let mut reader = BufReader::new(File::open(&self.path).map_err(BioFormatsError::Io)?);
        let file_len = reader
            .seek(std::io::SeekFrom::End(0))
            .map_err(BioFormatsError::Io)?;
        let read_len = match self.compression {
            // Java CellSensReader.decodeTile allocates tileSize bytes for RAW
            // and JPEG-family codecs, ignoring the chunk-table nBytes field.
            ETS_RAW | ETS_JPEG | ETS_JPEG_2000 | ETS_JPEG_LOSSLESS => tile_size,
            ETS_PNG | ETS_BMP => self
                .tiles
                .get(index + 1)
                .map(|(_, next_offset, _)| next_offset.saturating_sub(offset) as usize)
                .unwrap_or_else(|| file_len.saturating_sub(offset) as usize),
            _ => n_bytes as usize,
        };
        let available = file_len.saturating_sub(offset).min(read_len as u64) as usize;
        let mut raw = read_bytes_at(&mut reader, offset, available)?;
        if raw.len() < read_len {
            raw.resize(read_len, 0);
        }

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

        if buf.len() < tile_size {
            buf.resize(tile_size, 0);
        } else if buf.len() > tile_size {
            buf.truncate(tile_size);
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

    /// Assemble only a rectangular region from the ETS tile grid. This mirrors
    /// the same tile-origin/intersection math as `assemble_plane`, but copies
    /// directly into the requested output rectangle instead of materialising the
    /// full image. Missing tiles still decode through `decode_tile`, preserving
    /// Java's background-fill behavior.
    fn assemble_region(
        &self,
        resolution: usize,
        z: i32,
        c: i32,
        t: i32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let level = self
            .levels
            .get(resolution)
            .ok_or(BioFormatsError::PlaneOutOfRange(0))?;
        let x2 = x
            .checked_add(w)
            .ok_or_else(|| BioFormatsError::Format("cellSens ETS region width overflows".into()))?;
        let y2 = y.checked_add(h).ok_or_else(|| {
            BioFormatsError::Format("cellSens ETS region height overflows".into())
        })?;
        if x2 > level.size_x || y2 > level.size_y {
            return Err(BioFormatsError::Format(
                "cellSens ETS region is outside image bounds".into(),
            ));
        }

        let bpp = self.pixel_type()?.bytes_per_sample();
        let channels = self.rgb_channels() as usize;
        let pixel = bpp * channels;
        let out_w = w as usize;
        let out_h = h as usize;
        let out_row_len = out_w * pixel;
        let mut out = vec![0u8; out_row_len * out_h];

        if w == 0 || h == 0 {
            return Ok(out);
        }

        let tile_w = self.tile_x as i64;
        let tile_h = self.tile_y as i64;
        let img = (0i64, 0i64, level.size_x as i64, level.size_y as i64);
        let req = (x as i64, y as i64, w as i64, h as i64);
        let res_scale = 1i64 << resolution;
        let origin_x = self.tile_origin_x.map_or(0, |v| v as i64) / res_scale;
        let origin_y = self.tile_origin_y.map_or(0, |v| v as i64) / res_scale;

        let mut output_row: usize = 0;
        let mut output_col: usize = 0;
        for row in 0..level.rows {
            let mut last_height: Option<i64> = None;
            for col in 0..level.cols {
                let tx = col as i64 * tile_w + origin_x;
                let ty = row as i64 * tile_h + origin_y;
                let ix0 = tx.max(img.0);
                let iy0 = ty.max(img.1);
                let ix1 = (tx + tile_w).min(img.0 + img.2);
                let iy1 = (ty + tile_h).min(img.1 + img.3);
                if ix1 <= ix0 || iy1 <= iy0 {
                    continue;
                }

                let inter_w = ix1 - ix0;
                let inter_h = iy1 - iy0;
                let dst_full_x0 = (output_col / pixel) as i64;
                let dst_full_y0 = output_row as i64;
                let dst_full_x1 = dst_full_x0 + inter_w;
                let dst_full_y1 = dst_full_y0 + inter_h;
                let ox0 = dst_full_x0.max(req.0);
                let oy0 = dst_full_y0.max(req.1);
                let ox1 = dst_full_x1.min(req.0 + req.2);
                let oy1 = dst_full_y1.min(req.1 + req.3);
                if ox1 > ox0 && oy1 > oy0 {
                    let tile = self.decode_tile(resolution, row as i32, col as i32, z, c, t)?;
                    let src_inter_x = if tx < img.0 { (img.0 - tx) as usize } else { 0 };
                    let src_inter_y = (iy0 - ty) as usize;
                    let src_x = src_inter_x + (ox0 - dst_full_x0) as usize;
                    let src_y = src_inter_y + (oy0 - dst_full_y0) as usize;
                    let dst_x = (ox0 - req.0) as usize;
                    let dst_y = (oy0 - req.1) as usize;
                    let copy_w = (ox1 - ox0) as usize;
                    let copy_len = copy_w * pixel;
                    let src_stride = self.tile_x as usize * pixel;
                    for copy_row in 0..(oy1 - oy0) as usize {
                        let src = (src_y + copy_row) * src_stride + src_x * pixel;
                        let dst = (dst_y + copy_row) * out_row_len + dst_x * pixel;
                        if src + copy_len <= tile.len() && dst + copy_len <= out.len() {
                            out[dst..dst + copy_len].copy_from_slice(&tile[src..src + copy_len]);
                        }
                    }
                }
                output_col += pixel * inter_w.min(tile_w) as usize;
                last_height = Some(inter_h);
            }
            if let Some(height) = last_height {
                output_row += height as usize;
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
    let floats: [(&str, Option<f64>); 14] = [
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
        // Stage origin position (CellSensReader.java:1859-1863).
        ("frame_origin_x", m.origin_x),
        ("frame_origin_y", m.origin_y),
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

    // Generic named-tag original metadata, the `tagPrefix + getTagName(tag)`
    // string keys Java records via addMetaList/addGlobalMetaList
    // (CellSensReader.java:1995-2002). Repeated keys accumulate into a Vector in
    // Java; here we surface a single occurrence under `{prefix}.tag.{name}` and
    // any further occurrences under `{prefix}.tag.{name}.{idx}` (matching how the
    // other list-valued metadata is surfaced).
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for (key, _) in &m.named_tags {
        *counts.entry(key.as_str()).or_insert(0) += 1;
    }
    let mut seen: HashMap<&str, usize> = HashMap::new();
    for (key, value) in &m.named_tags {
        let total = counts.get(key.as_str()).copied().unwrap_or(1);
        let full = if total > 1 {
            let i = seen.entry(key.as_str()).or_insert(0);
            let out = format!("{prefix}.tag.{key}.{i}");
            *i += 1;
            out
        } else {
            format!("{prefix}.tag.{key}")
        };
        sm.insert(full, MetadataValue::String(value.clone()));
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
// RWC_FRAME_ORIGIN: stage origin position (doubleValues[0]/[1]) in micrometres
// (CellSensReader.java:300, 1859-1863). Numerically equal to
// EXTERNAL_FILE_PROPERTIES (2018); the two are disambiguated by context exactly
// as in Java (the metadata-index bump checks the tag against IMAGE_FRAME_VOLUME,
// while the origin capture lives in the DOUBLE-array leaf switch).
const VSI_RWC_FRAME_ORIGIN: i32 = 2018;
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

    fn read_tags_inner(&mut self, container_fp: i64, populate: bool, tag_prefix: &str) -> i64 {
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

                // Generic named-tag original metadata, mirroring Java's
                // addMetaList/addGlobalMetaList(tagPrefix + getTagName(tag), value)
                // (CellSensReader.java:1995-2002). Only emitted when the tag has a
                // known name and metadata population is enabled. The
                // metadataIndex >= 0 (per-pyramid `addMetaList`) branch is the one
                // surfaced into series_metadata; the global branch (< 0) gates out
                // the bare VALUE tag exactly like Java.
                if populate {
                    if let (Some(name), Some(v)) = (cellsens_tag_name(tag), &value) {
                        let key = format!("{tag_prefix}{name}");
                        if self.metadata_index >= 0 {
                            let idx = self.metadata_index as usize;
                            self.pyramids[idx].meta.named_tags.push((key, v.clone()));
                        }
                        // else: global metadata (tag != VALUE || prefix non-empty);
                        // the per-ETS reader has no global series store, so unlike
                        // Java these are not surfaced here.
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
            // DEVICE_SUBTYPE is translated to a label before storage, mirroring
            // Java's `getDeviceSubtype(value)` then `pyramid.deviceTypes.add(value)`
            // (CellSensReader.java:1886-1889).
            VSI_DEVICE_SUBTYPE => m.device_subtypes.push(get_device_subtype(v)),
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
            // STACK_TYPE is translated to a label before storage, mirroring Java's
            // `value = getStackType(value)` (CellSensReader.java:1883-1884).
            VSI_STACK_TYPE => m.stack_type = Some(get_stack_type(v)),
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
                } else if tag == VSI_RWC_FRAME_ORIGIN && vals.len() >= 2 && self.metadata_index >= 0
                {
                    // Stage origin position (CellSensReader.java:1859-1863). Only the
                    // first value wins, mirroring Java's `pyramid.originX == null` guard.
                    let m = &mut self.pyramids[self.metadata_index as usize].meta;
                    if m.origin_x.is_none() {
                        m.origin_x = Some(vals[0]);
                        m.origin_y = Some(vals[1]);
                    }
                }
                Some(format!("{vals:?}"))
            }
            _ => None,
        }
    }
}

// Stack-type enum values (CellSensReader.java:315-323), translated by
// `get_stack_type`.
const VSI_STACK_DEFAULT_IMAGE: i64 = 0;
const VSI_STACK_OVERVIEW_IMAGE: i64 = 1;
const VSI_STACK_SAMPLE_MASK: i64 = 2;
const VSI_STACK_FOCUS_IMAGE: i64 = 4;
const VSI_STACK_EFI_SHARPNESS_MAP: i64 = 8;
const VSI_STACK_EFI_HEIGHT_MAP: i64 = 16;
const VSI_STACK_EFI_TEXTURE_MAP: i64 = 32;
const VSI_STACK_EFI_STACK: i64 = 64;
const VSI_STACK_MACRO_IMAGE: i64 = 256;

/// Translate a STACK_TYPE numeric code to a human-readable label. Mirrors
/// `CellSensReader.getStackType` (CellSensReader.java:2564-2587). An unrecognised
/// (or non-numeric) value passes through unchanged, exactly like Java.
fn get_stack_type(value: &str) -> String {
    match value.trim().parse::<i64>() {
        Ok(VSI_STACK_DEFAULT_IMAGE) => "Default image".to_string(),
        Ok(VSI_STACK_OVERVIEW_IMAGE) => "Overview image".to_string(),
        Ok(VSI_STACK_SAMPLE_MASK) => "Sample mask".to_string(),
        Ok(VSI_STACK_FOCUS_IMAGE) => "Focus image".to_string(),
        Ok(VSI_STACK_EFI_SHARPNESS_MAP) => "EFI sharpness map".to_string(),
        Ok(VSI_STACK_EFI_HEIGHT_MAP) => "EFI height map".to_string(),
        Ok(VSI_STACK_EFI_TEXTURE_MAP) => "EFI texture map".to_string(),
        Ok(VSI_STACK_EFI_STACK) => "EFI stack".to_string(),
        Ok(VSI_STACK_MACRO_IMAGE) => "Macro image".to_string(),
        _ => value.to_string(),
    }
}

/// Translate a DEVICE_SUBTYPE numeric code to a human-readable label. Mirrors
/// `CellSensReader.getDeviceSubtype` (CellSensReader.java:2519-2562). An
/// unrecognised (or non-numeric) value passes through unchanged, like Java.
fn get_device_subtype(value: &str) -> String {
    match value.trim().parse::<i64>() {
        Ok(0) => "Camera".to_string(),
        Ok(10000) => "Stage".to_string(),
        Ok(20000) => "Objective revolver".to_string(),
        Ok(20001) => "TV Adapter".to_string(),
        Ok(20002) => "Filter Wheel".to_string(),
        Ok(20003) => "Lamp".to_string(),
        Ok(20004) => "Aperture Stop".to_string(),
        Ok(20005) => "Shutter".to_string(),
        Ok(20006) => "Objective".to_string(),
        Ok(20007) => "Objective Changer".to_string(),
        Ok(20008) => "TopLens".to_string(),
        Ok(20009) => "Prism".to_string(),
        Ok(20010) => "Zoom".to_string(),
        Ok(20011) => "DSU".to_string(),
        Ok(20012) => "ZDC".to_string(),
        Ok(20050) => "Stage Insert".to_string(),
        Ok(30000) => "Slide Loader".to_string(),
        Ok(40000) => "Manual Control".to_string(),
        Ok(40500) => "Microscope Frame".to_string(),
        _ => value.to_string(),
    }
}

/// Map a numeric VSI/ETS tag ID to its human-readable name. Direct port of
/// `CellSensReader.getTagName` (CellSensReader.java:2110-2517): the big switch
/// mapping tag IDs to the strings Java records under `tagPrefix + tagName`.
/// Returns `None` for unhandled tags, exactly like Java (which returns null and
/// logs "Unhandled tag").
fn cellsens_tag_name(tag: i32) -> Option<&'static str> {
    match tag {
        2063 => Some("Image plane rectangle unit (Y dimension)"), // Y_PLANE_DIMENSION_UNIT
        2064 => Some("Y dimension unit"),                         // Y_DIMENSION_UNIT
        2073 => Some("Channel under/overflow"),                   // CHANNEL_OVERFLOW
        2055 => Some("Specimen"),                                 // SLIDE_SPECIMEN
        2057 => Some("Tissue"),                                   // SLIDE_TISSUE
        2058 => Some("Preparation"),                              // SLIDE_PREPARATION
        2059 => Some("Staining"),                                 // SLIDE_STAINING
        2060 => Some("Slide Info"),                               // SLIDE_INFO
        2061 => Some("Slide Name"),                               // SLIDE_NAME
        100002 => Some("Exposure time (microseconds)"),           // EXPOSURE_TIME
        100003 => Some("Camera gain"),                            // CAMERA_GAIN
        100004 => Some("Camera offset"),                          // CAMERA_OFFSET
        100005 => Some("Gamma"),                                  // CAMERA_GAMMA
        100006 => Some("Sharpness"),                              // SHARPNESS
        100007 => Some("Red channel gain"),                       // RED_GAIN
        100008 => Some("Green channel gain"),                     // GREEN_GAIN
        100009 => Some("Blue channel gain"),                      // BLUE_GAIN
        100010 => Some("Red channel offset"),                     // RED_OFFSET
        100011 => Some("Green channel offset"),                   // GREEN_OFFSET
        100012 => Some("Blue channel offset"),                    // BLUE_OFFSET
        100013 => Some("Shading sub"),                            // SHADING_SUB
        100014 => Some("Shading mul"),                            // SHADING_MUL
        100015 => Some("Binning (X)"),                            // X_BINNING
        100016 => Some("Binning (Y)"),                            // Y_BINNING
        100017 => Some("Clipping"),                               // CLIPPING
        100023 => Some("Mirror (horizontal)"),                    // MIRROR_H
        100024 => Some("Mirror (vertical)"),                      // MIRROR_V
        100025 => Some("Clipping state"),                         // CLIPPING_STATE
        100030 => Some("ICC enabled"),                            // ICC_ENABLED
        100031 => Some("Brightness"),                             // BRIGHTNESS
        100032 => Some("Contrast"),                               // CONTRAST
        100033 => Some("Contrast reference"),                     // CONTRAST_TARGET
        100034 => Some("Camera accumulation"),                    // ACCUMULATION
        100035 => Some("Camera averaging"),                       // AVERAGING
        100038 => Some("ISO sensitivity"),                        // ISO_SENSITIVITY
        100039 => Some("Camera accumulation mode"),               // ACCUMULATION_MODE
        100043 => Some("Autoexposure enabled"),                   // AUTOEXPOSURE
        100044 => Some("Autoexposure metering mode"),             // EXPOSURE_METERING_MODE
        2012 => Some("Z stack start"),                            // Z_START
        2013 => Some("Z stack increment"),                        // Z_INCREMENT
        2014 => Some("Z position"),                               // Z_VALUE
        2100 => Some("Timelapse start"),                          // TIME_START
        2016 => Some("Timelapse increment"),                      // TIME_INCREMENT
        2017 => Some("Timestamp"),                                // TIME_VALUE
        2039 => Some("Lambda start"),                             // LAMBDA_START
        2040 => Some("Lambda increment"),                         // LAMBDA_INCREMENT
        2041 => Some("Lambda value"),                             // LAMBDA_VALUE
        2021 => Some("Dimension name"),                           // DIMENSION_NAME
        2023 => Some("Dimension description"),                    // DIMENSION_MEANING
        2025 => Some("Dimension start ID"),                       // DIMENSION_START_ID
        2026 => Some("Dimension increment ID"),                   // DIMENSION_INCREMENT_ID
        2027 => Some("Dimension value ID"),                       // DIMENSION_VALUE_ID
        2053 => Some("Image size"),                               // IMAGE_BOUNDARY
        20004 => Some("Tile system"),                             // TILE_SYSTEM
        20005 => Some("External file present"),                   // HAS_EXTERNAL_FILE
        20025 => Some("External file volume"),                    // EXTERNAL_DATA_VOLUME
        2410 => Some("Origin of tile coordinate system"),         // TILE_ORIGIN
        2003 => Some("Display limits"),                           // DISPLAY_LIMITS
        2004 => Some("Stack display LUT"),                        // STACK_DISPLAY_LUT
        2005 => Some("Gamma correction"),                         // GAMMA_CORRECTION
        2006 => Some("Frame origin (plane coordinates)"),         // FRAME_ORIGIN
        2007 => Some("Frame scale (plane coordinates)"),          // FRAME_SCALE
        2008 => Some("Display color"),                            // DISPLAY_COLOR
        2015 => Some("Creation time (UTC)"),                      // CREATION_TIME
        2018 => Some("Origin"),                                   // RWC_FRAME_ORIGIN
        2019 => Some("Calibration"),                              // RWC_FRAME_SCALE
        2020 => Some("Calibration units"),                        // RWC_FRAME_UNIT
        2030 => Some("Layer"),                                    // STACK_NAME
        2031 => Some("Channel dimension"),                        // CHANNEL_DIM
        2074 => Some("Image Type"),                               // STACK_TYPE
        2076 => Some("Live overflow"),                            // LIVE_OVERFLOW
        20035 => Some("IS transmission mask"),                    // IS_TRANSMISSION
        10047 => Some("Contrast and brightness"),                 // CONTRAST_BRIGHTNESS
        10048 => Some("Acquisition properties"),                  // ACQUISITION_PROPERTIES
        10065 => Some("Gradient LUT"),                            // GRADIENT_LUT
        10000 => Some("Display processor type"),                  // DISPLAY_PROCESSOR_TYPE
        10001 => Some("Render operation ID"),                     // RENDER_OPERATION_ID
        10005 => Some("Displayed stack ID"),                      // DISPLAY_STACK_ID
        10006 => Some("Transparency ID"),                         // TRANSPARENCY_ID
        10007 => Some("Display third ID"),                        // THIRD_ID
        10008 => Some("Display visible"),                         // DISPLAY_VISIBLE
        10009 => Some("Transparency value"),                      // TRANSPARENCY_VALUE
        10013 => Some("Display LUT"),                             // DISPLAY_LUT
        10014 => Some("Display stack index"),                     // DISPLAY_STACK_INDEX
        10018 => Some("Channel transparency value"),              // CHANNEL_TRANSPARENCY_VALUE
        10025 => Some("Channel visible"),                         // CHANNEL_VISIBLE
        10028 => Some("List of selected channels"),               // SELECTED_CHANNELS
        10032 => Some("Display gamma correction"),                // DISPLAY_GAMMA_CORRECTION
        10033 => Some("Channel gamma correction"),                // CHANNEL_GAMMA_CORRECTION
        10045 => Some("Display contrast and brightness"),         // DISPLAY_CONTRAST_BRIGHTNESS
        10046 => Some("Channel contrast and brightness"),         // CHANNEL_CONTRAST_BRIGHTNESS
        10049 => Some("Active stack dimension"),                  // ACTIVE_STACK_DIMENSION
        10050 => Some("Selected frames"),                         // SELECTED_FRAMES
        10054 => Some("Displayed LUT ID"),                        // DISPLAYED_LUT_ID
        10056 => Some("Hidden layer"),                            // HIDDEN_LAYER
        10057 => Some("Layer fixed in XY"),                       // LAYER_XY_FIXED
        10060 => Some("Active layer vector"),                     // ACTIVE_LAYER_VECTOR
        10061 => Some("Active layer index vector"),               // ACTIVE_LAYER_INDEX_VECTOR
        10062 => Some("Chained layers"),                          // CHAINED_LAYERS
        10063 => Some("Layer selection"),                         // LAYER_SELECTION
        10064 => Some("Layer selection index"),                   // LAYER_SELECTION_INDEX
        10066 => Some("Canvas background color 1"),               // CANVAS_COLOR_1
        10067 => Some("Canvas background color 2"),               // CANVAS_COLOR_2
        10069 => Some("Original frame rate (ms)"),                // ORIGINAL_FRAME_RATE
        10070 => Some("Use original frame rate"),                 // USE_ORIGINAL_FRAME_RATE
        10071 => Some("Active channel"),                          // ACTIVE_CHANNEL
        2011 => Some("Plane unit"),                               // PLANE_UNIT
        20006 => Some("Origin"),                                  // PLANE_ORIGIN_RWC
        20007 => Some("Physical pixel size"),                     // PLANE_SCALE_RWC
        1073741824 => Some("Original magnification"),             // MAGNIFICATION
        11 => Some("Document Name"),                              // DOCUMENT_NAME
        13 => Some("Document Note"),                              // DOCUMENT_NOTE
        14 => Some("Document Creation Time"),                     // DOCUMENT_TIME
        15 => Some("Document Author"),                            // DOCUMENT_AUTHOR
        16 => Some("Document Company"),                           // DOCUMENT_COMPANY
        17 => Some("Document creator name"),                      // DOCUMENT_CREATOR_NAME
        18 => Some("Document creator major version"),             // DOCUMENT_CREATOR_MAJOR_VERSION
        19 => Some("Document creator minor version"),             // DOCUMENT_CREATOR_MINOR_VERSION
        20 => Some("Document creator sub version"),               // DOCUMENT_CREATOR_SUB_VERSION
        21 => Some("Product Build Number"),                       // DOCUMENT_CREATOR_BUILD_NUMBER
        22 => Some("Document creator package"),                   // DOCUMENT_CREATOR_PACKAGE
        23 => Some("Document product"),                           // DOCUMENT_PRODUCT
        24 => Some("Document product name"),                      // DOCUMENT_PRODUCT_NAME
        25 => Some("Document product version"),                   // DOCUMENT_PRODUCT_VERSION
        27 => Some("Document type hint"),                         // DOCUMENT_TYPE_HINT
        28 => Some("Document thumbnail"),                         // DOCUMENT_THUMB
        2022 => Some("Coarse pyramid level"),                     // COARSE_PYRAMID_LEVEL
        2028 => Some("Extra samples"),                            // EXTRA_SAMPLES
        2034 => Some("Default background color"),                 // DEFAULT_BACKGROUND_COLOR
        2035 => Some("Version number"),                           // VERSION_NUMBER
        2419 => Some("Channel name"),                             // CHANNEL_NAME
        120060 => Some("Magnification"),                          // OBJECTIVE_MAG
        120061 => Some("Numerical Aperture"),                     // NUMERICAL_APERTURE
        120062 => Some("Objective Working Distance"),             // WORKING_DISTANCE
        120063 => Some("Objective Name"),                         // OBJECTIVE_NAME
        120064 => Some("Objective Type"),                         // OBJECTIVE_TYPE
        120065 => Some("Objective Description"),
        120066 => Some("Objective Subtype"),
        120069 => Some("Brightness Correction"),
        120070 => Some("Objective Lens"),
        120075 => Some("Objective X Shift"),
        120076 => Some("Objective Y Shift"),
        120077 => Some("Objective Z Shift"),
        120078 => Some("Objective Gear Setting"),
        120635 => Some("Slide Bar Code"),
        120638 => Some("Tray No."),
        120637 => Some("Slide No."),
        34 => Some("Product Name"),
        35 => Some("Product Version"),
        120116 => Some("Device Name"),             // DEVICE_NAME
        100049 => Some("Camera Actual Bit Depth"), // BIT_DEPTH
        120001 => Some("Device Position"),
        120050 => Some("TV Adapter Magnification"),
        120079 => Some("Objective Refractive Index"), // REFRACTIVE_INDEX
        120117 => Some("Device Type"),
        120129 => Some("Device Unit ID"), // DEVICE_ID
        120130 => Some("Device Subtype"), // DEVICE_SUBTYPE
        120132 => Some("Device Model"),
        120133 => Some("Device Manufacturer"), // DEVICE_MANUFACTURER
        121102 => Some("Stage Insert Position"),
        121131 => Some("Laser/Lamp Intensity"),
        268435456 => Some("Units"),
        268435458 => Some("Value"), // VALUE
        175208 => Some("Snapshot Count"),
        175209 => Some("Scanning Time (seconds)"),
        120210 => Some("Device Configuration Position"),
        120211 => Some("Device Configuration Index"),
        124000 => Some("Aperture Max Mode"),
        100048 => Some("Camera Maximum Frame Size"), // FRAME_SIZE
        100055 => Some("Camera HDRI Enabled"),       // HDRI_ON
        100056 => Some("Camera Images per HDRI image"), // HDRI_FRAMES
        100057 => Some("Camera HDRI Exposure Ratio"), // HDRI_EXPOSURE_RANGE
        100058 => Some("Camera HDRI Mapping Mode"),  // HDRI_MAP_MODE
        100059 => Some("Camera Custom Grayscale Value"), // CUSTOM_GRAYSCALE
        100060 => Some("Camera Saturation"),         // SATURATION
        100061 => Some("Camera White Balance Preset ID"), // WB_PRESET_ID
        100062 => Some("Camera White Balance Preset Name"), // WB_PRESET_NAME
        100063 => Some("Camera White Balance Mode"), // WB_MODE
        100064 => Some("Camera CCD Sensitivity"),    // CCD_SENSITIVITY
        100065 => Some("Camera Enhanced Dynamic Range"), // ENHANCED_DYNAMIC_RANGE
        100066 => Some("Camera Pixel Clock (MHz)"),  // PIXEL_CLOCK
        100067 => Some("Camera Colorspace"),         // COLORSPACE
        100068 => Some("Camera Cooling Enabled"),    // COOLING_ON
        100069 => Some("Camera Cooling Fan Speed"),  // FAN_SPEED
        100070 => Some("Camera Cooling Temperature Target"), // TEMPERATURE_TARGET
        100071 => Some("Camera Gain Unit"),          // GAIN_UNIT
        100072 => Some("Camera EM Gain"),            // EM_GAIN
        100073 => Some("Camera Photon Imaging Mode"), // PHOTON_IMAGING_MODE
        100074 => Some("Camera Frame Transfer Enabled"), // FRAME_TRANSFER
        100075 => Some("Camera iXon Shift Speed"),   // ANDOR_SHIFT_SPEED
        100076 => Some("Camera Vertical Clock Amplitude"), // VCLOCK_AMPLITUDE
        100077 => Some("Camera Spurious Noise Removal Enabled"), // SPURIOUS_NOISE_REMOVAL
        100078 => Some("Camera Signal Output"),      // SIGNAL_OUTPUT
        100079 => Some("Camera Baseline Offset Clamp"), // BASELINE_OFFSET_CLAMP
        100080 => Some("Camera DP80 Frame Centering"), // DP80_FRAME_CENTERING
        100081 => Some("Camera Hot Pixel Correction Enabled"), // HOT_PIXEL_CORRECTION
        100082 => Some("Camera Noise Reduction"),    // NOISE_REDUCTION
        100083 => Some("Camera WiDER"),              // WIDER
        100084 => Some("Camera Photobleaching Enabled"), // PHOTOBLEACHING
        100085 => Some("Camera Preamp Gain"),        // PREAMP_GAIN_VALUE
        100086 => Some("Camera WiDER Enabled"),      // WIDER_ENABLED
        _ => None,
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

    fn sibling_vsi_for_ets(ets_path: &Path) -> Option<PathBuf> {
        let stack_dir = ets_path.parent()?;
        let pixels_dir = stack_dir.parent()?;
        let pixels_name = pixels_dir.file_name()?.to_str()?;
        let stem = pixels_name.strip_prefix('_')?.strip_suffix('_')?;
        let vsi = pixels_dir.parent()?.join(format!("{stem}.vsi"));
        vsi.exists().then_some(vsi)
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
        let filename = vsi_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("image")
            .to_string();

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
        if volumes.is_empty() {
            // Java's files.size()==1 path still exposes the embedded TIFF IFDs
            // (CellSensReader.java:706-773). Do not leave a valid `.vsi`
            // initialized with zero logical series just because no ETS companions
            // were found or parsed.
            self.ets.clear();
            self.series_map.clear();
            self.series_names.clear();
            self.series_phys.clear();
            for s in 0..self.tiff_series {
                self.series_map.push(CellSensTarget::Tiff(s));
                self.series_names.push(if s == 0 {
                    filename.clone()
                } else {
                    format!("{filename} #{}", s + 1)
                });
                self.series_phys.push(None);
            }
            return;
        } else if pyramids.is_empty() {
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
        matches!(ext.as_deref(), Some("vsi") | Some("ets"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let _ = self.close();
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        if matches!(ext.as_deref(), Some("ets")) {
            if let Some(vsi) = Self::sibling_vsi_for_ets(path) {
                return self.set_id(&vsi);
            }
            let vol = Self::parse_ets(path)?;
            let filename = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("image")
                .to_string();
            self.ets.push(vol);
            for res in 0..self.ets[0].levels.len() {
                self.series_map.push(CellSensTarget::Ets {
                    volume: 0,
                    resolution: res,
                });
                self.series_names.push(if res == 0 {
                    filename.clone()
                } else {
                    format!("{filename} #{}", res + 1)
                });
                self.series_phys.push(None);
            }
            if !self.series_map.is_empty() {
                let _ = self.set_series(0);
            }
            return Ok(());
        }
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
            CellSensTarget::Ets { volume, resolution } => {
                let vol = &self.ets[volume];
                let level = vol
                    .levels
                    .get(resolution)
                    .ok_or(BioFormatsError::PlaneOutOfRange(p))?;
                let n_c = (level.size_c / vol.rgb_channels().max(1)).max(1);
                let n_z = level.size_z.max(1);
                let count = n_c * n_z * level.size_t.max(1);
                if p >= count {
                    return Err(BioFormatsError::PlaneOutOfRange(p));
                }
                let c = (p % n_c) as i32;
                let z = ((p / n_c) % n_z) as i32;
                let t = (p / (n_c * n_z)) as i32;
                vol.assemble_region(resolution, z, c, t, x, y, w, h)
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
// 12. GE MicroCT VFF
// ---------------------------------------------------------------------------
/// GE MicroCT VFF reader (`.vff`).
///
/// Public compatibility wrapper for the Java Bio-Formats `MicroCTReader`.
/// The faithful implementation lives in `formats::bruker::MicroCtVffReader`.
pub struct MicroCtReader {
    inner: crate::formats::bruker::MicroCtVffReader,
}

impl MicroCtReader {
    pub fn new() -> Self {
        MicroCtReader {
            inner: crate::formats::bruker::MicroCtVffReader::new(),
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
        self.inner.is_this_type_by_name(path)
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
    fn resolution_count(&self) -> usize {
        self.inner.resolution_count()
    }
    fn set_resolution(&mut self, level: usize) -> Result<()> {
        self.inner.set_resolution(level)
    }
}

// ---------------------------------------------------------------------------
// 13. Bio-Rad SCN
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
        // Java BioRadSCNReader sets sizeC from <channel_count>, then
        // unconditionally sets imageCount = 1.
        meta.image_count = 1;
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
        crop_full_plane("Bio-Rad SCN", &full, &meta, 1, x, y, w, h)
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

fn slidebook_tiff_matches_first_ifd(ifd: &Ifd) -> bool {
    if ifd.get_str(tag::SOFTWARE) != Some("SlideBook") {
        return false;
    }
    if ifd
        .get_str(tag::IMAGE_DESCRIPTION)
        .is_some_and(|s| !s.is_empty())
    {
        return false;
    }
    [
        SLIDEBOOK_X_POS_TAG,
        SLIDEBOOK_Y_POS_TAG,
        SLIDEBOOK_Z_POS_TAG,
        SLIDEBOOK_CHANNEL_TAG,
        SLIDEBOOK_PHYSICAL_SIZE_TAG,
        SLIDEBOOK_MAGNIFICATION_TAG,
    ]
    .iter()
    .any(|tag| ifd.get(*tag).is_some())
}

fn slidebook_tiff_matches_header(header: &[u8]) -> bool {
    if header.len() < 8 {
        return false;
    }
    let little = match &header[..2] {
        b"II" => true,
        b"MM" => false,
        _ => return false,
    };
    let u16_at = |offset: usize| -> Option<u16> {
        let bytes: [u8; 2] = header.get(offset..offset + 2)?.try_into().ok()?;
        Some(if little {
            u16::from_le_bytes(bytes)
        } else {
            u16::from_be_bytes(bytes)
        })
    };
    let u32_at = |offset: usize| -> Option<u32> {
        let bytes: [u8; 4] = header.get(offset..offset + 4)?.try_into().ok()?;
        Some(if little {
            u32::from_le_bytes(bytes)
        } else {
            u32::from_be_bytes(bytes)
        })
    };
    if u16_at(2) != Some(42) {
        return false;
    }
    let Some(ifd_offset) = u32_at(4).map(|v| v as usize) else {
        return false;
    };
    let Some(count) = u16_at(ifd_offset).map(|v| v as usize) else {
        return false;
    };
    let table_start = ifd_offset + 2;
    let mut software_ok = false;
    let mut comment_empty = true;
    let mut has_private_tag = false;

    for i in 0..count {
        let entry = table_start + i * 12;
        let Some(tag_id) = u16_at(entry) else {
            return false;
        };
        let typ = u16_at(entry + 2).unwrap_or(0);
        let value_count = u32_at(entry + 4).unwrap_or(0) as usize;
        let value_offset = entry + 8;
        let ascii_value = || -> Option<String> {
            if typ != 2 || value_count == 0 {
                return None;
            }
            let bytes = if value_count <= 4 {
                header.get(value_offset..value_offset + value_count)?
            } else {
                let off = u32_at(value_offset)? as usize;
                header.get(off..off + value_count)?
            };
            let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
            Some(String::from_utf8_lossy(&bytes[..end]).into_owned())
        };

        if tag_id == tag::SOFTWARE {
            software_ok = ascii_value().as_deref() == Some("SlideBook");
        } else if tag_id == tag::IMAGE_DESCRIPTION {
            comment_empty = ascii_value().is_none_or(|value| value.is_empty());
        } else if [
            SLIDEBOOK_X_POS_TAG,
            SLIDEBOOK_Y_POS_TAG,
            SLIDEBOOK_Z_POS_TAG,
            SLIDEBOOK_CHANNEL_TAG,
            SLIDEBOOK_PHYSICAL_SIZE_TAG,
            SLIDEBOOK_MAGNIFICATION_TAG,
        ]
        .contains(&tag_id)
        {
            has_private_tag = true;
        }
    }

    software_ok && comment_empty && has_private_tag
}

fn slidebook_tiff_matches_path(path: &Path) -> Result<bool> {
    let file = File::open(path).map_err(BioFormatsError::Io)?;
    let mut parser = TiffParser::new(file)?;
    let ifds = parser.read_ifds()?;
    Ok(ifds.first().is_some_and(slidebook_tiff_matches_first_ifd))
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
        matches!(ext.as_deref(), Some("tif") | Some("tiff"))
    }
    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        slidebook_tiff_matches_header(header)
    }
    fn set_id(&mut self, path: &Path) -> Result<()> {
        if !slidebook_tiff_matches_path(path)? {
            return Err(BioFormatsError::UnsupportedFormat(
                "SlideBook TIFF: missing SlideBook software/private tags".into(),
            ));
        }
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

// ===========================================================================
// SPCReader — Becker & Hickl SPC-Image SPC FIFO files
//
// Faithful port of
// java-bioformats/components/formats-gpl/src/loci/formats/in/SPCReader.java
//
// The dataset is a pair of similarly named files: one `.spc` (FIFO photon
// stream) and one `.set` (ASCII setup describing the SPC module and the TAC
// range/gain that fix the lifetime time base). The `.spc` stream is parsed
// one 32-bit word at a time; pixel/line/frame clock marker words carry the
// scan geometry, while photon words accumulate into a per-timebin histogram.
//
// This is a 1:1 translation: each Java method has a matching Rust fn, the
// mutable instance fields of the Java reader become struct fields, and the
// constants `TAC_RANGE`, `TAC_GAIN` and `adcResShift` are preserved.
// ===========================================================================

/// Setup file text field strings. (Java: `TAC_RANGE`.)
const SPC_TAC_RANGE: &str = "SP_TAC_R";
/// Setup file text field strings. (Java: `TAC_GAIN`.)
const SPC_TAC_GAIN: &str = "SP_TAC_G";

/// Number of bits by which the ADC value is shifted. There are 12 bits in the
/// file format so shifting by 6 bits leaves 6 bits of resolution, i.e. 64
/// timebins. (Java: `adcResShift`.)
const SPC_ADC_RES_SHIFT: i32 = 6;

/// Becker & Hickl SPC FIFO reader. (Java: `class SPCReader`.)
pub struct SpcReader {
    /// List of all files to open. (Java: `allFiles`.)
    all_files: Vec<PathBuf>,

    /// Number of time bins in lifetime histogram. (Java: `nTimebins`.)
    n_timebins: i32,

    /// Number of spectral channels. (Java: `nChannels`.)
    n_channels: i32,

    /// Re-ordered data for all the timebins in one channel at one real-time
    /// point. (Java: `Tstore` / `tstoreb`.)
    tstore: Option<Vec<u8>>,

    /// Currently stored channel. (Java: `storedChannel`.)
    stored_channel: i32,

    /// Currently stored real-time data-cube. (Java: `storedT`.)
    stored_t: i32,

    /// Current position in image. (Java: `currentPixel`, `currentLine`,
    /// `currentFrame`.)
    current_pixel: i32,
    current_line: i32,
    current_frame: i32,

    /// Buffer for reading from files. (Java: `bufLength`, `rawBuf`,
    /// `nBuffers`.)
    buf_length: i32,
    raw_buf: Vec<u8>,
    n_buffers: i32,

    /// Image size. (Java: `nLines`, `nFrames`, `nPixels`.)
    n_lines: i32,
    n_frames: i32,
    n_pixels: i32,

    /// Flag indicating that a frame clock has been detected; true until the
    /// first line clock in that frame is detected. (Java: `endOfFrameFlag`.)
    end_of_frame_flag: bool,

    /// Bits per pixel. (Java: `bpp`.)
    bpp: i32,

    /// Length in bytes of data in a single timebin. (Java: `binSize`.)
    bin_size: i32,

    /// Requested channel — photons in other channels are ignored.
    /// (Java: `channel`.)
    channel: i32,

    /// Position of each frame clock in the `.spc` file. (Java: `frameClockList`.)
    frame_clock_list: Vec<i32>,

    /// Position of the end of each frame in the `.spc` file.
    /// (Java: `endOfFrameList`.)
    end_of_frame_list: Vec<i32>,

    /// Flag to indicate single-line mode. (Java: `lineMode`.)
    line_mode: bool,

    /// `.spc` file id. (Java: `spcId`.)
    spc_id: Option<PathBuf>,

    /// Open `.spc` stream. (Java: `in`, after `reopenFile`.)
    spc_in: Option<File>,

    /// Core metadata for the single series. (Java: `core.get(0)`.)
    meta: ImageMetadata,

    /// Global metadata table. (Java: `addGlobalMeta`.)
    global_meta: HashMap<String, MetadataValue>,
}

impl SpcReader {
    /// Constructs a new SPC reader. (Java: `SPCReader()`.)
    pub fn new() -> Self {
        SpcReader {
            all_files: Vec::new(),
            n_timebins: 0,
            n_channels: 0,
            tstore: None,
            stored_channel: -1,
            stored_t: -1,
            current_pixel: 0,
            current_line: -1,
            current_frame: -1,
            buf_length: 0,
            raw_buf: Vec::new(),
            n_buffers: 0,
            n_lines: 0,
            n_frames: 0,
            n_pixels: 0,
            end_of_frame_flag: false,
            bpp: 0,
            bin_size: 0,
            channel: 0,
            frame_clock_list: Vec::new(),
            end_of_frame_list: Vec::new(),
            line_mode: false,
            spc_id: None,
            spc_in: None,
            meta: ImageMetadata::default(),
            global_meta: HashMap::new(),
        }
    }

    /// (Java: `getSeriesUsedFiles(boolean)`.)
    pub fn series_used_files(&self) -> Vec<PathBuf> {
        self.all_files.clone()
    }

    /// Re-open the `.spc` stream. (Java: `reopenFile()`.)
    fn reopen_file(&mut self) -> Result<()> {
        self.spc_in = None;
        let id = self
            .spc_id
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?;
        self.spc_in = Some(File::open(id).map_err(BioFormatsError::Io)?);
        Ok(())
    }

    /// Initialise from the dataset. (Java: `initFile(String)`.)
    fn init_file(&mut self, id: &Path) -> Result<()> {
        self.all_files = Vec::new();

        // Resolve the working directory and locate the matching .set/.spc pair.
        let tmp_file = std::fs::canonicalize(id).unwrap_or_else(|_| id.to_path_buf());
        let working_dir = tmp_file
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));

        let name = tmp_file
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| BioFormatsError::Format("SPC: invalid file name".into()))?
            .to_owned();

        // generate the name of the two matching files
        let mut set_name: Option<PathBuf> = None;
        let mut spc_name: Option<PathBuf> = None;
        if let Some(pos) = name.rfind('.') {
            let base = &name[..pos];
            let want_set = format!("{base}.set");
            let want_spc = format!("{base}.spc");
            if let Ok(entries) = std::fs::read_dir(&working_dir) {
                for entry in entries.flatten() {
                    if let Some(l) = entry.file_name().to_str() {
                        if l.eq_ignore_ascii_case(&want_set) {
                            set_name = Some(working_dir.join(l));
                        }
                        if l.eq_ignore_ascii_case(&want_spc) {
                            spc_name = Some(working_dir.join(l));
                        }
                    }
                }
            }
        }

        let set_name = set_name.ok_or_else(|| {
            BioFormatsError::Format("Failed to find a matching .set file!".into())
        })?;
        let spc_name = spc_name.ok_or_else(|| {
            BioFormatsError::Format("Failed to find a matching .spc file!".into())
        })?;

        self.frame_clock_list = Vec::new();
        self.end_of_frame_list = Vec::new();

        self.all_files.push(set_name.clone());
        self.all_files.push(spc_name.clone());

        // ---- Read info from .set file ----
        let mut set_file = File::open(&set_name).map_err(BioFormatsError::Io)?;

        spc_skip(&mut set_file, 8)?;

        let setuppos = spc_read_i32_le(&mut set_file)?;
        let setupcount = spc_read_i16_le(&mut set_file)?;
        let mut module = String::new();

        // Arbitrary length established by trial and error
        if let Ok(header) = spc_read_string(&mut set_file, 600) {
            if let Some(index) = header.find("module SPC-") {
                // Java: header.substring(index + 7, index + 14)
                let start = index + 7;
                let end = index + 14;
                if end <= header.len() {
                    module = header[start..end].to_owned();
                }
            }
        }

        if !module.eq_ignore_ascii_case("SPC-134")
            && !module.eq_ignore_ascii_case("SPC-144")
            && !module.eq_ignore_ascii_case("SPC-154")
            && !module.eq_ignore_ascii_case("SPC-830")
        {
            return Err(BioFormatsError::Format(
                "Failed to find a matching .set file!".into(),
            ));
        }

        // goto start of setup information
        spc_seek(&mut set_file, setuppos as u64)?;

        let setup = spc_read_string(&mut set_file, setupcount.max(0) as usize)?;
        drop(set_file);

        // get the tac range from the setup information
        let tac_range = self.parse_setup(SPC_TAC_RANGE, &setup)?;
        // get the tac gain from the setup information
        let tac_gain = self.parse_setup(SPC_TAC_GAIN, &setup)?;

        let time_base: f64;
        if tac_gain != 0.0 && tac_range != 0.0 {
            let mut tb = 4095.0 * tac_range / (tac_gain * 4096.0);
            // convert from s to ps
            tb *= 1.000e12;
            time_base = tb;
        } else {
            return Err(BioFormatsError::Format(
                "Failed to parse setup file!".into(),
            ));
        }

        // ---- Now read .spc file ----
        self.spc_id = Some(spc_name.clone());
        let mut spc_file = File::open(&spc_name).map_err(BioFormatsError::Io)?;

        // The first 3 bytes contain macro-time clock info; skip them.
        spc_skip(&mut spc_file, 3)?;

        // The 4th byte contains the number of routing channels in bits 3..6.
        // Bits 0..2 reserved, bit 7 = 1 ("Data invalid").
        let routing = spc_read_i8(&mut spc_file)?;

        if (routing & 0x10) != 0 {
            return Err(BioFormatsError::Format("Invalid data!".into()));
        }

        self.n_channels = ((routing as i32) & 0x78) >> 3;

        self.current_pixel = 0;
        self.current_line = -1;
        self.current_frame = -1;
        self.end_of_frame_flag = false;
        self.n_buffers = 0;

        self.buf_length = 1024;
        self.raw_buf = vec![0u8; self.buf_length as usize];
        self.n_buffers = 0;

        // Stream the whole .spc file in buf_length-sized buffers, discovering
        // geometry through the marker words.
        loop {
            let no_of_bytes = spc_read(&mut spc_file, &mut self.raw_buf)?;
            if no_of_bytes == -1 {
                break;
            }
            let mut bb = 3;
            while bb < no_of_bytes {
                let adc_l = self.raw_buf[bb as usize] as i8; // upper byte w/ ADC data
                let adc_lm = (adc_l as u8) & 0xF0; // mask out upper 4 bits

                // at this point only the various clocks are of interest
                match adc_lm {
                    0x90 => self.invalid_and_mark_init(bb),
                    // Invalid, Mark and MTOV all set. Not well documented.
                    0xd0 => self.invalid_and_mark_init(bb),
                    _ => {}
                }
                bb += 4;
            }
            self.n_buffers += 1;
        }

        self.n_timebins = (0xFFF >> SPC_ADC_RES_SHIFT) + 1;
        self.n_frames = self.current_frame - 1;

        self.add_global_meta("time bins", MetadataValue::Int(self.n_timebins as i64));
        self.add_global_meta("nChannels", MetadataValue::Int(self.n_channels as i64));
        self.add_global_meta("time base", MetadataValue::Float(time_base));

        // ---- Populate metadata ----
        let mut m = ImageMetadata::default();

        // Undocumented and possibly system specific; duplicates U.Lorenzo's
        // Matlab behaviour.
        if self.n_lines < 530 {
            self.line_mode = false;
            m.size_y = self.n_lines.max(0) as u32;
        } else {
            self.line_mode = true; // return a single line
            m.size_y = 1;
        }

        let mut max_frame_length: i32 = 0;
        let mut t = 0;
        while t < self.n_frames {
            let frame_length =
                self.end_of_frame_list[(t + 1) as usize] - self.frame_clock_list[t as usize];
            if frame_length > max_frame_length {
                max_frame_length = frame_length;
            }
            t += 1;
        }

        self.raw_buf = vec![0u8; max_frame_length.max(0) as usize];

        m.size_x = self.n_pixels.max(0) as u32;
        m.size_z = 1;
        m.size_t = (self.n_timebins * self.n_frames).max(0) as u32;
        m.size_c = self.n_channels.max(0) as u32;
        m.dimension_order = crate::common::metadata::DimensionOrder::XYZTC;
        m.pixel_type = PixelType::Uint16;
        m.bits_per_pixel = 16;
        m.is_rgb = false;
        m.is_little_endian = true;
        m.image_count = m.size_z * m.size_c * m.size_t;
        m.is_indexed = false;

        // moduloT: lifetime sub-dimension within T.
        let step = if self.n_timebins != 0 {
            time_base / self.n_timebins as f64
        } else {
            0.0
        };
        m.modulo_t = Some(ModuloAnnotation {
            parent_dimension: "T".to_owned(),
            modulo_type: "lifetime".to_owned(),
            start: 0.0,
            step,
            end: step * (self.n_timebins - 1) as f64,
            unit: "ps".to_owned(),
            labels: Vec::new(),
        });

        self.meta = m;

        // Open the .spc stream for openBytes seeks.
        self.spc_in = Some(spc_file);
        Ok(())
    }

    /// Decode marker/photon words for one frame buffer, accumulating photons
    /// into `Tstore`. (Java: `processBuffer(int)`.)
    fn process_buffer(&mut self, no_of_bytes: i32) {
        let mut bb = 3;
        while bb < no_of_bytes {
            let adc_l = self.raw_buf[bb as usize] as i8;
            let adc_lm = (adc_l as u8) & 0xF0;

            match adc_lm {
                0xA0 => {}               // gap
                0x20 => {}               // Got GAP but not invalid
                0x40 => self.photon(bb), // photon + ovfl
                0x00 => self.photon(bb), // photon
                0x80 => {}               // invalid photon
                0x90 => self.invalid_and_mark(bb),
                // Invalid, Mark and MTOV all set. Not well documented.
                0xd0 => self.invalid_and_mark(bb),
                0xC0 => {
                    // timer overflow; Java reads rawBuf[bb-3] into routLM (unused)
                    let _rout_lm = self.raw_buf[(bb - 3) as usize];
                }
                _ => {} // Unrecognised pattern
            }
            bb += 4;
        }
    }

    /// Process a marker word during `openBytes`. (Java: `invalidAndMark(int)`.)
    fn invalid_and_mark(&mut self, block_ptr: i32) {
        let rout_m = (self.raw_buf[(block_ptr - 2) as usize] as u8) & 0xf0;

        match rout_m {
            0x10 => {
                // pixel clock
                self.current_pixel += 1;
            }
            0x20 => {
                // line clock
                if self.end_of_frame_flag {
                    self.current_line = -1;
                    self.end_of_frame_flag = false;
                    self.current_frame += 1;
                }
                self.current_line += 1;
                self.current_pixel = 0;
            }
            0x40 => {
                // frame clock
                self.end_of_frame_flag = true;
            }
            0x60 => {
                // frame and line clock — shouldn't happen
            }
            _ => {
                // unknown mark
            }
        }
    }

    /// Process a marker word during the `initFile` geometry pass, recording
    /// frame-clock and end-of-frame positions. (Java: `invalidAndMarkInit(int)`.)
    fn invalid_and_mark_init(&mut self, block_ptr: i32) {
        let rout_m = (self.raw_buf[(block_ptr - 2) as usize] as u8) & 0xf0;

        match rout_m {
            0x10 => {
                // pixel clock
                self.current_pixel += 1;
            }
            0x20 => {
                // line clock
                if self.current_frame == 0 && self.current_line == 1 {
                    self.n_pixels = self.current_pixel;
                }

                if self.end_of_frame_flag {
                    self.current_line = -1;
                    self.end_of_frame_flag = false;
                    self.current_frame += 1;
                    let position = (block_ptr - 3) + (self.buf_length * self.n_buffers);
                    self.end_of_frame_list.push(position);
                }

                self.current_line += 1;
                self.current_pixel = 0;
            }
            0x40 => {
                // frame clock
                if self.current_frame == 0 {
                    self.n_lines = self.current_line + 1;
                }
                // Store position of start of word containing frame clock.
                let position = (block_ptr - 3) + (self.buf_length * self.n_buffers);
                self.frame_clock_list.push(position);

                self.end_of_frame_flag = true;
            }
            0x60 => {
                // frame and line clock — shouldn't happen
            }
            _ => {
                // unknown mark
            }
        }
    }

    /// Accumulate one photon into the per-timebin histogram. (Java: `photon(int)`.)
    fn photon(&mut self, block_ptr: i32) {
        let current_channel = ((self.raw_buf[(block_ptr - 2) as usize] as u8 & 0xF0) >> 4) as i32;

        if current_channel == self.channel || self.n_channels == 1 {
            if self.current_pixel < self.n_pixels
                && self.current_line > -1
                && self.current_line < (self.n_lines + 1)
            {
                let pix = self.bpp * ((self.current_line * self.n_pixels) + self.current_pixel);

                // 4 bottom bits are 4 MSBs of 12-bit ADC.
                let mut adc_m = ((self.raw_buf[block_ptr as usize] as i32) & 0x0F) << 8;
                adc_m |= (self.raw_buf[(block_ptr - 1) as usize] as i32) & 0x0FF;
                let micro_time = 4095 - adc_m;
                let current_bin = micro_time >> SPC_ADC_RES_SHIFT;
                let pix = pix + current_bin * self.bin_size;

                if let Some(ref mut tstore) = self.tstore {
                    let idx = pix as usize;
                    if idx + 1 < tstore.len() {
                        let mut intensity = u16::from_le_bytes([tstore[idx], tstore[idx + 1]]);
                        intensity = intensity.wrapping_add(1);
                        let b = intensity.to_le_bytes();
                        tstore[idx] = b[0];
                        tstore[idx + 1] = b[1];
                    }
                }
            }
        }
    }

    /// Parse one tagged field from the `.set` setup text. (Java: `parseSetup`.)
    fn parse_setup(&self, tag: &str, setup: &str) -> Result<f64> {
        // Fields in setup text consist of a tag, followed by a type ("I" or
        // "F") followed by a text value, e.g. #SP [SP_TAC_G,I,4]
        let tag_offset = setup
            .find(tag)
            .ok_or_else(|| BioFormatsError::Format("Failed to parse setup file!".into()))?;
        let end = (tag_offset + 30).min(setup.len());
        let tagged_string = &setup[tag_offset..end];
        let comma = tagged_string
            .find(',')
            .ok_or_else(|| BioFormatsError::Format("Failed to parse setup file!".into()))?;
        let tag_type = &tagged_string[comma + 1..comma + 2];
        let close = tagged_string
            .find(']')
            .ok_or_else(|| BioFormatsError::Format("Failed to parse setup file!".into()))?;
        let value_txt = &tagged_string[comma + 3..close];
        let mut value = 0.0_f64;
        if tag_type == "I" {
            value = value_txt
                .trim()
                .parse::<i64>()
                .map_err(|_| BioFormatsError::Format("Failed to parse setup file!".into()))?
                as f64;
        }
        if tag_type == "F" {
            value = value_txt
                .trim()
                .parse::<f64>()
                .map_err(|_| BioFormatsError::Format("Failed to parse setup file!".into()))?;
        }
        Ok(value)
    }

    /// (Java: `addGlobalMeta`.)
    fn add_global_meta(&mut self, key: &str, value: MetadataValue) {
        self.global_meta.insert(key.to_owned(), value);
    }
}

impl Default for SpcReader {
    fn default() -> Self {
        Self::new()
    }
}

// -- Little-endian stream helpers mirroring RandomAccessInputStream(order=true).

fn spc_skip(f: &mut File, n: u64) -> Result<()> {
    use std::io::Seek;
    f.seek(std::io::SeekFrom::Current(n as i64))
        .map_err(BioFormatsError::Io)?;
    Ok(())
}

fn spc_seek(f: &mut File, pos: u64) -> Result<()> {
    use std::io::Seek;
    f.seek(std::io::SeekFrom::Start(pos))
        .map_err(BioFormatsError::Io)?;
    Ok(())
}

fn spc_read_i32_le(f: &mut File) -> Result<i32> {
    let mut b = [0u8; 4];
    f.read_exact(&mut b).map_err(BioFormatsError::Io)?;
    Ok(i32::from_le_bytes(b))
}

fn spc_read_i16_le(f: &mut File) -> Result<i16> {
    let mut b = [0u8; 2];
    f.read_exact(&mut b).map_err(BioFormatsError::Io)?;
    Ok(i16::from_le_bytes(b))
}

fn spc_read_i8(f: &mut File) -> Result<i8> {
    let mut b = [0u8; 1];
    f.read_exact(&mut b).map_err(BioFormatsError::Io)?;
    Ok(b[0] as i8)
}

/// Read up to `len` bytes as an ASCII string. (Java: `readString(int)`.)
fn spc_read_string(f: &mut File, len: usize) -> Result<String> {
    let mut b = vec![0u8; len];
    let n = f.read(&mut b).map_err(BioFormatsError::Io)?;
    b.truncate(n);
    Ok(String::from_utf8_lossy(&b).into_owned())
}

/// Read into `buf`, returning the number of bytes read, or -1 at EOF.
/// (Java: `RandomAccessInputStream.read(byte[])`.)
fn spc_read(f: &mut File, buf: &mut [u8]) -> Result<i32> {
    let mut filled = 0usize;
    while filled < buf.len() {
        let n = f.read(&mut buf[filled..]).map_err(BioFormatsError::Io)?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    if filled == 0 {
        Ok(-1)
    } else {
        Ok(filled as i32)
    }
}

impl FormatReader for SpcReader {
    /// (Java: `isThisType(String, boolean)`.)
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        if !matches!(ext.as_deref(), Some("spc") | Some("set")) {
            return false;
        }
        // Java requires both base.spc and base.set to exist.
        let base = match (path.parent(), path.file_stem().and_then(|s| s.to_str())) {
            (Some(dir), Some(stem)) => dir.join(stem),
            _ => return false,
        };
        let spc = base.with_extension("spc");
        let set = base.with_extension("set");
        spc.exists() && set.exists()
    }

    /// The SPC FIFO format has no reliable magic bytes; detection is by name
    /// plus the companion-file check above. (Java has no byte-magic check;
    /// `isThisType(byte[])` is not overridden.)
    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.init_file(path)
    }

    /// (Java: `close(boolean)`.)
    fn close(&mut self) -> Result<()> {
        self.tstore = None;
        self.stored_channel = -1;
        self.stored_t = -1;
        self.all_files = Vec::new();
        self.frame_clock_list = Vec::new();
        self.spc_in = None;
        self.spc_id = None;
        self.meta = ImageMetadata::default();
        self.global_meta = HashMap::new();
        Ok(())
    }

    fn series_count(&self) -> usize {
        1
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
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
        &self.meta
    }

    /// (Java: `openBytes(int, byte[], int, int, int, int)`.)
    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let w = self.meta.size_x;
        let h = self.meta.size_y;
        self.open_bytes_region(plane_index, 0, 0, w, h)
    }

    /// (Java: `openBytes(int, byte[], int, int, int, int)`.)
    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        if plane_index >= self.meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        crate::common::region::validate_region(
            "SPC",
            self.meta.size_x,
            self.meta.size_y,
            x,
            y,
            w,
            h,
        )?;

        let size_t = self.meta.size_t as i32;
        let mut no = plane_index as i32;

        self.channel = if size_t != 0 { no / size_t } else { 0 };
        no -= self.channel * size_t;

        let t = if self.n_timebins != 0 {
            no / self.n_timebins
        } else {
            0
        };
        no -= t * self.n_timebins;

        let timebin = no;

        self.bpp = 2;
        self.bin_size = self.n_pixels * self.n_lines * self.bpp;

        if self.tstore.is_none() {
            self.tstore = Some(vec![
                0u8;
                (self.n_pixels * self.n_lines * self.bpp * self.n_timebins).max(0)
                    as usize
            ]);
        }

        // if the pre-stored data doesn't match that requested then read it.
        if self.stored_t != t || self.stored_channel != self.channel {
            let frame_clock_pos = *self.frame_clock_list.get(t as usize).ok_or_else(|| {
                BioFormatsError::Format("SPC: frame clock index out of range".into())
            })?;
            let end_of_frame_pos =
                *self
                    .end_of_frame_list
                    .get((t + 1) as usize)
                    .ok_or_else(|| {
                        BioFormatsError::Format("SPC: end-of-frame index out of range".into())
                    })?;

            let frame_length = end_of_frame_pos - frame_clock_pos;

            // Clear and size the histogram buffer for this frame.
            if let Some(ref mut tstore) = self.tstore {
                for b in tstore.iter_mut() {
                    *b = 0;
                }
            }

            self.reopen_file()?;
            let no_of_bytes = {
                let f = self
                    .spc_in
                    .as_mut()
                    .ok_or(BioFormatsError::NotInitialized)?;
                spc_seek(f, frame_clock_pos as u64)?;
                if self.raw_buf.len() < frame_length.max(0) as usize {
                    self.raw_buf = vec![0u8; frame_length.max(0) as usize];
                }
                let mut tmp = vec![0u8; frame_length.max(0) as usize];
                let n = spc_read(f, &mut tmp)?;
                if n > 0 {
                    self.raw_buf[..n as usize].copy_from_slice(&tmp[..n as usize]);
                }
                n
            };

            if no_of_bytes == frame_length {
                self.current_line = -1;
                self.current_frame = -1;
                self.end_of_frame_flag = false;

                self.process_buffer(no_of_bytes);
                self.stored_t = t;
                self.stored_channel = self.channel;
            }
        }

        // copy 2D plane from Tstore into buf
        let i_line_size = self.n_pixels * self.bpp;
        let o_line_size = (w as i32) * self.bpp;

        let tstore = self
            .tstore
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?;
        let mut buf = vec![0u8; (h as usize) * (o_line_size as usize)];

        if !self.line_mode {
            // image Mode
            let mut input =
                (self.bin_size * timebin) + (y as i32 * i_line_size) + (x as i32 * self.bpp);
            let mut output = 0i32;
            for _line in 0..h as i32 {
                let src = input as usize;
                let dst = output as usize;
                if src + o_line_size as usize <= tstore.len() {
                    buf[dst..dst + o_line_size as usize]
                        .copy_from_slice(&tstore[src..src + o_line_size as usize]);
                }
                input += i_line_size;
                output += o_line_size;
            }
        } else {
            // line Mode: copy first line, then sum all other lines.
            let mut input = (self.bin_size * timebin) + (x as i32 * self.bpp);
            let output = 0i32;
            let src = input as usize;
            if src + o_line_size as usize <= tstore.len() {
                buf[output as usize..output as usize + o_line_size as usize]
                    .copy_from_slice(&tstore[src..src + o_line_size as usize]);
            }
            input += i_line_size;
            for _line in 1..self.n_lines {
                let mut p = 0i32;
                while p < o_line_size {
                    let out_idx = (output + p) as usize;
                    let in_idx = (input + p) as usize;
                    if out_idx + 1 < buf.len() && in_idx + 1 < tstore.len() {
                        let s = i16::from_le_bytes([buf[out_idx], buf[out_idx + 1]]);
                        let ts = i16::from_le_bytes([tstore[in_idx], tstore[in_idx + 1]]);
                        let sum = s.wrapping_add(ts);
                        let b = sum.to_le_bytes();
                        buf[out_idx] = b[0];
                        buf[out_idx + 1] = b[1];
                    }
                    p += 2;
                }
                input += i_line_size;
            }
        }

        Ok(buf)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let tw = self.meta.size_x.min(256);
        let th = self.meta.size_y.min(256);
        let tx = (self.meta.size_x - tw) / 2;
        let ty = (self.meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn oir_axis_text_accumulates_entity_split_chunks_like_dom() {
        let xml = r#"<imageProperties>
  <commonimage:dimensionAxis>
    <commonparam:axis>LAM&#66;DA</commonparam:axis>
    <commonparam:maxSize>3</commonparam:maxSize>
  </commonimage:dimensionAxis>
</imageProperties>"#;
        let mut meta = ImageMetadata::default();
        meta.size_c = 1;

        oir_apply_axes(xml, &mut meta);

        assert_eq!(meta.size_c, 3);
    }

    #[test]
    fn oir_channels_respect_java_order_attributes() {
        let xml = r#"<imageProperties>
  <commonphase:channel id="c2" order="2"><commonphase:name>second</commonphase:name></commonphase:channel>
  <commonphase:channel id="c1" order="1"><commonphase:name>first</commonphase:name></commonphase:channel>
  <commonphase:channel id="rgb" order="3">
    <commonphase:elementChannel id="blue" order="3"/>
    <commonphase:elementChannel id="red" order="1"/>
    <commonphase:elementChannel id="green" order="2"/>
  </commonphase:channel>
</imageProperties>"#;
        let mut channel_ids = Vec::new();

        oir_apply_channels(xml, &mut channel_ids);

        assert_eq!(channel_ids, vec!["c1", "c2", "red", "green", "blue"]);
    }

    #[test]
    fn oir_dimension_order_follows_java_basename_zt_rule() {
        assert_eq!(
            oir_dimension_order("run_t001_z001", 2, 3),
            crate::common::metadata::DimensionOrder::XYCZT
        );
        assert_eq!(
            oir_dimension_order("run_z001_t001", 2, 3),
            crate::common::metadata::DimensionOrder::XYCTZ
        );
        assert_eq!(
            oir_dimension_order("run_t001_z001", 1, 3),
            crate::common::metadata::DimensionOrder::XYCZT
        );
        assert_eq!(
            oir_dimension_order("run_z001_t001", 2, 1),
            crate::common::metadata::DimensionOrder::XYCZT
        );
    }

    fn push_oir_u32(buf: &mut Vec<u8>, value: u32) {
        buf.extend_from_slice(&value.to_le_bytes());
    }

    fn push_oir_prefix(buf: &mut Vec<u8>) {
        buf.extend_from_slice(OIR_IDENTIFIER);
        push_oir_u32(buf, 0xffff_ffff);
        push_oir_u32(buf, 0);
    }

    fn push_oir_xml_block(buf: &mut Vec<u8>, xml: &str) {
        let total = 48 + xml.len() as u32;
        push_oir_u32(buf, total);
        push_oir_u32(buf, 0);
        buf.extend(std::iter::repeat(0).take(36));
        push_oir_u32(buf, xml.len() as u32);
        buf.extend_from_slice(xml.as_bytes());
    }

    fn push_empty_oir_xml_block(buf: &mut Vec<u8>) {
        push_oir_u32(buf, 8);
        push_oir_u32(buf, 0);
    }

    fn push_oir_pixel_block(buf: &mut Vec<u8>, uid: &str, pixels: &[u8]) {
        push_oir_u32(buf, uid.len() as u32 + 12);
        push_oir_u32(buf, 3);
        buf.extend_from_slice(&[0; 8]);
        push_oir_u32(buf, uid.len() as u32);
        buf.extend_from_slice(uid.as_bytes());
        push_oir_u32(buf, pixels.len() as u32);
        push_oir_u32(buf, 0);
        buf.extend_from_slice(pixels);
    }

    fn minimal_oir_metadata_xml() -> String {
        "<?xml version=\"1.0\"?>\
         <imageProperties>\
           <frameProperties>\
             <width>2</width><height>2</height><depth>1</depth><bitCounts>8</bitCounts>\
           </frameProperties>\
           <imageInfo>\
             <channel id=\"c1\" order=\"1\"/>\
           </imageInfo>\
         </imageProperties>"
            .to_string()
    }

    #[test]
    fn oir_set_id_accepts_numbered_companion_like_java() {
        let dir = temp_flim2_path("oir-companion-dir");
        std::fs::create_dir_all(&dir).unwrap();
        let main = dir.join("sample.oir");
        let companion = dir.join("sample_00001");

        let mut main_bytes = Vec::new();
        push_oir_prefix(&mut main_bytes);
        push_oir_xml_block(&mut main_bytes, &minimal_oir_metadata_xml());
        std::fs::write(&main, main_bytes).unwrap();

        let mut companion_bytes = Vec::new();
        push_oir_prefix(&mut companion_bytes);
        push_oir_u32(&mut companion_bytes, 0xffff_ffff);
        push_oir_u32(&mut companion_bytes, 0);
        push_empty_oir_xml_block(&mut companion_bytes);
        push_oir_pixel_block(&mut companion_bytes, "z001t001_c1_0", &[1, 2, 3, 4]);
        std::fs::write(&companion, companion_bytes).unwrap();

        let mut reader = OirReader::new();
        reader.set_id(&companion).unwrap();

        assert_eq!(reader.metadata().size_x, 2);
        assert_eq!(reader.metadata().size_y, 2);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4]);
        assert_eq!(reader.series_used_files(), vec![companion.clone()]);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn slidebook7_typed_decoder_matches_java_classdecoder() {
        // Real SlideBook 7 record layout: StartClass/EndClass-delimited typed
        // classes, ClassName-gated, with count-prefixed numeric arrays.
        let yaml = "\
StartClass:
  ClassName: CImageRecord70
  mWidth: 320
  mHeight: 240
  mNumPlanes: 5
  mNumChannels: 2
  mNumTimepoints: 3
  mNumMasks: 1
  mName: Capture A
  mInfo: line1_#10;line2
  mUniqueId: abc-123
  mThumbNail: [3, 100, 200, 300]
EndClass: 0
StartClass:
  ClassName: CLensDef70
  mName: 60x Oil
  mNA: 1.4
  mMicronPerPixel: 0.1083
  mActualMagnification: 60
  mCameraName: ORCA
EndClass: 0
StartClass:
  ClassName: COptovarDef70
  mName: 1.5x
  mMagnification: 1.5
  mDefault: true
EndClass: 0
StartClass:
  ClassName: CMainViewRecord70
  mViewID: 7
  mRedChannel: 0
  mGreenChannel: 1
  mBlueChannel: 2
  mLow: [2, 10, 20]
  mHigh: [2, 4000, 4095]
  mGamma: [2, 1.0, 0.8]
EndClass: 0
";
        let node = slidebook7_yaml_compose(yaml);
        let record = Sb7ImageRecord70::decode(&node);

        assert_eq!(record.width, Some(320));
        assert_eq!(record.height, Some(240));
        assert_eq!(record.num_planes, Some(5));
        assert_eq!(record.num_channels, Some(2));
        assert_eq!(record.num_timepoints, Some(3));
        assert_eq!(record.num_masks, Some(1));
        assert_eq!(record.name.as_deref(), Some("Capture A"));
        // _#10; restored to a newline by RestoreSpecialCharacters.
        assert_eq!(record.info.as_deref(), Some("line1\nline2"));
        assert_eq!(record.unique_id.as_deref(), Some("abc-123"));
        // count prefix (3) dropped, trailing values kept.
        assert_eq!(record.thumbnail, Some(vec![100.0, 200.0, 300.0]));

        // Composed lens / optovar sub-records decoded from the chained index.
        assert_eq!(record.lens.name.as_deref(), Some("60x Oil"));
        assert_eq!(record.lens.na, Some(1.4));
        assert_eq!(record.lens.camera_name.as_deref(), Some("ORCA"));
        assert_eq!(record.optovar.magnification, Some(1.5));
        assert_eq!(record.optovar.default, Some(true));

        // Main-view numeric arrays.
        assert_eq!(record.main_view.view_id, Some(7));
        assert_eq!(record.main_view.red_channel, Some(0));
        assert_eq!(record.main_view.low, Some(vec![10.0, 20.0]));
        assert_eq!(record.main_view.high, Some(vec![4000.0, 4095.0]));
        assert_eq!(record.main_view.gamma, Some(vec![1.0, 0.8]));
    }

    #[test]
    fn slidebook7_typed_channel_records_decode_with_interleaved_manip_classes() {
        // Two channels; channel 0 is followed by manip/LUT classes that must be
        // skipped before the next CChannelRecord70 (Java LoadChannelRecord loop).
        let yaml = "\
StartClass:
  ClassName: CChannelRecord70
  mNumPlanes: 4
  mDataType: 0
EndClass: 0
StartClass:
  ClassName: CExposureRecord70
  mExposureTime: 50
EndClass: 0
StartClass:
  ClassName: CChannelDef70
  mName: DAPI
  mCameraName: ORCA
EndClass: 0
StartClass:
  ClassName: CFluorDef70
  mName: DAPI dye
  mExcitationLambda: 405
  mLambda: 461
EndClass: 0
StartClass:
  ClassName: CRemapChannelLUT70
  mManipID: 7
EndClass: 0
StartClass:
  ClassName: CHistogramRecord70
  mManipID: 8
EndClass: 0
StartClass:
  ClassName: CChannelRecord70
  mNumPlanes: 4
  mDataType: 0
EndClass: 0
StartClass:
  ClassName: CExposureRecord70
  mExposureTime: 120
EndClass: 0
StartClass:
  ClassName: CChannelDef70
  mName: FITC
EndClass: 0
StartClass:
  ClassName: CFluorDef70
  mExcitationLambda: 488
  mLambda: 525
EndClass: 0
";
        let node = slidebook7_yaml_compose(yaml);
        let (channels, extras) = slidebook7_load_channel_records(&node, 2);
        // The CRemapChannelLUT70 + CHistogramRecord70 interleaved after channel 0
        // are now captured rather than skipped.
        assert_eq!(extras.histograms.len(), 1);
        assert_eq!(extras.remap_luts.len(), 1);
        assert_eq!(channels.len(), 2);
        assert_eq!(channels[0].channel_def.name.as_deref(), Some("DAPI"));
        assert_eq!(channels[0].channel_def.camera_name.as_deref(), Some("ORCA"));
        assert_eq!(channels[0].exposure.exposure_time, Some(50));
        assert_eq!(channels[0].channel_def.fluor.excitation_lambda, Some(405.0));
        assert_eq!(channels[0].channel_def.fluor.lambda, Some(461.0));
        // Second channel decoded after the skipped manip/LUT classes.
        assert_eq!(channels[1].channel_def.name.as_deref(), Some("FITC"));
        assert_eq!(channels[1].exposure.exposure_time, Some(120));
        assert_eq!(channels[1].channel_def.fluor.lambda, Some(525.0));
    }

    #[test]
    fn slidebook7_typed_mask_records_load_like_java() {
        let yaml = "\
theNumMasks: 2
StartClass:
  ClassName: CMaskRecord70
  mName: Nucleus
  mCentroidFeature: area
  mCentroidChannel: 0
  mPersistentSubmasks: 1
EndClass: 0
StartClass:
  ClassName: CMaskRecord70
  mName: Cytoplasm
  mCentroidChannel: 1
EndClass: 0
theTimepointIndex: 0
theMaskCompressedSizes: [1, 128]
theMaskFileOffsets: [1, 4096]
theTimepointIndex: 1
theMaskCompressedSizes: [1, 130]
theMaskFileOffsets: [1, 8192]
";
        let node = slidebook7_yaml_compose(yaml);
        let (masks, position_tables) = slidebook7_load_masks(&node);
        assert_eq!(masks.len(), 2);
        assert_eq!(masks[0].name.as_deref(), Some("Nucleus"));
        assert_eq!(masks[0].centroid_feature.as_deref(), Some("area"));
        assert_eq!(masks[0].centroid_channel, Some(0));
        assert_eq!(masks[1].name.as_deref(), Some("Cytoplasm"));
        assert_eq!(masks[1].centroid_channel, Some(1));
        assert_eq!(position_tables, 2);
    }

    #[test]
    fn slidebook7_typed_annotation_graph_loads_like_java() {
        // One timepoint: 1 cube (base CAnnotation70 chained), 1 base annotation,
        // 1 FRAP region (base + 2 cube regions), 1 unknown (base chained).
        let yaml = "\
StartClass:
  ClassName: CDataTableHeaderRecord70
  mChannelIndex: 0
  mRows: 2
  mColumns: 3
  mTableType: 4
EndClass: 0
theTimepointIndex: 0
theCubeAnnotation70ListSize: 1
StartClass:
  ClassName: CCubeAnnotation70
  mIsBackground: false
  mRegionIndex: 5
  mIsFRAP: true
  mFRAPDevice: Galvo
EndClass: 0
StartClass:
  ClassName: CAnnotation70
  mGraphicType70: 2
  mObjectId: 11
EndClass: 0
theAnnotation70ListSize: 1
StartClass:
  ClassName: CAnnotation70
  mGraphicType70: 7
  mText: Region A
  mPlaneId: 3
EndClass: 0
theFRAPRegionAnnotation70ListSize: 1
StartClass:
  ClassName: CFRAPRegionAnnotation70
  mXML: <frap/>
EndClass: 0
StartClass:
  ClassName: CAnnotation70
  mGraphicType70: 9
EndClass: 0
theNumRegions: 2
StartClass:
  ClassName: CCubeAnnotation70
  mRegionIndex: 0
EndClass: 0
StartClass:
  ClassName: CAnnotation70
  mObjectId: 21
EndClass: 0
StartClass:
  ClassName: CCubeAnnotation70
  mRegionIndex: 1
EndClass: 0
StartClass:
  ClassName: CAnnotation70
  mObjectId: 22
EndClass: 0
theUnknownAnnotation70ListSize: 1
StartClass:
  ClassName: CUnknownAnnotation70
EndClass: 0
StartClass:
  ClassName: CAnnotation70
  mPlaneId: 99
EndClass: 0
";
        let node = slidebook7_yaml_compose(yaml);
        let (header, timepoints) = slidebook7_load_annotations(&node);
        assert_eq!(header.channel_index, Some(0));
        assert_eq!(header.table_type, Some(4));
        assert_eq!(timepoints.len(), 1);
        let tp = &timepoints[0];
        assert_eq!(tp.cube.len(), 1);
        assert_eq!(tp.cube[0].region_index, Some(5));
        assert_eq!(tp.cube[0].is_frap, Some(true));
        assert_eq!(tp.cube[0].frap_device.as_deref(), Some("Galvo"));
        assert_eq!(tp.cube[0].ann.object_id, Some(11));
        assert_eq!(tp.base.len(), 1);
        assert_eq!(tp.base[0].text.as_deref(), Some("Region A"));
        assert_eq!(tp.base[0].graphic_type, Some(7));
        assert_eq!(tp.base[0].plane_id, Some(3));
        assert_eq!(tp.frap.len(), 1);
        assert_eq!(tp.frap[0].xml.as_deref(), Some("<frap/>"));
        assert_eq!(tp.frap[0].region_count, 2);
        assert_eq!(tp.frap[0].ann.graphic_type, Some(9));
        // The unknown annotation is reached only if the FRAP region's 2 cube
        // regions (4 classes) were consumed with the correct index advancement.
        assert_eq!(tp.unknown.len(), 1);
        assert_eq!(tp.unknown[0].ann.plane_id, Some(99));
    }

    #[test]
    fn slidebook7_typed_channel_manip_records_capture_fields() {
        // One channel followed by one of each manip/LUT class; all field sets
        // must be captured (not just skipped for index advancement).
        let yaml = "\
StartClass:
  ClassName: CChannelRecord70
  mNumPlanes: 1
EndClass: 0
StartClass:
  ClassName: CExposureRecord70
  mExposureTime: 10
EndClass: 0
StartClass:
  ClassName: CChannelDef70
  mName: Ch0
EndClass: 0
StartClass:
  ClassName: CFluorDef70
  mLambda: 500
EndClass: 0
StartClass:
  ClassName: CRemapChannelLUT70
  mRemapType: 2
  mLowDesired: 0.0
  mHighDesired: 1.0
  mLowGiven: 10
  mHighGiven: 4000
  mBuiltTable: true
  mEquationString: y=x
EndClass: 0
StartClass:
  ClassName: CAlignManipRecord70
  mManipID: 1
  mXOffset: 1.5
  mYOffset: -2.5
  mZOffset: 0.25
EndClass: 0
StartClass:
  ClassName: CRatioManipRecord70
  mManipID: 2
  mKd: 224.0
  mRmin: 0.3
  mRmax: 3.1
  mBeta: 5.0
EndClass: 0
StartClass:
  ClassName: CFRETManipRecord70
  mManipID: 3
  mFRETParadigm: 1
  mFdDd: 0.7
  mFaAa: 0.4
EndClass: 0
StartClass:
  ClassName: CRemapManipRecord70
  mManipID: 4
  mRemapType: 6
  mNumCalibPoints: 8
EndClass: 0
";
        let node = slidebook7_yaml_compose(yaml);
        let (channels, extras) = slidebook7_load_channel_records(&node, 1);
        assert_eq!(channels.len(), 1);
        assert_eq!(extras.remap_luts.len(), 1);
        let lut = &extras.remap_luts[0];
        assert_eq!(lut.remap_type, Some(2));
        assert_eq!(lut.low_given, Some(10));
        assert_eq!(lut.high_given, Some(4000));
        assert_eq!(lut.built_table, Some(true));
        assert_eq!(lut.equation_string.as_deref(), Some("y=x"));
        assert_eq!(extras.align_manips.len(), 1);
        assert_eq!(extras.align_manips[0].manip_id, Some(1));
        assert_eq!(extras.align_manips[0].x_offset, Some(1.5));
        assert_eq!(extras.align_manips[0].z_offset, Some(0.25));
        assert_eq!(extras.ratio_manips.len(), 1);
        assert_eq!(extras.ratio_manips[0].kd, Some(224.0));
        assert_eq!(extras.ratio_manips[0].beta, Some(5.0));
        assert_eq!(extras.fret_manips.len(), 1);
        assert_eq!(extras.fret_manips[0].fret_paradigm, Some(1));
        assert_eq!(extras.fret_manips[0].fd_dd, Some(0.7));
        assert_eq!(extras.remap_manips.len(), 1);
        assert_eq!(extras.remap_manips[0].remap_type, Some(6));
        assert_eq!(extras.remap_manips[0].num_calib_points, Some(8));
    }

    #[test]
    fn slidebook7_typed_elapsed_sa_stage_loaders_match_java() {
        let elapsed = slidebook7_load_elapsed_times(&slidebook7_yaml_compose(
            "theElapsedTimes: [3, 0, 100, 200]\n",
        ));
        assert_eq!(elapsed, vec![0.0, 100.0, 200.0]);

        let sa = slidebook7_load_sa_positions(&slidebook7_yaml_compose(
            "theImageCount: 2\ntheSAPositions: [2, 5, 6]\ntheSAPositions: [2, 7, 8]\n",
        ));
        assert_eq!(sa.len(), 2);
        assert_eq!(sa[0], vec![5.0, 6.0]);
        assert_eq!(sa[1], vec![7.0, 8.0]);

        // StructArrayValues is a flat (non-size-prefixed) float array grouped XYZ.
        let stage = slidebook7_load_stage_positions(&slidebook7_yaml_compose(
            "StructArraySize: 2\nStructArrayValues: [1.0, 2.0, 3.0, 4.0, 5.0, 6.0]\n",
        ));
        assert_eq!(stage, vec![(1.0, 2.0, 3.0), (4.0, 5.0, 6.0)]);
    }

    #[test]
    fn slidebook7_typed_aux_data_loads_all_sections() {
        let yaml = "\
theAuxFloatDataTablesSize: 1
StartClass:
  ClassName: CDataTableHeaderRecord70
  mChannelIndex: 0
EndClass: 0
theXMLDescriptor: float-desc
theAuxData: [3, 1.5, 2.5, 3.5]
theAuxDoubleDataTablesSize: 1
StartClass:
  ClassName: CDataTableHeaderRecord70
  mChannelIndex: 1
EndClass: 0
theXMLDescriptor: double-desc
theAuxData: [2, 9.0, 8.0]
theAuxSInt32DataTablesSize: 0
theAuxSInt64DataTablesSize: 0
theAuxSerializedDataTablesSize: 1
StartClass:
  ClassName: CDataTableHeaderRecord70
  mChannelIndex: 2
EndClass: 0
theXMLDescriptor: xml-desc
theXmlAuxDataSize: 1
theXmlAuxData: <aux/>
";
        let aux = slidebook7_load_aux_data(&slidebook7_yaml_compose(yaml));
        assert_eq!(aux.float_tables.len(), 1);
        assert_eq!(
            aux.float_tables[0].xml_descriptor.as_deref(),
            Some("float-desc")
        );
        assert_eq!(aux.float_tables[0].value_count, 3);
        assert_eq!(aux.double_tables.len(), 1);
        assert_eq!(aux.double_tables[0].value_count, 2);
        assert_eq!(aux.sint32_tables.len(), 0);
        assert_eq!(aux.sint64_tables.len(), 0);
        assert_eq!(aux.xml_tables.len(), 1);
        assert_eq!(
            aux.xml_tables[0].xml_descriptor.as_deref(),
            Some("xml-desc")
        );
        assert_eq!(aux.xml_tables[0].xml_data.as_deref(), Some("<aux/>"));
    }

    #[test]
    fn slidebook7_typed_decoder_skips_unmatched_classname() {
        // A StartClass whose ClassName does not match must leave fields unset
        // (Java breaks out of the attribute loop on the leading ClassName).
        let yaml = "\
StartClass:
  ClassName: CSomeOtherRecord
  mWidth: 999
EndClass: 0
";
        let node = slidebook7_yaml_compose(yaml);
        let record = Sb7ImageRecord70::decode(&node);
        assert_eq!(record.width, None);
    }

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

    fn push_tiff_u16(out: &mut Vec<u8>, value: u16) {
        out.extend_from_slice(&value.to_le_bytes());
    }

    fn push_tiff_u32(out: &mut Vec<u8>, value: u32) {
        out.extend_from_slice(&value.to_le_bytes());
    }

    fn push_tiff_short(out: &mut Vec<u8>, tag: u16, value: u16) {
        push_tiff_u16(out, tag);
        push_tiff_u16(out, 3);
        push_tiff_u32(out, 1);
        push_tiff_u16(out, value);
        push_tiff_u16(out, 0);
    }

    fn push_tiff_long(out: &mut Vec<u8>, tag: u16, value: u32) {
        push_tiff_u16(out, tag);
        push_tiff_u16(out, 4);
        push_tiff_u32(out, 1);
        push_tiff_u32(out, value);
    }

    fn push_tiff_ascii_at_offset(out: &mut Vec<u8>, tag: u16, value: &str, offset: u32) {
        push_tiff_u16(out, tag);
        push_tiff_u16(out, 2);
        push_tiff_u32(out, (value.len() + 1) as u32);
        push_tiff_u32(out, offset);
    }

    fn write_afi_svs_with_label_macro(
        path: &Path,
        bits_per_sample: u16,
        main_pixels: &[u8],
        label_pixel: &[u8],
        macro_pixel: &[u8],
    ) {
        let ifds = [
            (2u32, 1u32, "Aperio Image|MPP=0.25", main_pixels),
            (1u32, 1u32, "label image", label_pixel),
            (1u32, 1u32, "macro image", macro_pixel),
        ];
        let ifd_size = 2 + 9 * 12 + 4;
        let ifd0_off: u32 = 8;
        let desc0_off: u32 = ifd0_off + (ifd_size * ifds.len()) as u32;
        let desc1_off: u32 = desc0_off + ifds[0].2.len() as u32 + 1;
        let desc2_off: u32 = desc1_off + ifds[1].2.len() as u32 + 1;
        let px0_off: u32 = desc2_off + ifds[2].2.len() as u32 + 1;
        let px1_off: u32 = px0_off + ifds[0].3.len() as u32;
        let px2_off: u32 = px1_off + ifds[1].3.len() as u32;

        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"II");
        push_tiff_u16(&mut bytes, 42);
        push_tiff_u32(&mut bytes, ifd0_off);

        for (i, (w, h, desc, pixels)) in ifds.iter().enumerate() {
            let next = if i + 1 < ifds.len() {
                ifd0_off + ((i + 1) * ifd_size) as u32
            } else {
                0
            };
            let desc_off = match i {
                0 => desc0_off,
                1 => desc1_off,
                _ => desc2_off,
            };
            let px_off = match i {
                0 => px0_off,
                1 => px1_off,
                _ => px2_off,
            };

            push_tiff_u16(&mut bytes, 9);
            push_tiff_long(&mut bytes, tag::IMAGE_WIDTH, *w);
            push_tiff_long(&mut bytes, tag::IMAGE_LENGTH, *h);
            push_tiff_short(&mut bytes, tag::BITS_PER_SAMPLE, bits_per_sample);
            push_tiff_short(&mut bytes, tag::COMPRESSION, 1);
            push_tiff_short(&mut bytes, tag::PHOTOMETRIC_INTERPRETATION, 1);
            push_tiff_ascii_at_offset(&mut bytes, tag::IMAGE_DESCRIPTION, desc, desc_off);
            push_tiff_long(&mut bytes, tag::STRIP_OFFSETS, px_off);
            push_tiff_long(&mut bytes, tag::ROWS_PER_STRIP, *h);
            push_tiff_long(&mut bytes, tag::STRIP_BYTE_COUNTS, pixels.len() as u32);
            push_tiff_u32(&mut bytes, next);
        }

        for (_, _, desc, _) in &ifds {
            bytes.extend_from_slice(desc.as_bytes());
            bytes.push(0);
        }
        for (_, _, _, pixels) in &ifds {
            bytes.extend_from_slice(pixels);
        }
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

    fn im3_bool_scalar(name: &str, value: bool) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&0u32.to_le_bytes());
        payload.extend_from_slice(&1u32.to_le_bytes());
        payload.push(if value { 1 } else { 0 });
        im3_record(name, 9, payload)
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
        bytes.extend_from_slice(&1985u32.to_le_bytes());
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
        assert!(reader.is_this_type_by_bytes(&1985u32.to_le_bytes()));
        assert!(!reader.is_this_type_by_bytes(&1985u32.to_be_bytes()));
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
        multi_bytes.extend_from_slice(&1985u32.to_le_bytes());
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
        bytes.extend_from_slice(&1985u32.to_le_bytes());
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
    fn im3_parses_spectral_library_and_boolean_records_like_java() {
        let path = temp_flim2_path("native-spectral-library.im3");
        let pixels = [3u16, 30]
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        let dataset = im3_container(
            "",
            vec![
                im3_int_array("Shape", &[1, 1, 2]),
                im3_bool_scalar("Homogeneous", true),
                im3_data_record(1, 1, 2, &pixels),
            ],
        );
        // SpectralLibrary -> (unnamed container) -> Spectra -> Values -> spectrum
        // containers, each grouping a Name and a Spectrum container whose nested
        // container holds Wavelengths/Magnitudes float records.
        let spectrum_one = im3_container(
            "",
            vec![
                im3_java_string_scalar("Name", "DAPI"),
                im3_container(
                    "Spectrum",
                    vec![im3_container(
                        "",
                        vec![
                            im3_java_float_array("Wavelengths", &[420.0, 440.0]),
                            im3_java_float_array("Magnitudes", &[0.5, 0.75]),
                        ],
                    )],
                ),
            ],
        );
        let spectrum_two = im3_container(
            "",
            vec![
                im3_java_string_scalar("Name", "FITC"),
                im3_container(
                    "Spectrum",
                    vec![im3_container(
                        "",
                        vec![im3_java_float_array("Wavelengths", &[520.0, 540.0])],
                    )],
                ),
            ],
        );
        let spectral_library = im3_container(
            "SpectralLibrary",
            vec![im3_container(
                "",
                vec![im3_container(
                    "Spectra",
                    vec![im3_container("Values", vec![spectrum_one, spectrum_two])],
                )],
            )],
        );
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1985u32.to_le_bytes());
        bytes.extend_from_slice(&im3_container(
            "Root",
            vec![im3_container("DataSet", vec![dataset]), spectral_library],
        ));
        std::fs::write(&path, bytes).unwrap();

        let mut reader = Im3Reader::new();
        reader
            .set_id(&path)
            .expect("native IM3 spectral-library fixture");
        let metadata = &reader.metadata().series_metadata;
        assert!(matches!(
            metadata.get("im3.spectral_library.spectrum_count"),
            Some(crate::common::metadata::MetadataValue::Int(2))
        ));
        assert!(matches!(
            metadata.get("im3.spectral_library.spectrum.0.name"),
            Some(crate::common::metadata::MetadataValue::String(value)) if value == "DAPI"
        ));
        assert!(matches!(
            metadata.get("im3.spectral_library.spectrum.0.wavelengths"),
            Some(crate::common::metadata::MetadataValue::String(value)) if value == "420,440"
        ));
        assert!(matches!(
            metadata.get("im3.spectral_library.spectrum.0.magnitudes"),
            Some(crate::common::metadata::MetadataValue::String(value)) if value == "0.5,0.75"
        ));
        assert!(matches!(
            metadata.get("im3.spectral_library.spectrum.1.name"),
            Some(crate::common::metadata::MetadataValue::String(value)) if value == "FITC"
        ));
        assert!(matches!(
            metadata.get("im3.spectral_library.spectrum.1.wavelengths"),
            Some(crate::common::metadata::MetadataValue::String(value)) if value == "520,540"
        ));
        // The second spectrum has no Magnitudes record, so none is surfaced.
        assert!(metadata
            .get("im3.spectral_library.spectrum.1.magnitudes")
            .is_none());
        // A REC_BOOLEAN scalar is now recognised rather than flagged unsupported.
        assert!(matches!(
            metadata.get("im3.native.homogeneous"),
            Some(crate::common::metadata::MetadataValue::Bool(true))
        ));
        assert!(metadata
            .get("im3.native.unsupported_metadata_records")
            .is_none());

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
        bytes.extend_from_slice(&1985u32.to_le_bytes());
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
        bytes.extend_from_slice(&1985u32.to_le_bytes());
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
        bytes.extend_from_slice(&1985u32.to_le_bytes());
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
        mismatch_bytes.extend_from_slice(&1985u32.to_le_bytes());
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
    fn ivision_native_scrapes_iplab_acquisition_metadata_like_java() {
        // 16-bit mono, 2x2, single Z plane, with a trailing Apple plist that
        // carries the same iplab:* keys the Java IvisionHandler recognises.
        let path = temp_flim2_path("native-iplab.ipm");
        let payload = [0x0102u16, 0x0304, 0x0506, 0x0708]
            .into_iter()
            .flat_map(u16::to_be_bytes)
            .collect::<Vec<_>>();

        let mut bytes = vec![0u8; 72];
        bytes[..4].copy_from_slice(b"1.0A");
        bytes[4] = 1;
        bytes[5] = 6; // 16-bit unsigned mono
        bytes[6..10].copy_from_slice(&2u32.to_be_bytes());
        bytes[10..14].copy_from_slice(&2u32.to_be_bytes());
        bytes[20..22].copy_from_slice(&1u16.to_be_bytes());
        bytes.extend_from_slice(&vec![0u8; 2048]); // LUT, since x>1 && y>1
        bytes.extend_from_slice(&payload);
        let plist = concat!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>",
            "<plist version=\"1.0\"><dict>",
            "<key>iplab:Bin_X</key><string>2</string>",
            "<key>iplab:Bin_Y</key><string>2</string>",
            "<key>iplab:Capture_Date</key><string>2018-04-12T09:08:07Z</string>",
            "<key>iplab:Exposure</key><string>0.125</string>",
            "<key>iplab:Gain</key><string>3.5</string>",
            "<key>iplab:Offset</key><string>10</string>",
            "<key>iplab:Interval_T</key><string>2.0</string>",
            "<key>iplab:Objective_Mag</key><string>40.0</string>",
            "<key>iplab:Objective_NA</key><string>1.3</string>",
            "<key>iplab:Objective_RI</key><string>1.515</string>",
            "<key>iplab:Wavelength</key><string>488</string>",
            "</dict></plist>",
        );
        bytes.extend_from_slice(plist.as_bytes());
        std::fs::write(&path, &bytes).unwrap();

        let mut reader = IvisionReader::new();
        reader.set_id(&path).expect("native iplab iVision fixture");

        let md = &reader.metadata().series_metadata;
        // addGlobalMeta keys, mirroring the Java handler.
        assert!(matches!(
            md.get("iplab:Bin_X"),
            Some(crate::common::metadata::MetadataValue::String(v)) if v == "2"
        ));
        assert!(matches!(
            md.get("iplab:Capture_Date"),
            Some(crate::common::metadata::MetadataValue::String(v)) if v == "2018-04-12T09:08:07Z"
        ));
        assert!(matches!(
            md.get("iplab:Exposure"),
            Some(crate::common::metadata::MetadataValue::String(v)) if v == "0.125"
        ));
        assert!(matches!(
            md.get("iplab:Gain"),
            Some(crate::common::metadata::MetadataValue::String(v)) if v == "3.5"
        ));
        assert!(matches!(
            md.get("iplab:Interval_T"),
            Some(crate::common::metadata::MetadataValue::String(v)) if v == "2.0"
        ));
        assert!(matches!(
            md.get("iplab:Objective_Mag"),
            Some(crate::common::metadata::MetadataValue::Float(v)) if (*v - 40.0).abs() < 1e-9
        ));
        assert!(matches!(
            md.get("iplab:Objective_NA"),
            Some(crate::common::metadata::MetadataValue::Float(v)) if (*v - 1.3).abs() < 1e-9
        ));
        assert!(matches!(
            md.get("iplab:Objective_RI"),
            Some(crate::common::metadata::MetadataValue::Float(v)) if (*v - 1.515).abs() < 1e-9
        ));

        let ome = reader.ome_metadata().expect("iVision OME metadata");
        let image = ome.images.first().expect("OME image");
        assert_eq!(
            image.acquisition_date.as_deref(),
            Some("2018-04-12T09:08:07Z")
        );
        assert!(matches!(image.time_increment, Some(v) if (v - 2.0).abs() < 1e-9));

        // Plane-level DeltaT / ExposureTime.
        let plane = image.planes.first().expect("OME plane");
        assert!(matches!(plane.delta_t, Some(v) if (v - 2.0).abs() < 1e-9));
        assert!(matches!(plane.exposure_time, Some(v) if (v - 0.125).abs() < 1e-9));

        // Objective: lensNA + magnification.
        let instrument = ome
            .instruments
            .get(image.instrument_ref.expect("instrument ref"))
            .expect("OME instrument");
        let objective = instrument.objectives.first().expect("OME objective");
        assert!(matches!(objective.lens_na, Some(v) if (v - 1.3).abs() < 1e-9));
        assert!(matches!(objective.nominal_magnification, Some(v) if (v - 40.0).abs() < 1e-9));

        // DetectorSettings: gain + binning on the first channel.
        let channel = image.channels.first().expect("OME channel");
        assert!(matches!(channel.detector_settings_gain, Some(v) if (v - 3.5).abs() < 1e-9));
        assert_eq!(channel.detector_settings_binning.as_deref(), Some("2x2"));

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
        assert!(reader.metadata().is_rgb);
        assert!(reader.metadata().is_interleaved);
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
    fn afi_byte_detection_matches_java_permissive_probe() {
        let reader = AfiReader::new();
        assert!(reader.is_this_type_by_bytes(b"<AFI"));
        assert!(!reader.is_this_type_by_bytes(b"<AF"));
    }

    #[test]
    fn afi_widens_8bit_channel_planes_to_first_channel_depth_like_java() {
        let dir = temp_flim2_path("afi-widen-dir");
        std::fs::create_dir_all(&dir).unwrap();
        let afi = dir.join("case.afi");
        let first = dir.join("slide_DAPI.svs");
        let second = dir.join("slide_FITC.svs");

        write_afi_svs_with_label_macro(&first, 16, &[0x34, 0x12, 0x78, 0x56], &[0, 0], &[0, 0]);
        write_afi_svs_with_label_macro(&second, 8, &[0, 128], &[0], &[0]);
        std::fs::write(
            &afi,
            "<ImageList><Image><Path>slide_DAPI.svs</Path></Image><Image><Path>slide_FITC.svs</Path></Image></ImageList>",
        )
        .unwrap();

        let mut reader = AfiReader::new();
        reader.set_id(&afi).expect("AFI fixture");
        assert_eq!(reader.series_count(), 3);
        assert_eq!(reader.metadata().size_c, 2);
        assert_eq!(reader.metadata().pixel_type, PixelType::Uint16);
        assert_eq!(reader.metadata().image_count, 2);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![0x34, 0x12, 0x78, 0x56]);
        assert_eq!(reader.open_bytes(1).unwrap(), vec![0x00, 0x00, 0x80, 0x80]);
        assert_eq!(
            reader.open_bytes_region(1, 1, 0, 1, 1).unwrap(),
            vec![0x80, 0x80]
        );
        assert_eq!(
            reader.open_thumb_bytes(1).unwrap(),
            vec![0x00, 0x00, 0x80, 0x80]
        );

        let _ = std::fs::remove_file(first);
        let _ = std::fs::remove_file(second);
        let _ = std::fs::remove_file(afi);
        let _ = std::fs::remove_dir(dir);
    }

    #[test]
    fn afi_rejects_wider_later_channel_downsampling_like_java() {
        let dir = temp_flim2_path("afi-downsample-dir");
        std::fs::create_dir_all(&dir).unwrap();
        let afi = dir.join("case.afi");
        let first = dir.join("slide_DAPI.svs");
        let second = dir.join("slide_FITC.svs");

        write_afi_svs_with_label_macro(&first, 8, &[1, 2], &[0], &[0]);
        write_afi_svs_with_label_macro(&second, 16, &[0x34, 0x12, 0x78, 0x56], &[0, 0], &[0, 0]);
        std::fs::write(
            &afi,
            "<ImageList><Image><Path>slide_DAPI.svs</Path></Image><Image><Path>slide_FITC.svs</Path></Image></ImageList>",
        )
        .unwrap();

        let mut reader = AfiReader::new();
        reader.set_id(&afi).expect("AFI fixture");
        assert_eq!(reader.metadata().pixel_type, PixelType::Uint8);
        let err = reader.open_bytes(1).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("downsampling images is not supported")),
            "unexpected AFI downsampling error: {err:?}"
        );

        let _ = std::fs::remove_file(first);
        let _ = std::fs::remove_file(second);
        let _ = std::fs::remove_file(afi);
        let _ = std::fs::remove_dir(dir);
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

    fn short_vec_entry(tag: u16, values: &[u16]) -> TestEntry {
        TestEntry {
            tag,
            typ: 3,
            count: values.len() as u32,
            value: values.iter().flat_map(|v| v.to_le_bytes()).collect(),
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

    fn write_test_ifd_be(
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

        out.extend_from_slice(&(entries.len() as u16).to_be_bytes());
        for (_, entry) in entries {
            out.extend_from_slice(&entry.tag.to_be_bytes());
            out.extend_from_slice(&entry.typ.to_be_bytes());
            out.extend_from_slice(&entry.count.to_be_bytes());
            if entry.value.len() <= 4 {
                let mut inline = [0u8; 4];
                inline[..entry.value.len()].copy_from_slice(&entry.value);
                out.extend_from_slice(&inline);
            } else {
                let offset = (extra_base + extra.len()) as u32;
                out.extend_from_slice(&offset.to_be_bytes());
                extra.extend_from_slice(&entry.value);
            }
        }
        out.extend_from_slice(&next_ifd_offset.to_be_bytes());
        out.extend_from_slice(&extra);
    }

    fn write_synthetic_flowsight_cif(
        path: &Path,
        bits_per_sample: u16,
        compression: u16,
        compressed: &[u8],
    ) {
        write_synthetic_flowsight_cif_with_metadata(
            path,
            bits_per_sample,
            compression,
            compressed,
            "BF|SSC",
            "Brightfield|Scatter",
            "<Root><Imaging><ChannelInUseIndicators>1 1</ChannelInUseIndicators></Imaging></Root>",
        );
    }

    fn write_synthetic_flowsight_cif_with_metadata(
        path: &Path,
        bits_per_sample: u16,
        compression: u16,
        compressed: &[u8],
        channel_names: &str,
        channel_descs: &str,
        metadata_xml: &str,
    ) {
        let ifd0_entries = vec![
            short_entry(FLOWSIGHT_CHANNEL_COUNT_TAG, 2),
            ascii_entry(FLOWSIGHT_CHANNEL_NAMES_TAG, channel_names),
            ascii_entry(FLOWSIGHT_CHANNEL_DESCS_TAG, channel_descs),
            ascii_entry(FLOWSIGHT_METADATA_XML_TAG, metadata_xml),
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

    fn write_big_endian_synthetic_flowsight_cif(path: &Path) {
        let compressed = [0x00, 1, 0xff, 1];
        let ifd0_entries = vec![
            TestEntry {
                tag: FLOWSIGHT_CHANNEL_COUNT_TAG,
                typ: 3,
                count: 1,
                value: 2u16.to_be_bytes().to_vec(),
            },
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
            TestEntry {
                tag: tag::IMAGE_WIDTH,
                typ: 4,
                count: 1,
                value: 4u32.to_be_bytes().to_vec(),
            },
            TestEntry {
                tag: tag::IMAGE_LENGTH,
                typ: 4,
                count: 1,
                value: 1u32.to_be_bytes().to_vec(),
            },
            TestEntry {
                tag: tag::BITS_PER_SAMPLE,
                typ: 3,
                count: 1,
                value: 8u16.to_be_bytes().to_vec(),
            },
            TestEntry {
                tag: tag::COMPRESSION,
                typ: 3,
                count: 1,
                value: FLOWSIGHT_BITMASK_COMPRESSION.to_be_bytes().to_vec(),
            },
            TestEntry {
                tag: tag::ROWS_PER_STRIP,
                typ: 4,
                count: 1,
                value: 1u32.to_be_bytes().to_vec(),
            },
            TestEntry {
                tag: tag::STRIP_OFFSETS,
                typ: 4,
                count: 1,
                value: ((ifd1_offset + ifd_table_len(ifd1_entry_count)) as u32)
                    .to_be_bytes()
                    .to_vec(),
            },
            TestEntry {
                tag: tag::STRIP_BYTE_COUNTS,
                typ: 4,
                count: 1,
                value: (compressed.len() as u32).to_be_bytes().to_vec(),
            },
        ];

        let mut data = Vec::new();
        data.extend_from_slice(b"MM");
        data.extend_from_slice(&42u16.to_be_bytes());
        data.extend_from_slice(&(ifd0_offset as u32).to_be_bytes());
        write_test_ifd_be(&mut data, &ifd0_entries, ifd0_offset, ifd1_offset as u32);
        write_test_ifd_be(&mut data, &ifd1_entries, ifd1_offset, 0);
        data.extend_from_slice(&compressed);

        let mut file = File::create(path).unwrap();
        file.write_all(&data).unwrap();
    }

    fn write_synthetic_flowsight_cif_without_metadata_tag(path: &Path) {
        let compressed = [0x00, 1, 0xff, 1];
        let ifd0_entries = vec![
            short_entry(FLOWSIGHT_CHANNEL_COUNT_TAG, 2),
            ascii_entry(FLOWSIGHT_CHANNEL_NAMES_TAG, "BF|SSC"),
            ascii_entry(FLOWSIGHT_CHANNEL_DESCS_TAG, "Brightfield|Scatter"),
        ];
        let ifd0_offset = 8usize;
        let ifd1_offset =
            ifd0_offset + ifd_table_len(ifd0_entries.len()) + ifd_extra_len(&ifd0_entries);
        let ifd1_entry_count = 7usize;
        let ifd1_entries = vec![
            long_entry(tag::IMAGE_WIDTH, 4),
            long_entry(tag::IMAGE_LENGTH, 1),
            short_entry(tag::BITS_PER_SAMPLE, 8),
            short_entry(tag::COMPRESSION, FLOWSIGHT_BITMASK_COMPRESSION),
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
        data.extend_from_slice(&compressed);

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

    fn write_one_pixel_tiff_with_description(path: &Path, value: u8, description: &str) {
        let mut entries = vec![
            long_entry(tag::IMAGE_WIDTH, 1),
            long_entry(tag::IMAGE_LENGTH, 1),
            short_entry(tag::BITS_PER_SAMPLE, 8),
            short_entry(tag::COMPRESSION, 1),
            short_entry(tag::PHOTOMETRIC_INTERPRETATION, 1),
            ascii_entry(tag::IMAGE_DESCRIPTION, description),
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

    fn write_rgb_ndpi_for_ndpis(path: &Path, channel_name: &str, wavelength: f64, pixels: &[u8]) {
        const NDPI_MARKER_TAG: u16 = 65426;
        let mut entries = vec![
            long_entry(tag::IMAGE_WIDTH, 2),
            long_entry(tag::IMAGE_LENGTH, 1),
            short_vec_entry(tag::BITS_PER_SAMPLE, &[8, 8, 8]),
            short_entry(tag::COMPRESSION, 1),
            short_entry(tag::PHOTOMETRIC_INTERPRETATION, 2),
            short_entry(tag::SAMPLES_PER_PIXEL, 3),
            long_entry(tag::ROWS_PER_STRIP, 1),
            long_entry(tag::STRIP_BYTE_COUNTS, pixels.len() as u32),
            short_entry(tag::PLANAR_CONFIGURATION, 1),
            long_entry(NDPI_MARKER_TAG, 1),
            ascii_entry(NDPI_TAG_CHANNEL, channel_name),
            double_entry(NDPI_TAG_EMISSION_WAVELENGTH, wavelength),
        ];
        let strip_offset = 8 + ifd_table_len(entries.len() + 1) + ifd_extra_len(&entries);
        entries.push(long_entry(tag::STRIP_OFFSETS, strip_offset as u32));

        let mut data = Vec::new();
        data.extend_from_slice(b"II");
        data.extend_from_slice(&42u16.to_le_bytes());
        data.extend_from_slice(&8u32.to_le_bytes());
        write_test_ifd(&mut data, &entries, 8, 0);
        data.resize(strip_offset, 0);
        data.extend_from_slice(pixels);

        let mut file = File::create(path).unwrap();
        file.write_all(&data).unwrap();
    }

    #[test]
    fn ndpis_rgb_companions_select_java_band_used_by_emission_wavelength() {
        let dir = temp_flim2_path("ndpis-band-dir");
        std::fs::create_dir_all(&dir).unwrap();
        let index = dir.join("case.ndpis");
        let blue = dir.join("blue.ndpi");
        let green = dir.join("green.ndpi");

        write_rgb_ndpi_for_ndpis(&blue, "blue channel", 450.0, &[10, 11, 20, 40, 50, 60]);
        write_rgb_ndpi_for_ndpis(&green, "green channel", 520.0, &[11, 41, 21, 51, 31, 61]);
        std::fs::write(
            &index,
            "NoImages=2\r\nImage0=blue.ndpi\r\nImage1=green.ndpi\r\n",
        )
        .unwrap();

        let mut reader = NdpisReader::new();
        reader.set_id(&index).expect("NDPIS RGB companion fixture");

        assert_eq!(reader.metadata().size_c, 2);
        assert_eq!(reader.metadata().image_count, 2);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![20, 60]);
        assert_eq!(reader.open_bytes(1).unwrap(), vec![41, 31]);
        assert_eq!(reader.open_bytes_region(0, 1, 0, 1, 1).unwrap(), vec![60]);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn ndpis_failed_second_set_id_clears_previous_state() {
        let dir = temp_flim2_path("ndpis-failed-second");
        std::fs::create_dir_all(&dir).unwrap();
        let good_index = dir.join("good.ndpis");
        let bad_index = dir.join("bad.ndpis");
        let ndpi = dir.join("channel.ndpi");

        write_rgb_ndpi_for_ndpis(&ndpi, "blue channel", 450.0, &[10, 11, 20, 40, 50, 60]);
        std::fs::write(&good_index, "NoImages=1\r\nImage0=channel.ndpi\r\n").unwrap();
        std::fs::write(&bad_index, "NoImages=1\r\nImage0=missing.ndpi\r\n").unwrap();

        let mut reader = NdpisReader::new();
        reader.set_id(&good_index).expect("valid NDPIS fixture");
        assert_eq!(reader.metadata().image_count, 1);

        assert!(reader.set_id(&bad_index).is_err());
        assert_eq!(reader.series_count(), 0);
        assert!(matches!(
            reader.open_bytes(0),
            Err(BioFormatsError::NotInitialized)
        ));
        assert!(matches!(
            reader.open_bytes_region(0, 0, 0, 1, 1),
            Err(BioFormatsError::NotInitialized)
        ));

        let _ = std::fs::remove_dir_all(dir);
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
        assert!(Im3Reader::new().is_this_type_by_bytes(&1985u32.to_le_bytes()));
        assert!(!Im3Reader::new().is_this_type_by_bytes(&1985u32.to_be_bytes()));

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
    fn imaris_tiff_skips_channel_name_that_is_filename_suffix_like_java() {
        let ims = temp_flim2_path("sample_name.ims");
        write_one_pixel_tiff_with_description(
            &ims,
            7,
            "[Imaris]\nName=sample_name.ims\nName=DAPI\nDescription=desc\nRecordingDate=2024-01-02 03:04:05.678\n",
        );

        let header = std::fs::read(&ims).unwrap();
        assert!(ImarisTiffReader::new().is_this_type_by_bytes(&header));

        let mut reader = ImarisTiffReader::new();
        reader.set_id(&ims).unwrap();
        let metadata = reader.metadata();
        assert!(matches!(
            metadata.series_metadata.get("imaris.channel.0.name"),
            Some(MetadataValue::String(name)) if name == "DAPI"
        ));
        assert!(!metadata.series_metadata.values().any(|value| {
            matches!(value, MetadataValue::String(name) if name == "sample_name.ims")
        }));
        let _ = std::fs::remove_file(ims);
    }

    #[test]
    fn xlef_references_single_tiff() {
        let xlef = temp_flim2_path("project.xlef");
        let tiff = xlef.with_file_name("image.tif");
        std::fs::write(&xlef, r#"<XLEF><Image File="image.tif"/></XLEF>"#).unwrap();
        let refs = xlef_referenced_paths(&std::fs::read_to_string(&xlef).unwrap(), &xlef);
        assert_eq!(refs, vec![tiff]);
        let images = XlefReader::referenced_images(&xlef).unwrap();
        assert_eq!(
            images,
            vec![XlefReference::Image {
                path: refs[0].clone(),
                tile_count: 1,
                tile_index_base: 0,
            }]
        );
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
    fn xlef_xlif_tilescan_file_tiles_are_not_multiplied_by_tile_count() {
        let xlef = temp_flim2_path("tilescan_project.xlef");
        let xlif = xlef.with_file_name("tilescan.xlif");
        let tile_a = xlef.with_file_name("tile_a.tif");
        let tile_b = xlef.with_file_name("tile_b.tif");
        write_one_pixel_tiff(&tile_a, 31);
        write_one_pixel_tiff(&tile_b, 42);
        std::fs::write(
            &xlif,
            r#"<XLIF>
<DimensionDescription DimID="10" NumberOfElements="2"/>
<Image File="tile_a.tif"/>
<Image File="tile_b.tif"/>
</XLIF>"#,
        )
        .unwrap();
        std::fs::write(&xlef, r#"<XLEF><Project File="tilescan.xlif"/></XLEF>"#).unwrap();

        let mut reader = XlefReader::new();
        reader.set_id(&xlef).unwrap();
        assert_eq!(reader.series_count(), 2);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![31]);
        assert!(matches!(
            reader
                .metadata()
                .series_metadata
                .get("xlef.project.tile_index"),
            Some(MetadataValue::Int(0))
        ));

        reader.set_series(1).unwrap();
        assert_eq!(reader.open_bytes(0).unwrap(), vec![42]);
        assert!(matches!(
            reader
                .metadata()
                .series_metadata
                .get("xlef.project.tile_index"),
            Some(MetadataValue::Int(1))
        ));

        let _ = std::fs::remove_file(xlef);
        let _ = std::fs::remove_file(xlif);
        let _ = std::fs::remove_file(tile_a);
        let _ = std::fs::remove_file(tile_b);
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
    fn xlef_lms_metadata_leaf_uses_x_bytes_inc_for_java_pixel_type() {
        let xlef = temp_flim2_path("lms_float32.xlef");
        let lms = xlef.with_extension("lms");
        std::fs::write(
            &lms,
            r#"<XLIF><Element Name="Float scan"><Data><Image Name="scan">
<ImageDescription>
<Channels><ChannelDescription BytesInc="0"/></Channels>
<Dimensions>
<DimensionDescription DimID="1" NumberOfElements="2" BytesInc="4"/>
<DimensionDescription DimID="2" NumberOfElements="2" BytesInc="8"/>
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
        let meta = reader.metadata();
        assert_eq!(meta.pixel_type, PixelType::Float32);
        assert_eq!(meta.bits_per_pixel, 32);

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
        let header = std::fs::read(&path).unwrap();
        assert!(reader.is_this_type_by_bytes(&header[..header.len().min(512)]));
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
    fn slidebook_tiff_rejects_plain_tiff_without_java_private_tags() {
        let path = temp_flim2_path("plain-slidebook-candidate.tif");
        write_one_pixel_tiff(&path, 1);

        let err = SlidebookTiffReader::new().set_id(&path).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("SlideBook TIFF")),
            "{err:?}"
        );

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
    fn flowsight_channel_name_split_preserves_java_interior_empty_tokens() {
        assert_eq!(
            split_flowsight_pipe_list("BF||SSC"),
            vec!["BF".to_string(), "".to_string(), "SSC".to_string()]
        );
        assert_eq!(
            split_flowsight_pipe_list("BF|SSC|"),
            vec!["BF".to_string(), "SSC".to_string()]
        );
        assert_eq!(split_flowsight_pipe_list("|"), Vec::<String>::new());
        assert_eq!(split_flowsight_pipe_list("||"), Vec::<String>::new());
        assert_eq!(split_flowsight_pipe_list(""), vec!["".to_string()]);
    }

    #[test]
    fn flowsight_channel_in_use_count_matches_java_imaging_child_scope() {
        assert_eq!(
            count_flowsight_channels_in_use(
                "<Root><ChannelInUseIndicators>1</ChannelInUseIndicators><Imaging><Other><ChannelInUseIndicators>1 1 1</ChannelInUseIndicators></Other><ChannelInUseIndicators_0>1 0 1</ChannelInUseIndicators_0></Imaging></Root>"
            ),
            Some(2)
        );
        assert_eq!(
            count_flowsight_channels_in_use(
                "<Root><ChannelInUseIndicators>1 1 1</ChannelInUseIndicators><Imaging><Other>1</Other></Imaging></Root>"
            ),
            None
        );
        assert_eq!(
            count_flowsight_channels_in_use(
                "<Root><Imaging><ChannelInUseIndicators>1\t1 1</ChannelInUseIndicators></Imaging></Root>"
            ),
            Some(1)
        );
        assert_eq!(
            count_flowsight_channels_in_use(
                "<Root><Imaging><ChannelInUseIndicators>1 1 1</ChannelInUseIndicators><Other><ChannelInUseIndicators>1</ChannelInUseIndicators></Other><ChannelInUseIndicators_1>0 1</ChannelInUseIndicators_1></Imaging></Root>"
            ),
            Some(1)
        );
    }

    #[test]
    fn flowsight_byte_detection_requires_metadata_tag_like_java() {
        let good = temp_cif_path("detect-good");
        write_synthetic_flowsight_cif(&good, 8, FLOWSIGHT_BITMASK_COMPRESSION, &[0x00, 1, 0xff, 1]);
        let good_bytes = std::fs::read(&good).unwrap();
        assert!(FlowSightReader::new().is_this_type_by_bytes(&good_bytes));

        let bad = temp_cif_path("detect-bad");
        write_synthetic_flowsight_cif_without_metadata_tag(&bad);
        let bad_bytes = std::fs::read(&bad).unwrap();
        assert!(!FlowSightReader::new().is_this_type_by_bytes(&bad_bytes));

        let _ = std::fs::remove_file(good);
        let _ = std::fs::remove_file(bad);
    }

    #[test]
    fn flowsight_metadata_endianness_matches_tiff_header_like_java() {
        let path = temp_cif_path("big-endian");
        write_big_endian_synthetic_flowsight_cif(&path);

        let mut reader = FlowSightReader::new();
        reader.set_id(&path).expect("big-endian FlowSight CIF");

        assert!(!reader.metadata().is_little_endian);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![0x00, 0x00]);
        assert_eq!(reader.open_bytes(1).unwrap(), vec![0xff, 0xff]);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn flowsight_rejects_channel_description_count_mismatch_like_java() {
        let path = temp_cif_path("bad-channel-descs");
        write_synthetic_flowsight_cif_with_metadata(
            &path,
            8,
            FLOWSIGHT_BITMASK_COMPRESSION,
            &[0x00, 1, 0xff, 1],
            "BF|SSC",
            "Only one description",
            "<Root><Imaging><ChannelInUseIndicators>1 1</ChannelInUseIndicators></Imaging></Root>",
        );

        let err = FlowSightReader::new().set_id(&path).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::Format(ref message) if message.contains("does not match number of channel descriptions")),
            "unexpected FlowSight error: {err:?}"
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn flowsight_rejects_channel_description_mismatch_before_xml_override_like_java() {
        let path = temp_cif_path("bad-channel-descs-before-xml");
        write_synthetic_flowsight_cif_with_metadata(
            &path,
            8,
            FLOWSIGHT_BITMASK_COMPRESSION,
            &[0x00, 1, 0xff, 1],
            "BF|SSC",
            "Only one description",
            "<Root><Imaging><ChannelInUseIndicators>1 0</ChannelInUseIndicators></Imaging></Root>",
        );

        let err = FlowSightReader::new().set_id(&path).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::Format(ref message) if message.contains("does not match number of channel descriptions")),
            "unexpected FlowSight error: {err:?}"
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn flowsight_ome_channel_names_use_descriptions_and_mask_suffix_like_java() {
        let greyscale = temp_cif_path("ome-channel-greyscale");
        write_synthetic_flowsight_cif(
            &greyscale,
            16,
            FLOWSIGHT_GREYSCALE_COMPRESSION,
            &[0x1a, 0x91, 0x11],
        );
        let mut reader = FlowSightReader::new();
        reader.set_id(&greyscale).expect("greyscale FlowSight CIF");
        let ome = reader.ome_metadata().expect("greyscale OME metadata");
        assert_eq!(
            ome.images[0].channels[0].name.as_deref(),
            Some("Brightfield")
        );
        assert_eq!(ome.images[0].channels[1].name.as_deref(), Some("Scatter"));

        let mask = temp_cif_path("ome-channel-mask");
        write_synthetic_flowsight_cif(&mask, 8, FLOWSIGHT_BITMASK_COMPRESSION, &[0x00, 1, 0xff, 1]);
        let mut reader = FlowSightReader::new();
        reader.set_id(&mask).expect("mask FlowSight CIF");
        let ome = reader.ome_metadata().expect("mask OME metadata");
        assert_eq!(
            ome.images[0].channels[0].name.as_deref(),
            Some("Brightfield Mask")
        );
        assert_eq!(
            ome.images[0].channels[1].name.as_deref(),
            Some("Scatter Mask")
        );

        let _ = std::fs::remove_file(greyscale);
        let _ = std::fs::remove_file(mask);
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

    /// DEVICE_SUBTYPE and STACK_TYPE numeric codes are translated to human
    /// readable labels (CellSensReader.java:1883-1889, getStackType /
    /// getDeviceSubtype), and RWC_FRAME_ORIGIN populates the stage origin
    /// (CellSensReader.java:1859-1863).
    #[test]
    fn vsi_tags_translate_device_subtype_stack_type_and_frame_origin() {
        let mut origin = Vec::new();
        origin.extend_from_slice(&12.5f64.to_le_bytes());
        origin.extend_from_slice(&34.0f64.to_le_bytes());
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
                field_type: VSI_INT,
                tag: VSI_DEVICE_SUBTYPE,
                data: 20006i32.to_le_bytes().to_vec(), // -> "Objective"
            },
            VsiField {
                field_type: VSI_INT,
                tag: VSI_STACK_TYPE,
                data: 1i32.to_le_bytes().to_vec(), // -> "Overview image"
            },
            VsiField {
                field_type: 261, // DOUBLE_2
                tag: VSI_RWC_FRAME_ORIGIN,
                data: origin,
            },
        ];
        let stream = build_vsi_tag_stream(&fields);
        let mut parser = VsiTagParser::new(&stream);
        parser.read_tags(8, true, "");

        assert_eq!(parser.pyramids.len(), 1);
        let m = &parser.pyramids[0].meta;
        assert_eq!(
            m.device_subtypes,
            vec!["Objective".to_string()],
            "DEVICE_SUBTYPE 20006 translated to label"
        );
        assert_eq!(
            m.stack_type.as_deref(),
            Some("Overview image"),
            "STACK_TYPE 1 translated to label"
        );
        assert_eq!(m.origin_x, Some(12.5));
        assert_eq!(m.origin_y, Some(34.0));

        // The translated values also flow into the emitted series metadata.
        let mut sm = HashMap::new();
        insert_cellsens_acquisition_metadata(&mut sm, "cellsens.ets.0", m);
        let get = |k: &str| sm.get(k).map(|v: &MetadataValue| v.to_string());
        assert_eq!(
            get("cellsens.ets.0.device_subtype").as_deref(),
            Some("Objective")
        );
        assert_eq!(
            get("cellsens.ets.0.stack_type").as_deref(),
            Some("Overview image")
        );
        assert_eq!(
            get("cellsens.ets.0.frame_origin_x").as_deref(),
            Some("12.5")
        );
        assert_eq!(get("cellsens.ets.0.frame_origin_y").as_deref(), Some("34"));
    }

    /// `cellsens_tag_name` mirrors the Java `getTagName` switch: known tag IDs
    /// resolve to the exact Java strings; unhandled tags return `None`.
    #[test]
    fn cellsens_tag_name_resolves_named_tags() {
        assert_eq!(cellsens_tag_name(100006), Some("Sharpness")); // SHARPNESS
        assert_eq!(cellsens_tag_name(100031), Some("Brightness")); // BRIGHTNESS
        assert_eq!(cellsens_tag_name(120063), Some("Objective Name")); // OBJECTIVE_NAME
        assert_eq!(cellsens_tag_name(268435458), Some("Value")); // VALUE
        assert_eq!(cellsens_tag_name(120065), Some("Objective Description"));
        // Unhandled tag -> None (Java returns null / logs "Unhandled tag").
        assert_eq!(cellsens_tag_name(999999), None);
    }

    /// Named-but-untyped tags are surfaced as `cellsens.ets.*.tag.<name>`
    /// original metadata, mirroring Java's
    /// `addMetaList(tagPrefix + getTagName(tag), value)`
    /// (CellSensReader.java:1995-2002), in addition to the typed keys.
    #[test]
    fn vsi_tags_emit_named_original_metadata() {
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
                field_type: VSI_INT,
                tag: 100006, // SHARPNESS -> "Sharpness" (not typed)
                data: 7i32.to_le_bytes().to_vec(),
            },
            VsiField {
                field_type: VSI_INT,
                tag: 100031, // BRIGHTNESS -> "Brightness" (not typed)
                data: 42i32.to_le_bytes().to_vec(),
            },
        ];
        let stream = build_vsi_tag_stream(&fields);
        let mut parser = VsiTagParser::new(&stream);
        parser.read_tags(8, true, "");

        assert_eq!(parser.pyramids.len(), 1);
        let m = &parser.pyramids[0].meta;
        assert!(
            m.named_tags
                .iter()
                .any(|(k, v)| k == "Sharpness" && v == "7"),
            "Sharpness named tag captured, got {:?}",
            m.named_tags
        );
        assert!(
            m.named_tags
                .iter()
                .any(|(k, v)| k == "Brightness" && v == "42"),
            "Brightness named tag captured, got {:?}",
            m.named_tags
        );

        // The named tags are surfaced into series metadata under `.tag.<name>`.
        let mut sm = HashMap::new();
        insert_cellsens_acquisition_metadata(&mut sm, "cellsens.ets.0", m);
        let get = |k: &str| sm.get(k).map(|v: &MetadataValue| v.to_string());
        assert_eq!(get("cellsens.ets.0.tag.Sharpness").as_deref(), Some("7"));
        assert_eq!(get("cellsens.ets.0.tag.Brightness").as_deref(), Some("42"));
    }

    /// `get_stack_type` and `get_device_subtype` pass unknown/non-numeric values
    /// through unchanged, matching Java's `return type` fallthrough.
    #[test]
    fn vsi_stack_and_device_subtype_passthrough_unknown() {
        assert_eq!(get_stack_type("999"), "999");
        assert_eq!(get_stack_type("not a number"), "not a number");
        assert_eq!(get_device_subtype("12345"), "12345");
        assert_eq!(get_device_subtype("Camera"), "Camera");
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
        assert_eq!(
            vol.assemble_region(0, 0, 0, 0, 0, 0, 2, 2).unwrap(),
            vec![0, 1, 4, 5]
        );
        assert_eq!(
            vol.assemble_region(0, 0, 0, 0, 1, 0, 2, 2).unwrap(),
            vec![1, 0, 5, 0]
        );

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
    fn ets_parse_tolerates_chunk_byte_count_mismatches_like_java() {
        let short_payload = temp_flim2_path("short-raw-tile.ets");
        std::fs::write(
            &short_payload,
            build_synthetic_ets(2, ETS_PT_USHORT, 1, 2, 2, 2, 2),
        )
        .unwrap();
        let vol = CellSensReader::parse_ets(&short_payload).unwrap();
        assert_eq!(vol.tiles[0].2, 2);
        assert_eq!(
            vol.decode_tile(0, 0, 0, 0, 0, 0).unwrap(),
            vec![0, 1, 0, 0, 0, 0, 0, 0]
        );
        let _ = std::fs::remove_file(short_payload);

        let zero_count = temp_flim2_path("zero-raw-count.ets");
        std::fs::write(
            &zero_count,
            build_synthetic_ets(2, ETS_PT_UCHAR, 1, 1, 1, 0, 1),
        )
        .unwrap();
        let vol = CellSensReader::parse_ets(&zero_count).unwrap();
        assert_eq!(vol.decode_tile(0, 0, 0, 0, 0, 0).unwrap(), vec![0]);
        let _ = std::fs::remove_file(zero_count);
    }

    #[test]
    fn ets_parse_rejects_truncated_chunk_table_before_metadata() {
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
        let vol = CellSensReader::parse_ets(&missing_payload).unwrap();
        assert_eq!(vol.decode_tile(0, 0, 0, 0, 0, 0).unwrap(), vec![0, 1, 0, 0]);
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

    #[test]
    fn cellsens_vsi_without_ets_exposes_embedded_tiff_series() {
        let path = temp_flim2_path("no-ets.vsi");
        write_one_pixel_tiff(&path, 0x5a);

        let mut reader = CellSensReader::new();
        reader.set_id(&path).unwrap();

        assert_eq!(
            reader.series_count(),
            1,
            "Java CellSensReader files.size()==1 path exposes embedded TIFF IFDs"
        );
        let meta = reader.metadata();
        assert_eq!(meta.size_x, 1);
        assert_eq!(meta.size_y, 1);
        assert_eq!(meta.image_count, 1);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![0x5a]);

        let _ = std::fs::remove_file(path);
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
                    && namespace.as_deref() == Some("openmicroscopy.org/OriginalMetadata") =>
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

    // -- SPCReader (Becker & Hickl SPC FIFO) ---------------------------------

    /// One 32-bit FIFO word. `b3` is the byte examined at offset `bb` (its
    /// high nibble selects photon/marker); `b1` is `bb-2` (its high nibble is
    /// the routing/mark nibble or the photon channel).
    fn spc_word(b0: u8, b1: u8, b2: u8, b3: u8) -> [u8; 4] {
        [b0, b1, b2, b3]
    }

    /// Frame-clock marker word (0x90 init pattern, rout nibble 0x40).
    fn spc_frame() -> [u8; 4] {
        spc_word(0, 0x40, 0, 0x90)
    }
    /// Line-clock marker word (0x90 init pattern, rout nibble 0x20).
    fn spc_line() -> [u8; 4] {
        spc_word(0, 0x20, 0, 0x90)
    }
    /// Pixel-clock marker word (0x90 init pattern, rout nibble 0x10).
    fn spc_pixel() -> [u8; 4] {
        spc_word(0, 0x10, 0, 0x90)
    }

    /// Build a synthetic .spc FIFO stream tracing a 2-line x 2-pixel x 2-frame
    /// geometry (so nFrames = currentFrame - 1 = 1).
    fn build_spc_stream() -> Vec<u8> {
        // 3 macro-time bytes + 1 routing byte (nChannels=1 => routing 0x08).
        let mut data = vec![0u8, 0u8, 0u8, 0x08u8];
        // Pixel-clock markers (not photons) advance currentPixel, which fixes
        // nPixels when the (frame==0, line==1) line clock is reached.
        let words: Vec<[u8; 4]> = vec![
            spc_frame(), // word0  frameClock[0]=0
            spc_line(),  // word1  endOfFrame[0]
            spc_pixel(), // word2
            spc_pixel(), // word3
            spc_line(),  // word4
            spc_pixel(), // word5
            spc_pixel(), // word6
            spc_frame(), // word7  frameClock[1], sets nLines=2
            spc_line(),  // word8  endOfFrame[1], sets nPixels=2
            spc_pixel(), // word9
            spc_pixel(), // word10
            spc_line(),  // word11
            spc_pixel(), // word12
            spc_pixel(), // word13
            spc_frame(), // word14 frameClock[2]
            spc_line(),  // word15 endOfFrame[2], currentFrame -> 2
        ];
        for w in words {
            data.extend_from_slice(&w);
        }
        data
    }

    /// Build a matching .set file: 8-byte pad, i32 setuppos, i16 setupcount,
    /// a 600-byte region containing "module SPC-830", then the setup text with
    /// SP_TAC_R / SP_TAC_G tags at `setuppos`.
    fn build_set_file() -> Vec<u8> {
        let setup_text = b"#SP [SP_TAC_R,F,50.000] [SP_TAC_G,I,4]";
        let setup_count = setup_text.len() as i16;

        let mut buf = Vec::new();
        buf.extend_from_slice(&[0u8; 8]); // skipped
                                          // placeholder for setuppos (i32) -- fill later
        let setuppos_field = buf.len();
        buf.extend_from_slice(&[0u8; 4]);
        buf.extend_from_slice(&setup_count.to_le_bytes());

        // 600-byte header region containing the module string.
        let mut header = vec![b' '; 600];
        let tagstr = b"FIFO_IMAGE measurement with module SPC-830";
        header[..tagstr.len()].copy_from_slice(tagstr);
        buf.extend_from_slice(&header);

        // setup text begins here.
        let setuppos = buf.len() as i32;
        buf.extend_from_slice(setup_text);

        // backfill setuppos.
        buf[setuppos_field..setuppos_field + 4].copy_from_slice(&setuppos.to_le_bytes());
        buf
    }

    fn unique_spc_base() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("bfrs_spc_{nanos}"))
    }

    #[test]
    fn spc_set_id_and_metadata() {
        let base = unique_spc_base();
        let spc_path = base.with_extension("spc");
        let set_path = base.with_extension("set");
        File::create(&spc_path)
            .unwrap()
            .write_all(&build_spc_stream())
            .unwrap();
        File::create(&set_path)
            .unwrap()
            .write_all(&build_set_file())
            .unwrap();

        let mut reader = SpcReader::new();
        reader.set_id(&spc_path).expect("SPC set_id should succeed");

        let m = reader.metadata();
        // nTimebins = (0xFFF >> 6) + 1 = 64.
        assert_eq!(reader.n_timebins, 64);
        // nFrames = currentFrame - 1 = 1.
        assert_eq!(reader.n_frames, 1);
        assert_eq!(reader.n_lines, 2);
        assert_eq!(reader.n_pixels, 2);
        assert_eq!(reader.n_channels, 1);

        assert_eq!(m.size_x, 2);
        assert_eq!(m.size_y, 2); // nLines < 530 -> image mode
        assert_eq!(m.size_c, 1);
        assert_eq!(m.size_t, 64 * 1);
        assert_eq!(m.size_z, 1);
        assert_eq!(m.image_count, 64);
        assert_eq!(m.pixel_type, PixelType::Uint16);
        assert!(m.is_little_endian);
        assert_eq!(
            m.dimension_order,
            crate::common::metadata::DimensionOrder::XYZTC
        );

        let modulo = m.modulo_t.as_ref().expect("moduloT should be set");
        assert_eq!(modulo.parent_dimension, "T");
        assert_eq!(modulo.modulo_type, "lifetime");
        assert_eq!(modulo.unit, "ps");
        assert_eq!(modulo.start, 0.0);

        // timeBase = 4095 * 50 / (4 * 4096) * 1e12 ; step = timeBase / 64.
        let time_base = 4095.0 * 50.0 / (4.0 * 4096.0) * 1.000e12;
        let expected_step = time_base / 64.0;
        assert!((modulo.step - expected_step).abs() < 1e-3);

        // Global metadata captured.
        assert!(matches!(
            reader.global_meta.get("nChannels"),
            Some(MetadataValue::Int(1))
        ));
        assert!(matches!(
            reader.global_meta.get("time bins"),
            Some(MetadataValue::Int(64))
        ));

        // open_bytes for plane 0 returns a correctly sized uint16 plane.
        let expected_plane_len = (m.size_x * m.size_y * 2) as usize;
        let plane = reader.open_bytes(0).expect("open_bytes should succeed");
        assert_eq!(plane.len(), expected_plane_len);

        // used files are the .set and .spc pair.
        let used = reader.series_used_files();
        assert_eq!(used.len(), 2);

        reader.close().unwrap();
        let _ = std::fs::remove_file(&spc_path);
        let _ = std::fs::remove_file(&set_path);
    }

    #[test]
    fn spc_rejects_out_of_bounds_region_like_java_check_plane_parameters() {
        let base = unique_spc_base();
        let spc_path = base.with_extension("spc");
        let set_path = base.with_extension("set");
        File::create(&spc_path)
            .unwrap()
            .write_all(&build_spc_stream())
            .unwrap();
        File::create(&set_path)
            .unwrap()
            .write_all(&build_set_file())
            .unwrap();

        let mut reader = SpcReader::new();
        reader.set_id(&spc_path).expect("SPC set_id should succeed");

        let err = reader.open_bytes_region(0, 1, 0, 2, 1).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::Format(message) if message.contains("outside image bounds"))
        );

        let _ = std::fs::remove_file(&spc_path);
        let _ = std::fs::remove_file(&set_path);
    }

    #[test]
    fn spc_rejects_wrong_module() {
        // A .set with an unrecognised module string must be rejected.
        let base = unique_spc_base();
        let spc_path = base.with_extension("spc");
        let set_path = base.with_extension("set");
        File::create(&spc_path)
            .unwrap()
            .write_all(&build_spc_stream())
            .unwrap();

        let mut set = build_set_file();
        // Corrupt "SPC-830" -> "SPC-999" inside the header region.
        if let Some(pos) = set.windows(7).position(|w| w == b"SPC-830") {
            set[pos..pos + 7].copy_from_slice(b"SPC-999");
        }
        File::create(&set_path).unwrap().write_all(&set).unwrap();

        let mut reader = SpcReader::new();
        let err = reader.set_id(&spc_path).unwrap_err();
        assert!(matches!(err, BioFormatsError::Format(_)));

        let _ = std::fs::remove_file(&spc_path);
        let _ = std::fs::remove_file(&set_path);
    }

    #[test]
    fn spc_is_this_type_by_name_requires_companion() {
        let base = unique_spc_base();
        let spc_path = base.with_extension("spc");
        let set_path = base.with_extension("set");
        let reader = SpcReader::new();

        // Neither file exists yet.
        assert!(!reader.is_this_type_by_name(&spc_path));

        File::create(&spc_path).unwrap().write_all(b"x").unwrap();
        // .spc present but .set missing -> not detected.
        assert!(!reader.is_this_type_by_name(&spc_path));

        File::create(&set_path).unwrap().write_all(b"x").unwrap();
        // Both present -> detected by either extension.
        assert!(reader.is_this_type_by_name(&spc_path));
        assert!(reader.is_this_type_by_name(&set_path));

        // SPC FIFO has no byte magic.
        assert!(!reader.is_this_type_by_bytes(b"SPC-830 Data File "));

        let _ = std::fs::remove_file(&spc_path);
        let _ = std::fs::remove_file(&set_path);
    }

    /// Build a synthetic classic (native RAW) Imaris file and exercise
    /// detection, header metadata, and the Y-flipped plane reads.
    #[test]
    fn imaris_classic_raw_roundtrip() {
        let size_x: usize = 3;
        let size_y: usize = 2;
        let size_z: usize = 2;
        let size_c: usize = 2;

        // All header integers/floats are big-endian (IS_LITTLE = false).
        let mut buf: Vec<u8> = Vec::new();
        let push_str = |buf: &mut Vec<u8>, s: &str, n: usize| {
            let mut field = vec![0u8; n];
            let bytes = s.as_bytes();
            field[..bytes.len().min(n)].copy_from_slice(&bytes[..bytes.len().min(n)]);
            buf.extend_from_slice(&field);
        };

        buf.extend_from_slice(&5021964i32.to_be_bytes()); // magic
        buf.extend_from_slice(&7i32.to_be_bytes()); // version
        buf.extend_from_slice(&[0u8; 4]); // skip 4
        push_str(&mut buf, "MyImage", 128); // imageName

        buf.extend_from_slice(&(size_x as i16).to_be_bytes());
        buf.extend_from_slice(&(size_y as i16).to_be_bytes());
        buf.extend_from_slice(&(size_z as i16).to_be_bytes());
        buf.extend_from_slice(&[0u8; 2]); // skip 2
        buf.extend_from_slice(&(size_c as i32).to_be_bytes());
        buf.extend_from_slice(&[0u8; 2]); // skip 2

        push_str(&mut buf, "2026-01-01", 32); // date

        buf.extend_from_slice(&0.5f32.to_be_bytes()); // dx
        buf.extend_from_slice(&0.25f32.to_be_bytes()); // dy
        buf.extend_from_slice(&1.5f32.to_be_bytes()); // dz
        buf.extend_from_slice(&63i16.to_be_bytes()); // mag

        push_str(&mut buf, "a comment", 128); // description
        buf.extend_from_slice(&0i32.to_be_bytes()); // isSurvey (0 => survey performed = true)

        // Per-channel records (164 bytes each).
        for c in 0..size_c {
            push_str(&mut buf, &format!("Channel {c}"), 128);
            buf.extend_from_slice(&((c as f32) + 1.0).to_be_bytes()); // gain
            buf.extend_from_slice(&0.0f32.to_be_bytes()); // detector offset
            buf.extend_from_slice(&(((c as f32) + 1.0) * 10.0).to_be_bytes()); // pinhole
            buf.extend_from_slice(&[0u8; 24]); // skip 24
        }

        // The fields Java reads occupy 332 + 164*sizeC bytes, but Java seeks pixel
        // data using the constant `offset = 336 + 164*sizeC` (a 4-byte gap that is
        // part of the on-disk format). Pad so the synthetic file matches that.
        assert_eq!(buf.len(), 332 + 164 * size_c);
        buf.extend_from_slice(&[0u8; 4]);
        assert_eq!(buf.len(), 336 + 164 * size_c);

        // Pixel data: channel-major, then Z, each plane size_x*size_y bytes.
        // Encode the byte value as (plane_index * 100 + row * 10 + col) so we can
        // verify both plane indexing and the Y-flip.
        for plane in 0..(size_c * size_z) {
            for row in 0..size_y {
                for col in 0..size_x {
                    buf.push((plane * 100 + row * 10 + col) as u8);
                }
            }
        }

        // Write to a temp file.
        let mut path = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        path.push(format!("imaris_classic_{nanos}.ims"));
        File::create(&path).unwrap().write_all(&buf).unwrap();

        let mut reader = ImarisReader::new();

        // Detection: magic must be recognised; foreign bytes rejected.
        assert!(reader.is_this_type_by_bytes(&buf[..16]));
        assert!(!reader.is_this_type_by_bytes(&[0, 0, 0, 0]));
        assert!(reader.is_this_type_by_name(&path));

        reader.set_id(&path).unwrap();

        let m = reader.metadata();
        assert_eq!(m.size_x, size_x as u32);
        assert_eq!(m.size_y, size_y as u32);
        assert_eq!(m.size_z, size_z as u32);
        assert_eq!(m.size_c, size_c as u32);
        assert_eq!(m.size_t, 1);
        assert_eq!(m.image_count, (size_c * size_z) as u32);
        assert_eq!(m.pixel_type, PixelType::Uint8);
        assert!(!m.is_little_endian);
        assert_eq!(
            m.dimension_order,
            crate::common::metadata::DimensionOrder::XYZCT
        );

        // Global metadata captured.
        assert_eq!(
            m.series_metadata.get("Image name").map(|v| v.to_string()),
            Some("MyImage".to_string())
        );
        assert_eq!(
            m.series_metadata.get("Version").map(|v| v.to_string()),
            Some("7".to_string())
        );
        assert_eq!(
            m.series_metadata
                .get("Channel #0 Comment")
                .map(|v| v.to_string()),
            Some("Channel 0".to_string())
        );
        assert_eq!(
            m.series_metadata
                .get("Channel #1 Comment")
                .map(|v| v.to_string()),
            Some("Channel 1".to_string())
        );

        // OME metadata: physical sizes, channel pinholes/gains.
        let ome = reader.ome_metadata().expect("ome metadata");
        let img = &ome.images[0];
        assert_eq!(img.name.as_deref(), Some("MyImage"));
        assert_eq!(img.description.as_deref(), Some("a comment"));
        assert_eq!(img.physical_size_x, Some(0.5));
        assert_eq!(img.physical_size_z, Some(1.5));
        assert_eq!(img.channels.len(), size_c);
        assert_eq!(img.channels[0].pinhole_size, Some(10.0));
        assert_eq!(img.channels[1].pinhole_size, Some(20.0));
        assert_eq!(img.channels[0].detector_settings_gain, Some(1.0));

        // Plane reads with Y-flip. The file stores file-row k at output row
        // (size_y - 1 - k), so the encoded value `plane*100 + filerow*10 + col`
        // lands at output row `size_y - 1 - filerow`.
        for plane in 0..(size_c * size_z) {
            let data = reader.open_bytes(plane as u32).unwrap();
            assert_eq!(data.len(), size_x * size_y);
            for out_row in 0..size_y {
                for col in 0..size_x {
                    let file_row = size_y - 1 - out_row;
                    let expected = (plane * 100 + file_row * 10 + col) as u8;
                    assert_eq!(
                        data[out_row * size_x + col],
                        expected,
                        "plane {plane} out_row {out_row} col {col}"
                    );
                }
            }
        }

        let _ = std::fs::remove_file(&path);
    }
}
