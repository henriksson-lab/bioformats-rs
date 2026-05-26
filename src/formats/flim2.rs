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
/// Full ETS pyramid assembly is implemented here: each `.ets` volume is exposed
/// as an additional series after the inner TIFF's series. For every volume the
/// reader reconstructs the resolution levels (the last tile coordinate when
/// `usePyramid` is set), computes per-level tile grids and plane sizes following
/// the Java halving rules, and assembles tiles into a full plane on
/// `open_bytes`. Tiles are decoded according to the ETS compression code: RAW,
/// JPEG, JPEG-2000 and JPEG-lossless reuse codec.rs decoders. PNG/BMP tile
/// codecs are not wired in and produce an `UnsupportedFormat` error rather than
/// wrong pixels. Tag 700-style metadata and label/overview images continue to be
/// served by the inner TIFF.
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
        let cols0 = if max_x[0] >= 1 { (max_x[0] + 1) as u32 } else { 1 };
        let rows0 = if max_y[0] >= 1 { (max_y[0] + 1) as u32 } else { 1 };
        let base_c = self.size_c * if max_c[0] > 0 { (max_c[0] + 1) as u32 } else { 1 };
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
            let cols = if max_x[i] >= 1 { (max_x[i] + 1) as u32 } else { 1 };
            let rows = if max_y[i] >= 1 { (max_y[i] + 1) as u32 } else { 1 };
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
            let sc = self.size_c * if max_c[i] > 0 { (max_c[i] + 1) as u32 } else { 1 };
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

    fn pixel_type(&self) -> PixelType {
        convert_ets_pixel_type(self.pixel_type_code).unwrap_or(PixelType::Uint8)
    }

    /// RGB channel count: ETS stores all channels in one tile when sizeC > 1.
    fn rgb_channels(&self) -> u32 {
        self.size_c.max(1)
    }

    /// Byte length of one decoded tile.
    fn tile_size(&self) -> usize {
        self.pixel_type().bytes_per_sample()
            * self.rgb_channels() as usize
            * self.tile_x as usize
            * self.tile_y as usize
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
        let tile_size = self.tile_size();
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
        // For PNG/BMP the compressed size is the byte count; otherwise read the
        // full tile_size of raw/codestream bytes.
        let read_len = match self.compression {
            ETS_PNG | ETS_BMP => n_bytes as usize,
            _ => tile_size.max(n_bytes as usize),
        };
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

        if buf.len() < tile_size {
            buf.resize(tile_size, 0);
        } else if buf.len() > tile_size {
            buf.truncate(tile_size);
        }

        // BGR -> RGB swap for RAW component-order-1 multichannel tiles.
        if self.bgr && self.rgb_channels() >= 3 {
            let bpp = self.pixel_type().bytes_per_sample();
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
        let bpp = self.pixel_type().bytes_per_sample();
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
                    if input_offset + row_len <= tile.len()
                        && output_offset + row_len <= out.len()
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
    fn level_metadata(&self, resolution: usize) -> Option<ImageMetadata> {
        let level = self.levels.get(resolution)?;
        let pt = self.pixel_type();
        let channels = self.rgb_channels();
        let image_count =
            level.size_z * level.size_t * (level.size_c / channels.max(1)).max(1);
        Some(ImageMetadata {
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
            is_little_endian: true,
            resolution_count: self.levels.len() as u32,
            ..ImageMetadata::default()
        })
    }
}

// ---- VSI proprietary tag-tree parser (CellSensReader.java:1589-2079) --------
//
// The base `.vsi` is a TIFF whose first IFD also points (at byte offset 8) to a
// proprietary tag-tree describing each `Pyramid` (image) block: its exact
// full-resolution width/height (IMAGE_BOUNDARY), the tile-origin crop
// (TILE_ORIGIN) and the canonical dimension ordering. This is a focused port of
// the tree walk that collects only those geometry fields; the large body of
// per-device acquisition metadata tags is intentionally not ported (reported as
// remaining work).

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
        self.rd(off, 2).map_or(0, |b| i16::from_le_bytes([b[0], b[1]]))
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
    fn read_tags(&mut self, fp: i64, populate: bool) -> i64 {
        if self.depth > 64 {
            return fp;
        }
        self.depth += 1;
        let end = self.read_tags_inner(fp, populate);
        self.depth -= 1;
        end
    }

    fn read_tags_inner(&mut self, container_fp: i64, _populate: bool) -> i64 {
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
                let end_pointer = cur + data_size as i64;
                let mut child = cur;
                while child < end_pointer && child < self.len() {
                    let start = child;
                    let end = self.read_tags(child, true);
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
                && (real_type == VSI_PROPERTY_SET_VOLUME
                    || real_type == VSI_NEW_MDIM_VOLUME_HEADER)
            {
                self.read_tags(cur, tag != 2037);
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
                        self.capture_metadata(tag, v);
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

            // Navigation (CellSensReader.java:2063-2073).
            if next_field == 0 || tag == -494804095 {
                // Java seeks to fp + dataSize + 32 here before returning.
                let resume = fp + data_size as i64 + 32;
                if resume + data_size as i64 + 32 < self.len() && resume >= 0 {
                    return resume;
                }
                return fp;
            }
            if fp + next_field < self.len() && fp + next_field >= 0 {
                fp += next_field;
            } else {
                break;
            }
        }
        fp
    }

    /// Capture non-geometry acquisition metadata for the current pyramid.
    /// Mirrors the metadata dispatch in `readTags` (CellSensReader.java:1881-1979).
    ///
    /// The `tagPrefix`-gated tags (channel wavelength, Z start/increment/value,
    /// timestamp via the generic VALUE tag, and the EXPOSURE_TIME prefix split)
    /// are NOT ported: this parser does not reconstruct the recursive tag-name
    /// prefix Java builds while descending volumes, so those values cannot be
    /// disambiguated here. EXPOSURE_TIME is captured generically.
    fn capture_metadata(&mut self, tag: i32, value: &str) {
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
            VSI_EXPOSURE_TIME => {
                if let Some(n) = as_i64() {
                    m.exposure_times.push(n);
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
                    f64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]).to_string(),
                )
            }
            VSI_BOOLEAN => Some((self.rd(off, 1).map(|b| b[0]).unwrap_or(0) != 0).to_string()),
            VSI_TCHAR | VSI_UNICODE_TCHAR => {
                let n = data_size.max(0) as usize;
                let bytes = self.rd(off, n)?;
                Some(String::from_utf8_lossy(bytes).replace('\0', "").trim().to_string())
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
        parser.read_tags(8, false);
        parser.pyramids
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
        // compression(int), quality(int), tileX(int), tileY(int), tileZ(int),
        // skip 4*17 (pixel info hints), color[sizeC*bpp], skip(40-color),
        // componentOrder(int), usePyramid(int).
        let base = additional_header_offset + 8;
        let pixel_type_code = i32_at(base);
        let size_c = u32_at(base + 4);
        let compression = i32_at(base + 12);
        let tile_x = u32_at(base + 20);
        let tile_y = u32_at(base + 24);
        let pixel_type = convert_ets_pixel_type(pixel_type_code)?;
        let bpp = pixel_type.bytes_per_sample();
        // color region begins at base + 32 + 68 = base + 100, always 40 bytes.
        let color_start = base + 32 + 4 * 17;
        let color_len = (size_c as usize).saturating_mul(bpp).min(40);
        let background = rd(color_start, color_len).map(|b| b.to_vec()).unwrap_or_default();
        let component_order = i32_at(color_start + 40);
        let use_pyramid = i32_at(color_start + 44) != 0;
        let bgr = component_order == 1 && compression == ETS_RAW;

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
            match Self::parse_ets(f) {
                Ok(v) => volumes.push(v),
                Err(_) => {}
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
        let has_orphan_ets = pyramids.len() < volumes.len();
        if has_orphan_ets {
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
                        vol.compute_levels();
                        matched.push(vol);
                    }
                    // No matching metadata block: this is an orphan ETS file. Drop
                    // it entirely (CellSensReader.java:1350-1363).
                    None => {}
                }
            }
            volumes = matched;
        } else if pyramids.len() == volumes.len() {
            for (vol, p) in volumes.iter_mut().zip(pyramids.iter()) {
                vol.pyramid_width = p.width;
                vol.pyramid_height = p.height;
                vol.tile_origin_x = p.tile_origin_x;
                vol.tile_origin_y = p.tile_origin_y;
                vol.dim_order = p.dim_order;
                vol.meta = p.meta.clone();
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
                let m = &v.meta;
                let sm = &mut s.metadata.series_metadata;
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
                                format!("{p}.{key}"),
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
                        sm.insert(format!("{p}.{key}"), MetadataValue::Float(x));
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
                        sm.insert(format!("{p}.{key}"), MetadataValue::Int(x));
                    }
                }
                let _ = &m.objective_types;
            }
        }
        self.ets = volumes;
    }

    /// Resolve a global series index into either the inner TIFF or an ETS volume.
    fn resolve_series(&self, s: usize) -> Option<CellSensTarget> {
        if s < self.tiff_series {
            Some(CellSensTarget::Tiff(s))
        } else if s - self.tiff_series < self.ets.len() {
            Some(CellSensTarget::Ets {
                volume: s - self.tiff_series,
                resolution: 0,
            })
        } else {
            None
        }
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
        self.tiff_series = self.inner.series_count();
        self.target = CellSensTarget::Tiff(self.inner.series());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.ets.clear();
        self.tiff_series = 0;
        self.target = CellSensTarget::Tiff(0);
        self.ets_meta = None;
        self.inner.close()
    }
    fn series_count(&self) -> usize {
        self.tiff_series + self.ets.len()
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        match self.resolve_series(s) {
            Some(CellSensTarget::Tiff(ts)) => {
                self.inner.set_series(ts)?;
                self.target = CellSensTarget::Tiff(ts);
                self.ets_meta = None;
                Ok(())
            }
            Some(CellSensTarget::Ets { volume, .. }) => {
                self.target = CellSensTarget::Ets {
                    volume,
                    resolution: 0,
                };
                self.ets_meta = self.ets[volume].level_metadata(0);
                Ok(())
            }
            None => Err(BioFormatsError::SeriesOutOfRange(s)),
        }
    }
    fn series(&self) -> usize {
        match self.target {
            CellSensTarget::Tiff(ts) => ts,
            CellSensTarget::Ets { volume, .. } => self.tiff_series + volume,
        }
    }
    fn metadata(&self) -> &ImageMetadata {
        match self.target {
            CellSensTarget::Tiff(_) => self.inner.metadata(),
            CellSensTarget::Ets { .. } => self
                .ets_meta
                .as_ref()
                .unwrap_or_else(|| self.inner.metadata()),
        }
    }
    fn resolution_count(&self) -> usize {
        match self.target {
            CellSensTarget::Tiff(_) => self.inner.resolution_count(),
            CellSensTarget::Ets { volume, .. } => self.ets[volume].levels.len().max(1),
        }
    }
    fn set_resolution(&mut self, level: usize) -> Result<()> {
        match self.target {
            CellSensTarget::Tiff(_) => self.inner.set_resolution(level),
            CellSensTarget::Ets { volume, .. } => {
                if level >= self.ets[volume].levels.len() {
                    return Err(BioFormatsError::PlaneOutOfRange(level as u32));
                }
                self.target = CellSensTarget::Ets {
                    volume,
                    resolution: level,
                };
                self.ets_meta = self.ets[volume].level_metadata(level);
                Ok(())
            }
        }
    }
    fn resolution(&self) -> usize {
        match self.target {
            CellSensTarget::Tiff(_) => self.inner.resolution(),
            CellSensTarget::Ets { resolution, .. } => resolution,
        }
    }
    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        match self.target {
            CellSensTarget::Tiff(_) => self.inner.open_bytes(p),
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
            CellSensTarget::Tiff(_) => self.inner.open_bytes_region(p, x, y, w, h),
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

        for (i, f) in fields.iter().enumerate() {
            let record_len = 16 + f.data.len() as i64;
            let next_field = if i + 1 < fields.len() { record_len } else { 0 };
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
        parser.read_tags(8, false);

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
        parser.read_tags(8, false);
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
        parser.read_tags(8, false);

        assert_eq!(parser.pyramids.len(), 1);
        let m = &parser.pyramids[0].meta;
        assert_eq!(m.device_names, vec!["CameraX".to_string()]);
        assert_eq!(m.magnification, Some(40.0));
        assert_eq!(m.numerical_aperture, Some(0.95));
        assert_eq!(m.bit_depth, Some(12));
        assert_eq!(m.exposure_times, vec![25000]);
    }
}
