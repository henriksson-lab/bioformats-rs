//! Additional FLIM, flow cytometry, and miscellaneous imaging format readers.
//!
//! Includes FlowSightReader with basic binary header inspection and many
//! extension-only placeholder readers.

use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::io::read_bytes_at;
use crate::common::metadata::ImageMetadata;
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::tiff::ifd::{tag, Ifd};
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
                    concat!(stringify!($name), " format reading is not yet implemented").to_string()
                ))
            }

            fn close(&mut self) -> Result<()> {
                self.path = None;
                self.meta = None;
                Ok(())
            }

            fn series_count(&self) -> usize { 1 }

            fn set_series(&mut self, s: usize) -> Result<()> {
                if s != 0 { Err(BioFormatsError::SeriesOutOfRange(s)) } else { Ok(()) }
            }

            fn series(&self) -> usize { 0 }

            fn metadata(&self) -> &ImageMetadata {
                self.meta.as_ref().expect("set_id not called")
            }

            fn open_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
                Err(BioFormatsError::UnsupportedFormat(
                    concat!(stringify!($name), " format reading is not yet implemented").to_string()
                ))
            }

            fn open_bytes_region(&mut self, _plane_index: u32, _x: u32, _y: u32, _w: u32, _h: u32) -> Result<Vec<u8>> {
                Err(BioFormatsError::UnsupportedFormat(
                    concat!(stringify!($name), " format reading is not yet implemented").to_string()
                ))
            }

            fn open_thumb_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
                Err(BioFormatsError::UnsupportedFormat(
                    concat!(stringify!($name), " format reading is not yet implemented").to_string()
                ))
            }
        }
    };
}

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
                    concat!(stringify!($name), " format reading is not yet implemented").to_string()
                ))
            }

            fn close(&mut self) -> Result<()> {
                self.path = None;
                self.meta = None;
                Ok(())
            }

            fn series_count(&self) -> usize { 1 }

            fn set_series(&mut self, s: usize) -> Result<()> {
                if s != 0 { Err(BioFormatsError::SeriesOutOfRange(s)) } else { Ok(()) }
            }

            fn series(&self) -> usize { 0 }

            fn metadata(&self) -> &ImageMetadata {
                self.meta.as_ref().expect("set_id not called")
            }

            fn open_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
                Err(BioFormatsError::UnsupportedFormat(
                    concat!(stringify!($name), " format reading is not yet implemented").to_string()
                ))
            }

            fn open_bytes_region(&mut self, _plane_index: u32, _x: u32, _y: u32, _w: u32, _h: u32) -> Result<Vec<u8>> {
                Err(BioFormatsError::UnsupportedFormat(
                    concat!(stringify!($name), " format reading is not yet implemented").to_string()
                ))
            }

            fn open_thumb_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
                Err(BioFormatsError::UnsupportedFormat(
                    concat!(stringify!($name), " format reading is not yet implemented").to_string()
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
    let declared = ifd0
        .get_u32(FLOWSIGHT_CHANNEL_COUNT_TAG)
        .unwrap_or(1)
        .max(1) as usize;
    if let Some(names) = ifd0.get_str(FLOWSIGHT_CHANNEL_NAMES_TAG) {
        let count = split_flowsight_pipe_list(names).len();
        if count > 0 {
            return count;
        }
    }
    if let Some(xml) = ifd0.get_str(FLOWSIGHT_METADATA_XML_TAG) {
        if let Some(count) = count_flowsight_channels_in_use(xml) {
            return count.max(1);
        }
    }
    declared
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
                    value |= -((1i16).wrapping_shl(shift));
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
// 2. Amnis/Luminex IM3 — 64x64 uint16 placeholder
// ---------------------------------------------------------------------------
placeholder_reader_u16_small! {
    /// Amnis/Luminex IM3 format placeholder reader (`.im3`).
    pub struct Im3Reader;
    extensions: ["im3"];
    magic_bytes: false;
}

// ---------------------------------------------------------------------------
// 3. 3i SlideBook 7 — 64x64 uint16 placeholder
// ---------------------------------------------------------------------------
placeholder_reader_u16_small! {
    /// 3i SlideBook 7 format placeholder reader (`.sld`).
    pub struct SlideBook7Reader;
    extensions: ["sld"];
    magic_bytes: false;
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
            let name = r.ifd(0).and_then(|ifd| ifd.get_str(NDPI_TAG_CHANNEL).map(str::to_owned));
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
///
/// iVision is a proprietary format from BioVision Technologies with
/// undocumented binary structure.
pub struct IvisionReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl IvisionReader {
    pub fn new() -> Self {
        IvisionReader {
            path: None,
            meta: None,
        }
    }
}

impl Default for IvisionReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for IvisionReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("ipm"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, _path: &Path) -> Result<()> {
        Err(BioFormatsError::UnsupportedFormat(
            "iVision IPM is a proprietary format from BioVision Technologies".to_string(),
        ))
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        Ok(())
    }

    fn series_count(&self) -> usize {
        1
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
        self.meta.as_ref().expect("set_id not called")
    }

    fn open_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "iVision IPM is a proprietary format from BioVision Technologies".to_string(),
        ))
    }

    fn open_bytes_region(
        &mut self,
        _plane_index: u32,
        _x: u32,
        _y: u32,
        _w: u32,
        _h: u32,
    ) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "iVision IPM is a proprietary format from BioVision Technologies".to_string(),
        ))
    }

    fn open_thumb_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "iVision IPM is a proprietary format from BioVision Technologies".to_string(),
        ))
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
        let comment = self
            .inner
            .ifd(0)
            .and_then(|ifd| ifd.get_str(crate::tiff::ifd::tag::IMAGE_DESCRIPTION).map(str::to_owned));
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
// 8. Leica XLEF — TIFF delegate
// ---------------------------------------------------------------------------
/// Leica XLEF format reader (`.xlef`).
///
/// XLEF files contain embedded TIFF data; delegates to `TiffReader`.
pub struct XlefReader {
    inner: crate::tiff::TiffReader,
}

impl XlefReader {
    pub fn new() -> Self {
        XlefReader {
            inner: crate::tiff::TiffReader::new(),
        }
    }
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
// 9. Olympus OIR
// ---------------------------------------------------------------------------
/// Olympus OIR format reader (`.oir`).
///
/// Olympus OIR format requires OLE2 container parsing with proprietary
/// internal structure specific to Olympus FluoView software.
pub struct OirReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl OirReader {
    pub fn new() -> Self {
        OirReader {
            path: None,
            meta: None,
        }
    }
}

impl Default for OirReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for OirReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("oir"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, _path: &Path) -> Result<()> {
        Err(BioFormatsError::UnsupportedFormat(
            "Olympus OIR format requires OLE2 container parsing with proprietary Olympus FluoView structure".to_string()
        ))
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        Ok(())
    }

    fn series_count(&self) -> usize {
        1
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
        self.meta.as_ref().expect("set_id not called")
    }

    fn open_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "Olympus OIR format requires OLE2 container parsing".to_string(),
        ))
    }

    fn open_bytes_region(
        &mut self,
        _plane_index: u32,
        _x: u32,
        _y: u32,
        _w: u32,
        _h: u32,
    ) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "Olympus OIR format requires OLE2 container parsing".to_string(),
        ))
    }

    fn open_thumb_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "Olympus OIR format requires OLE2 container parsing".to_string(),
        ))
    }
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
/// Pixel assembly for the ETS pyramid is only attempted for uncompressed
/// (`RAW`) tiles whose tile coordinate has no extra (C/Z/T/resolution)
/// dimensions, because the per-dimension ordering is stored in the VSI's
/// proprietary tag tree (a non-OLE2 binary structure not yet parsed here).
/// JPEG / JPEG-2000 / JPEG-lossless tiles require codecs not wired in. These
/// limitations are recorded in series metadata under `cellsens.*`. Tag 700-style
/// metadata and label/overview images continue to be served by the inner TIFF.
pub struct CellSensReader {
    inner: crate::tiff::TiffReader,
    ets: Vec<EtsVolume>,
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
    /// (coordinate vector, file offset, byte count) for each used chunk.
    tiles: Vec<(Vec<i32>, u64, u32)>,
}

const ETS_RAW: i32 = 0;

impl CellSensReader {
    pub fn new() -> Self {
        CellSensReader {
            inner: crate::tiff::TiffReader::new(),
            ets: Vec::new(),
        }
    }

    /// Locate `frame_*.ets` files in the `_<name>_/<stack>/` pixel directories
    /// next to the `.vsi`. Mirrors the directory walk in `initFile`.
    fn find_ets_files(vsi_path: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let Some(dir) = vsi_path.parent() else { return out };
        let stem = vsi_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        let pixels_dir = dir.join(format!("_{}_", stem));
        let Ok(stacks) = std::fs::read_dir(&pixels_dir) else { return out };
        let mut stack_dirs: Vec<PathBuf> = stacks
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.is_dir())
            .collect();
        stack_dirs.sort();
        for stack in stack_dirs {
            if let Ok(files) = std::fs::read_dir(&stack) {
                let mut paths: Vec<PathBuf> = files.filter_map(|e| e.ok().map(|e| e.path())).collect();
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
        let rd = |off: usize, n: usize| -> Option<&[u8]> { bytes.get(off..off + n) };
        let u32_at = |off: usize| -> u32 {
            rd(off, 4)
                .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                .unwrap_or(0)
        };
        let i32_at = |off: usize| -> i32 { u32_at(off) as i32 };
        let u64_at = |off: usize| -> u64 {
            rd(off, 8)
                .map(|b| {
                    u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
                })
                .unwrap_or(0)
        };

        // Volume header (offset 0): "SIS" magic, then ints/longs.
        let magic = String::from_utf8_lossy(rd(0, 4).unwrap_or(&[])).trim().to_string();
        if magic != "SIS" {
            return Err(BioFormatsError::Format(format!(
                "ETS file {:?}: unexpected magic {:?}",
                path, magic
            )));
        }
        // headerSize(4) version(8) nDimensions(12) addHeaderOffset(16, long)
        // addHeaderSize(24) reserved(28) usedChunkOffset(32, long) nUsedChunks(40)
        let n_dimensions = u32_at(12);
        let additional_header_offset = u64_at(16) as usize;
        let used_chunk_offset = u64_at(32) as usize;
        let n_used_chunks = u32_at(40) as usize;

        // Additional header (additionalHeaderOffset): "ETS" magic.
        let more_magic = String::from_utf8_lossy(rd(additional_header_offset, 4).unwrap_or(&[]))
            .trim()
            .to_string();
        if more_magic != "ETS" {
            return Err(BioFormatsError::Format(format!(
                "ETS file {:?}: unexpected secondary magic {:?}",
                path, more_magic
            )));
        }
        // skip 4 (extra version), then pixelType(int), sizeC(int), colorspace(int),
        // compression(int), quality(int), tileX(int), tileY(int), tileZ(int)
        let base = additional_header_offset + 8;
        let pixel_type_code = i32_at(base);
        let size_c = u32_at(base + 4);
        let compression = i32_at(base + 12);
        let tile_x = u32_at(base + 20);
        let tile_y = u32_at(base + 24);

        // Used-chunk table at usedChunkOffset. Each entry:
        //   skip 4; nDimensions * int coordinate; long offset; int nBytes; skip 4.
        let mut tiles = Vec::with_capacity(n_used_chunks);
        let mut off = used_chunk_offset;
        for _ in 0..n_used_chunks {
            off += 4;
            let mut coord = Vec::with_capacity(n_dimensions as usize);
            for _ in 0..n_dimensions {
                coord.push(i32_at(off));
                off += 4;
            }
            let tile_offset = u64_at(off);
            off += 8;
            let n_bytes = u32_at(off);
            off += 4;
            off += 4; // reserved
            tiles.push((coord, tile_offset, n_bytes));
        }

        Ok(EtsVolume {
            path: path.to_path_buf(),
            n_dimensions,
            size_c,
            compression,
            tile_x,
            tile_y,
            pixel_type_code,
            tiles,
        })
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
            match Self::parse_ets(f) {
                Ok(v) => volumes.push(v),
                Err(_) => {}
            }
        }
        if volumes.is_empty() {
            return;
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
                s.metadata.series_metadata.insert(
                    format!("{p}.size_c"),
                    MetadataValue::Int(v.size_c as i64),
                );
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
                let _ = v.pixel_type_code;
            }
        }
        self.ets = volumes;
    }
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
        self.inner.set_id(path).map_err(|_| {
            BioFormatsError::UnsupportedFormat(
                "Olympus cellSens VSI: could not parse as TIFF (may require ETS companion files)"
                    .to_string(),
            )
        })?;
        self.enrich_metadata(path);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.ets.clear();
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
// 11. Volocity clipping ACFF
// ---------------------------------------------------------------------------
/// Volocity clipping format reader (`.acff`).
///
/// Volocity clipping files use OLE2/Compound Document format which requires
/// a dedicated OLE2 container parser.
pub struct VolocityClippingReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl VolocityClippingReader {
    pub fn new() -> Self {
        VolocityClippingReader {
            path: None,
            meta: None,
        }
    }
}

impl Default for VolocityClippingReader {
    fn default() -> Self {
        Self::new()
    }
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
        false
    }

    fn set_id(&mut self, _path: &Path) -> Result<()> {
        Err(BioFormatsError::UnsupportedFormat(
            "Volocity clipping format requires OLE2/Compound Document container parsing"
                .to_string(),
        ))
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        Ok(())
    }

    fn series_count(&self) -> usize {
        1
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
        self.meta.as_ref().expect("set_id not called")
    }

    fn open_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "Volocity clipping format requires OLE2/Compound Document container parsing"
                .to_string(),
        ))
    }

    fn open_bytes_region(
        &mut self,
        _plane_index: u32,
        _x: u32,
        _y: u32,
        _w: u32,
        _h: u32,
    ) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "Volocity clipping format requires OLE2/Compound Document container parsing"
                .to_string(),
        ))
    }

    fn open_thumb_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "Volocity clipping format requires OLE2/Compound Document container parsing"
                .to_string(),
        ))
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
        let mut pixels_offset = 0u64;
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
                current_type = line[line.find(' ').map(|i| i + 1).unwrap_or(line.len())..]
                    .to_string();
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
                    pixels_offset = body_offset;
                } else if current_type == "text/xml" {
                    let start = body_offset as usize;
                    let end = (start + current_length).min(bytes.len());
                    if start <= end {
                        xml_blocks
                            .push(String::from_utf8_lossy(&bytes[start..end]).into_owned());
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

        meta.image_count = meta.size_z.max(1) * meta.size_t.max(1);
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
        1
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
        self.meta.as_ref().expect("set_id not called")
    }
    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if p >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(p));
        }
        let bpp = (meta.bits_per_pixel as usize + 7) / 8;
        let plane = meta.size_x as usize * meta.size_y as usize * bpp;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut reader = BufReader::new(File::open(path).map_err(BioFormatsError::Io)?);
        read_bytes_at(&mut reader, self.pixels_offset + (p as u64 * plane as u64), plane)
    }
    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?.clone();
        let bpp = (meta.bits_per_pixel as usize + 7) / 8;
        let full = self.open_bytes(p)?;
        let full_w = meta.size_x as usize;
        let out_row = w as usize * bpp;
        let mut out = Vec::with_capacity(out_row * h as usize);
        for row in 0..h as usize {
            let src_row = (y as usize + row) * full_w * bpp;
            let start = src_row + x as usize * bpp;
            let end = start + out_row;
            if end <= full.len() {
                out.extend_from_slice(&full[start..end]);
            } else {
                out.resize(out.len() + out_row, 0);
            }
        }
        Ok(out)
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
            if let Some(name) = ifd.get_str(SLIDEBOOK_CHANNEL_TAG) {
                // Java strips a "prefix:" and a ";suffix".
                let mut n = name;
                if let Some(p) = n.find(':') {
                    n = &n[p + 1..];
                }
                if let Some(p) = n.find(';') {
                    n = &n[..p];
                }
                channel_name = Some(n.trim().to_string());
            }
            if let Some(p) = ifd.get_str(SLIDEBOOK_PHYSICAL_SIZE_TAG).and_then(|s| s.trim().parse::<f64>().ok()) {
                if p > 0.0 {
                    vendor.push(("slidebook.physical_size_x".into(), MetadataValue::Float(p)));
                    vendor.push(("slidebook.physical_size_y".into(), MetadataValue::Float(p)));
                }
            }
            if let Some(mag) = ifd.get_str(SLIDEBOOK_MAGNIFICATION_TAG).and_then(|s| s.trim().parse::<f64>().ok()) {
                vendor.push(("slidebook.magnification".into(), MetadataValue::Float(mag)));
            }
            for (tag, key) in [
                (SLIDEBOOK_X_POS_TAG, "slidebook.position_x"),
                (SLIDEBOOK_Y_POS_TAG, "slidebook.position_y"),
                (SLIDEBOOK_Z_POS_TAG, "slidebook.position_z"),
            ] {
                if let Some(v) = ifd.get_str(tag).and_then(|s| s.trim().parse::<f64>().ok()) {
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
}
