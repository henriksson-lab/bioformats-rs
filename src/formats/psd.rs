//! Adobe Photoshop PSD/PSB format reader.
//!
//! Supports PSD (version 1) and PSB Large Document (version 2) files.
//! Returns the merged composite image data.

use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata};
use crate::common::ome_metadata::OmeMetadata;
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::validate_region;

const PSD_RESOURCE_TEXT_MAX: usize = 4096;
const PSD_ALPHA_CHANNEL_NAMES_MAX: usize = 256;
const PSD_DISPLAY_INFO_RECORD_BYTES: usize = 14;
const PSD_DISPLAY_INFO_MAX_RECORDS: usize = 64;
const PSD_XMP_MAX_BYTES: usize = 65_536;
const PSD_XMP_MAX_SCALARS: usize = 128;
const PSD_XMP_MAX_DEPTH: usize = 16;
const PSD_XMP_MAX_VALUE_CHARS: usize = 1024;

pub struct PsdReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    /// Decoded composite pixel data, stored **planar** (channel-separated):
    /// all of channel 0's rows, then channel 1's, etc. — matching the on-disk
    /// PSD layout and Java Bio-Formats' channel-separated `openBytes` output.
    pixels: Option<Vec<u8>>,
}

impl PsdReader {
    pub fn new() -> Self {
        PsdReader {
            path: None,
            meta: None,
            pixels: None,
        }
    }
}

impl Default for PsdReader {
    fn default() -> Self {
        Self::new()
    }
}

/// Decode PackBits RLE-encoded data.
fn decode_packbits(src: &[u8], expected_bytes: usize) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(expected_bytes);
    let mut i = 0;
    while i < src.len() && out.len() < expected_bytes {
        let n = src[i] as i8;
        i += 1;
        if n >= 0 {
            // Copy next n+1 bytes literally
            let count = (n as usize) + 1;
            let end = i.checked_add(count).ok_or_else(|| {
                BioFormatsError::InvalidData("PSD PackBits row count overflow".into())
            })?;
            if end > src.len() {
                return Err(BioFormatsError::InvalidData(
                    "PSD PackBits row is truncated".into(),
                ));
            }
            out.extend_from_slice(&src[i..end]);
            i += count;
        } else if n != -128 {
            // Repeat next byte (-n+1) times
            let count = ((-n) as usize) + 1;
            if i >= src.len() {
                return Err(BioFormatsError::InvalidData(
                    "PSD PackBits row is truncated".into(),
                ));
            }
            let val = src[i];
            i += 1;
            for _ in 0..count {
                out.push(val);
            }
        }
        // n == -128: no-op
    }
    if out.len() < expected_bytes {
        return Err(BioFormatsError::InvalidData(
            "PSD PackBits row is shorter than expected".into(),
        ));
    }
    out.truncate(expected_bytes);
    Ok(out)
}

fn pixel_type_from_depth(depth: u16) -> Result<PixelType> {
    match depth {
        8 => Ok(PixelType::Uint8),
        16 => Ok(PixelType::Uint16),
        32 => Ok(PixelType::Uint32),
        64 => Ok(PixelType::Float64),
        _ => Err(BioFormatsError::UnsupportedFormat(format!(
            "PSD unsupported bit depth {depth}"
        ))),
    }
}

/// A minimal big-endian cursor over an in-memory buffer that mirrors the
/// `RandomAccessInputStream` operations used by the Java `PSDReader`. Reads
/// past end-of-buffer clamp the pointer rather than erroring, matching how the
/// Java offset-finding heuristic tolerates short reads.
struct Cur<'a> {
    d: &'a [u8],
    p: usize,
}

impl<'a> Cur<'a> {
    fn new(d: &'a [u8]) -> Self {
        Cur { d, p: 0 }
    }
    fn fp(&self) -> usize {
        self.p
    }
    fn len(&self) -> usize {
        self.d.len()
    }
    fn seek(&mut self, p: usize) {
        self.p = p.min(self.d.len());
    }
    fn skip(&mut self, n: usize) {
        self.p = self.p.saturating_add(n).min(self.d.len());
    }
    fn read_u8(&mut self) -> u8 {
        let v = self.d.get(self.p).copied().unwrap_or(0);
        if self.p < self.d.len() {
            self.p += 1;
        }
        v
    }
    fn read_u16(&mut self) -> u16 {
        let v = if self.p + 2 <= self.d.len() {
            u16::from_be_bytes([self.d[self.p], self.d[self.p + 1]])
        } else {
            0
        };
        self.skip(2);
        v
    }
    fn read_i16(&mut self) -> i16 {
        self.read_u16() as i16
    }
    fn read_u32(&mut self) -> u32 {
        let v = if self.p + 4 <= self.d.len() {
            u32::from_be_bytes([
                self.d[self.p],
                self.d[self.p + 1],
                self.d[self.p + 2],
                self.d[self.p + 3],
            ])
        } else {
            0
        };
        self.skip(4);
        v
    }
    fn read_i32(&mut self) -> i32 {
        self.read_u32() as i32
    }
    fn read_bytes(&mut self, n: usize) -> &'a [u8] {
        let end = (self.p + n).min(self.d.len());
        let s = &self.d[self.p..end];
        self.p = end;
        s
    }
}

fn psd_color_mode_name(mode: u16) -> &'static str {
    match mode {
        0 => "Bitmap",
        1 => "Grayscale",
        2 => "Indexed",
        3 => "RGB",
        4 => "CMYK",
        7 => "Multichannel",
        8 => "Duotone",
        9 => "Lab",
        _ => "Unknown",
    }
}

fn psd_resource_name(id: u16) -> &'static str {
    match id {
        1005 => "ResolutionInfo",
        1006 => "AlphaChannelNames",
        1007 => "DisplayInfo",
        1011 => "PrintFlags",
        1034 => "CopyrightFlag",
        1035 => "Url",
        1037 => "GlobalAngle",
        1039 => "ICCProfile",
        1057 => "VersionInfo",
        1060 => "XmpMetadata",
        1064 => "PixelAspectRatio",
        1065 => "LayerComps",
        2999 => "ClippingPathName",
        7000 => "ImageReadyVariables",
        7001 => "ImageReadyDataSets",
        10000 => "PrintFlagsInformation",
        _ => "Unknown",
    }
}

fn psd_clean_resource_text(bytes: &[u8]) -> Option<String> {
    if bytes.is_empty() || bytes.len() > PSD_RESOURCE_TEXT_MAX {
        return None;
    }
    let text = String::from_utf8_lossy(bytes)
        .chars()
        .map(|ch| if ch == '\0' { ' ' } else { ch })
        .collect::<String>();
    let cleaned = text.trim().to_string();
    if cleaned.is_empty()
        || cleaned
            .chars()
            .any(|ch| ch.is_control() && ch != '\n' && ch != '\r' && ch != '\t')
    {
        None
    } else {
        Some(cleaned)
    }
}

fn psd_read_be_u32(payload: &[u8], offset: &mut usize) -> Option<u32> {
    let end = offset.checked_add(4)?;
    let bytes = payload.get(*offset..end)?;
    *offset = end;
    Some(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn psd_read_be_u16(payload: &[u8], offset: &mut usize) -> Option<u16> {
    let end = offset.checked_add(2)?;
    let bytes = payload.get(*offset..end)?;
    *offset = end;
    Some(u16::from_be_bytes([bytes[0], bytes[1]]))
}

fn psd_read_unicode_string(payload: &[u8], offset: &mut usize) -> Option<String> {
    let length = psd_read_be_u32(payload, offset)? as usize;
    if length > 1024 {
        return None;
    }
    let byte_len = length.checked_mul(2)?;
    let end = offset.checked_add(byte_len)?;
    let bytes = payload.get(*offset..end)?;
    *offset = end;

    let units = bytes
        .chunks_exact(2)
        .map(|chunk| u16::from_be_bytes([chunk[0], chunk[1]]))
        .collect::<Vec<_>>();
    String::from_utf16(&units).ok()
}

fn decode_psd_display_info(
    payload: &[u8],
    metadata: &mut HashMap<String, crate::common::metadata::MetadataValue>,
) {
    let available_records = payload.len() / PSD_DISPLAY_INFO_RECORD_BYTES;
    let record_count = available_records.min(PSD_DISPLAY_INFO_MAX_RECORDS);
    metadata.insert(
        "psd.image_resource.1007.display_info_count".into(),
        crate::common::metadata::MetadataValue::Int(record_count as i64),
    );

    let mut offset = 0usize;
    for index in 0..record_count {
        let Some(color_space) = psd_read_be_u16(payload, &mut offset) else {
            break;
        };
        let Some(color0) = psd_read_be_u16(payload, &mut offset) else {
            break;
        };
        let Some(color1) = psd_read_be_u16(payload, &mut offset) else {
            break;
        };
        let Some(color2) = psd_read_be_u16(payload, &mut offset) else {
            break;
        };
        let Some(color3) = psd_read_be_u16(payload, &mut offset) else {
            break;
        };
        let Some(opacity) = psd_read_be_u16(payload, &mut offset) else {
            break;
        };
        let Some(&kind) = payload.get(offset) else {
            break;
        };
        offset += 2; // kind byte plus reserved padding byte

        let prefix = format!("psd.image_resource.1007.display_info.{index}");
        metadata.insert(
            format!("{prefix}.color_space"),
            crate::common::metadata::MetadataValue::Int(color_space as i64),
        );
        metadata.insert(
            format!("{prefix}.color_components"),
            crate::common::metadata::MetadataValue::String(format!(
                "{color0},{color1},{color2},{color3}"
            )),
        );
        metadata.insert(
            format!("{prefix}.opacity"),
            crate::common::metadata::MetadataValue::Int(opacity as i64),
        );
        metadata.insert(
            format!("{prefix}.kind"),
            crate::common::metadata::MetadataValue::Int(kind as i64),
        );
    }

    let trailing = payload.len() % PSD_DISPLAY_INFO_RECORD_BYTES;
    if trailing != 0 {
        metadata.insert(
            "psd.image_resource.1007.parse_status".into(),
            crate::common::metadata::MetadataValue::String(format!("trailing_{trailing}_bytes")),
        );
    } else if available_records > PSD_DISPLAY_INFO_MAX_RECORDS {
        metadata.insert(
            "psd.image_resource.1007.parse_status".into(),
            crate::common::metadata::MetadataValue::String("too_many_records".into()),
        );
    }
}

fn decode_psd_print_flags(
    payload: &[u8],
    metadata: &mut HashMap<String, crate::common::metadata::MetadataValue>,
) {
    const FLAG_NAMES: [&str; 9] = [
        "labels",
        "crop_marks",
        "color_bars",
        "registration_marks",
        "negative",
        "flip",
        "interpolate",
        "caption",
        "print_flags",
    ];

    for (index, name) in FLAG_NAMES.iter().enumerate() {
        let Some(&flag) = payload.get(index) else {
            metadata.insert(
                "psd.image_resource.1011.parse_status".into(),
                crate::common::metadata::MetadataValue::String("truncated".into()),
            );
            return;
        };
        metadata.insert(
            format!("psd.image_resource.1011.{name}"),
            crate::common::metadata::MetadataValue::Bool(flag != 0),
        );
    }

    if payload.len() > FLAG_NAMES.len() {
        metadata.insert(
            "psd.image_resource.1011.parse_status".into(),
            crate::common::metadata::MetadataValue::String(format!(
                "trailing_{}_bytes",
                payload.len() - FLAG_NAMES.len()
            )),
        );
    }
}

fn decode_psd_print_flags_information(
    payload: &[u8],
    metadata: &mut HashMap<String, crate::common::metadata::MetadataValue>,
) {
    if payload.len() < 10 {
        metadata.insert(
            "psd.image_resource.10000.parse_status".into(),
            crate::common::metadata::MetadataValue::String("truncated".into()),
        );
        return;
    }

    let version = u16::from_be_bytes([payload[0], payload[1]]);
    let center_crop_marks = payload[2] != 0;
    let reserved = payload[3];
    let bleed_width = u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]);
    let bleed_width_scale = u16::from_be_bytes([payload[8], payload[9]]);

    metadata.insert(
        "psd.image_resource.10000.version".into(),
        crate::common::metadata::MetadataValue::Int(version as i64),
    );
    metadata.insert(
        "psd.image_resource.10000.center_crop_marks".into(),
        crate::common::metadata::MetadataValue::Bool(center_crop_marks),
    );
    metadata.insert(
        "psd.image_resource.10000.reserved".into(),
        crate::common::metadata::MetadataValue::Int(reserved as i64),
    );
    metadata.insert(
        "psd.image_resource.10000.bleed_width".into(),
        crate::common::metadata::MetadataValue::Int(bleed_width as i64),
    );
    metadata.insert(
        "psd.image_resource.10000.bleed_width_scale".into(),
        crate::common::metadata::MetadataValue::Int(bleed_width_scale as i64),
    );

    if payload.len() > 10 {
        metadata.insert(
            "psd.image_resource.10000.parse_status".into(),
            crate::common::metadata::MetadataValue::String(format!(
                "trailing_{}_bytes",
                payload.len() - 10
            )),
        );
    }
}

fn decode_psd_version_info(
    payload: &[u8],
    metadata: &mut HashMap<String, crate::common::metadata::MetadataValue>,
) {
    let mut offset = 0usize;
    let Some(version) = psd_read_be_u32(payload, &mut offset) else {
        return;
    };
    let Some(&has_real_merged_data) = payload.get(offset) else {
        return;
    };
    offset += 1;
    let Some(writer_name) = psd_read_unicode_string(payload, &mut offset) else {
        return;
    };
    let Some(reader_name) = psd_read_unicode_string(payload, &mut offset) else {
        return;
    };
    let Some(file_version) = psd_read_be_u32(payload, &mut offset) else {
        return;
    };

    metadata.insert(
        "psd.image_resource.1057.version".into(),
        crate::common::metadata::MetadataValue::Int(version as i64),
    );
    metadata.insert(
        "psd.image_resource.1057.has_real_merged_data".into(),
        crate::common::metadata::MetadataValue::Bool(has_real_merged_data != 0),
    );
    if !writer_name.is_empty() {
        metadata.insert(
            "psd.image_resource.1057.writer_name".into(),
            crate::common::metadata::MetadataValue::String(writer_name),
        );
    }
    if !reader_name.is_empty() {
        metadata.insert(
            "psd.image_resource.1057.reader_name".into(),
            crate::common::metadata::MetadataValue::String(reader_name),
        );
    }
    metadata.insert(
        "psd.image_resource.1057.file_version".into(),
        crate::common::metadata::MetadataValue::Int(file_version as i64),
    );
}

fn decode_psd_pixel_aspect_ratio(
    payload: &[u8],
    metadata: &mut HashMap<String, crate::common::metadata::MetadataValue>,
) {
    if payload.len() < 12 {
        return;
    }

    let version = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
    let ratio = f64::from_bits(u64::from_be_bytes([
        payload[4],
        payload[5],
        payload[6],
        payload[7],
        payload[8],
        payload[9],
        payload[10],
        payload[11],
    ]));
    if !ratio.is_finite() {
        return;
    }

    metadata.insert(
        "psd.image_resource.1064.version".into(),
        crate::common::metadata::MetadataValue::Int(version as i64),
    );
    metadata.insert(
        "psd.image_resource.1064.aspect_ratio".into(),
        crate::common::metadata::MetadataValue::Float(ratio),
    );
}

fn decode_psd_resolution_info(
    payload: &[u8],
    metadata: &mut HashMap<String, crate::common::metadata::MetadataValue>,
) {
    if payload.len() < 16 {
        return;
    }

    let fixed_16_16 = |offset: usize| -> f64 {
        let raw = u32::from_be_bytes([
            payload[offset],
            payload[offset + 1],
            payload[offset + 2],
            payload[offset + 3],
        ]);
        raw as f64 / 65536.0
    };
    let u16_at = |offset: usize| -> i64 {
        u16::from_be_bytes([payload[offset], payload[offset + 1]]) as i64
    };

    let horizontal_resolution = fixed_16_16(0);
    let vertical_resolution = fixed_16_16(8);
    if !horizontal_resolution.is_finite() || !vertical_resolution.is_finite() {
        return;
    }

    metadata.insert(
        "psd.image_resource.1005.horizontal_resolution".into(),
        crate::common::metadata::MetadataValue::Float(horizontal_resolution),
    );
    metadata.insert(
        "psd.image_resource.1005.horizontal_resolution_unit".into(),
        crate::common::metadata::MetadataValue::Int(u16_at(4)),
    );
    metadata.insert(
        "psd.image_resource.1005.width_unit".into(),
        crate::common::metadata::MetadataValue::Int(u16_at(6)),
    );
    metadata.insert(
        "psd.image_resource.1005.vertical_resolution".into(),
        crate::common::metadata::MetadataValue::Float(vertical_resolution),
    );
    metadata.insert(
        "psd.image_resource.1005.vertical_resolution_unit".into(),
        crate::common::metadata::MetadataValue::Int(u16_at(12)),
    );
    metadata.insert(
        "psd.image_resource.1005.height_unit".into(),
        crate::common::metadata::MetadataValue::Int(u16_at(14)),
    );
}

fn decode_psd_alpha_channel_names(
    payload: &[u8],
    metadata: &mut HashMap<String, crate::common::metadata::MetadataValue>,
) {
    let mut offset = 0usize;
    let mut names = Vec::new();
    let mut status = "ok";

    while offset < payload.len() {
        if names.len() >= PSD_ALPHA_CHANNEL_NAMES_MAX {
            status = "too_many_names";
            break;
        }

        let name_len = payload[offset] as usize;
        offset += 1;
        let Some(end) = offset.checked_add(name_len) else {
            status = "truncated";
            break;
        };
        let Some(name_bytes) = payload.get(offset..end) else {
            status = "truncated";
            break;
        };
        offset = end;

        if let Some(name) = psd_clean_resource_text(name_bytes) {
            names.push(name);
        }
    }

    metadata.insert(
        "psd.image_resource.1006.alpha_channel_count".into(),
        crate::common::metadata::MetadataValue::Int(names.len() as i64),
    );
    if !names.is_empty() {
        metadata.insert(
            "psd.image_resource.1006.alpha_channel_names".into(),
            crate::common::metadata::MetadataValue::String(names.join("|")),
        );
    }
    if status != "ok" {
        metadata.insert(
            "psd.image_resource.1006.parse_status".into(),
            crate::common::metadata::MetadataValue::String(status.into()),
        );
    }
}

fn decode_psd_copyright_flag(
    payload: &[u8],
    metadata: &mut HashMap<String, crate::common::metadata::MetadataValue>,
) {
    let Some(&flag) = payload.first() else {
        return;
    };

    metadata.insert(
        "psd.image_resource.1034.copyrighted".into(),
        crate::common::metadata::MetadataValue::Bool(flag != 0),
    );
}

fn decode_psd_global_angle(
    payload: &[u8],
    metadata: &mut HashMap<String, crate::common::metadata::MetadataValue>,
) {
    if payload.len() < 4 {
        return;
    }

    let angle = i32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
    metadata.insert(
        "psd.image_resource.1037.angle".into(),
        crate::common::metadata::MetadataValue::Int(angle as i64),
    );
}

fn decode_psd_icc_profile(
    payload: &[u8],
    metadata: &mut HashMap<String, crate::common::metadata::MetadataValue>,
) {
    metadata.insert(
        "psd.image_resource.1039.profile_bytes".into(),
        crate::common::metadata::MetadataValue::Int(payload.len() as i64),
    );
    metadata.insert(
        "psd.image_resource.1039.profile_applied".into(),
        crate::common::metadata::MetadataValue::Bool(false),
    );

    if payload.len() < 128 {
        metadata.insert(
            "psd.image_resource.1039.parse_status".into(),
            crate::common::metadata::MetadataValue::String("truncated_header".into()),
        );
        return;
    }

    let declared_size = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
    let version_major = payload[8];
    let version_minor = payload[9] >> 4;
    let profile_class = String::from_utf8_lossy(&payload[12..16]).to_string();
    let color_space = String::from_utf8_lossy(&payload[16..20]).to_string();
    let pcs = String::from_utf8_lossy(&payload[20..24]).to_string();
    let signature = String::from_utf8_lossy(&payload[36..40]).to_string();

    metadata.insert(
        "psd.image_resource.1039.declared_size".into(),
        crate::common::metadata::MetadataValue::Int(declared_size as i64),
    );
    metadata.insert(
        "psd.image_resource.1039.version".into(),
        crate::common::metadata::MetadataValue::String(format!("{version_major}.{version_minor}")),
    );
    metadata.insert(
        "psd.image_resource.1039.profile_class".into(),
        crate::common::metadata::MetadataValue::String(profile_class),
    );
    metadata.insert(
        "psd.image_resource.1039.color_space".into(),
        crate::common::metadata::MetadataValue::String(color_space),
    );
    metadata.insert(
        "psd.image_resource.1039.pcs".into(),
        crate::common::metadata::MetadataValue::String(pcs),
    );
    metadata.insert(
        "psd.image_resource.1039.signature".into(),
        crate::common::metadata::MetadataValue::String(signature),
    );

    if declared_size as usize != payload.len() {
        metadata.insert(
            "psd.image_resource.1039.parse_status".into(),
            crate::common::metadata::MetadataValue::String("declared_size_mismatch".into()),
        );
    }
}

fn psd_xmp_name(name: &[u8]) -> String {
    String::from_utf8_lossy(name)
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch
            } else if ch == ':' || ch == '-' || ch == '_' {
                '.'
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_matches('.')
        .to_string()
}

fn psd_clean_xmp_text(bytes: &[u8]) -> Option<String> {
    if bytes.is_empty() || bytes.len() > PSD_XMP_MAX_BYTES {
        return None;
    }
    let text = String::from_utf8_lossy(bytes)
        .chars()
        .map(|ch| if ch == '\0' { ' ' } else { ch })
        .collect::<String>();
    let cleaned = text.trim().to_string();
    if cleaned.is_empty()
        || cleaned
            .chars()
            .any(|ch| ch.is_control() && ch != '\n' && ch != '\r' && ch != '\t')
    {
        None
    } else {
        Some(cleaned)
    }
}

fn psd_xmp_is_container(name: &str) -> bool {
    matches!(
        name.rsplit('.').next().unwrap_or(name),
        "xmpmeta" | "RDF" | "Description" | "Alt" | "Seq" | "Bag" | "li"
    )
}

fn psd_xmp_text_key(stack: &[String]) -> Option<&str> {
    stack
        .iter()
        .rev()
        .find(|name| !psd_xmp_is_container(name.as_str()))
        .map(String::as_str)
}

fn psd_xmp_insert_scalar(
    metadata: &mut HashMap<String, crate::common::metadata::MetadataValue>,
    key: &str,
    value: &str,
    inserted: &mut usize,
) {
    if *inserted >= PSD_XMP_MAX_SCALARS {
        return;
    }
    let value = value.trim_matches(char::from(0)).trim();
    if key.is_empty()
        || value.is_empty()
        || value.chars().count() > PSD_XMP_MAX_VALUE_CHARS
        || value
            .chars()
            .any(|ch| ch.is_control() && ch != '\n' && ch != '\r' && ch != '\t')
    {
        return;
    }

    metadata.insert(
        format!("psd.image_resource.1060.xmp.{key}"),
        crate::common::metadata::MetadataValue::String(value.to_string()),
    );
    *inserted += 1;
}

fn decode_psd_xmp_metadata(
    payload: &[u8],
    metadata: &mut HashMap<String, crate::common::metadata::MetadataValue>,
) {
    if payload.is_empty() || payload.len() > PSD_XMP_MAX_BYTES {
        return;
    }
    let Some(xml) = psd_clean_xmp_text(payload) else {
        return;
    };
    if !(xml.contains("<x:xmpmeta") || xml.contains("<rdf:RDF") || xml.contains("<xmpmeta")) {
        return;
    }

    let mut reader = quick_xml::Reader::from_str(&xml);
    reader.config_mut().trim_text(false);
    let mut stack = Vec::new();
    let mut text = String::new();
    let mut inserted = 0usize;
    let mut parse_error = false;

    loop {
        match reader.read_event() {
            Ok(quick_xml::events::Event::Start(element)) => {
                if stack.len() >= PSD_XMP_MAX_DEPTH {
                    parse_error = true;
                    break;
                }
                stack.push(psd_xmp_name(element.name().as_ref()));
                text.clear();
                for attr in element.attributes().flatten() {
                    if inserted >= PSD_XMP_MAX_SCALARS {
                        break;
                    }
                    let key = psd_xmp_name(attr.key.as_ref());
                    if key.is_empty()
                        || key == "rdf.about"
                        || key == "about"
                        || key.starts_with("xml.")
                        || key.starts_with("xmlns")
                    {
                        continue;
                    }
                    let Ok(value) = attr.decoded_and_normalized_value(
                        quick_xml::XmlVersion::Implicit1_0,
                        reader.decoder(),
                    ) else {
                        continue;
                    };
                    psd_xmp_insert_scalar(metadata, &key, value.as_ref(), &mut inserted);
                }
            }
            Ok(quick_xml::events::Event::Empty(element)) => {
                if stack.len() >= PSD_XMP_MAX_DEPTH {
                    parse_error = true;
                    break;
                }
                for attr in element.attributes().flatten() {
                    if inserted >= PSD_XMP_MAX_SCALARS {
                        break;
                    }
                    let key = psd_xmp_name(attr.key.as_ref());
                    if key.is_empty()
                        || key == "rdf.about"
                        || key == "about"
                        || key.starts_with("xml.")
                        || key.starts_with("xmlns")
                    {
                        continue;
                    }
                    let Ok(value) = attr.decoded_and_normalized_value(
                        quick_xml::XmlVersion::Implicit1_0,
                        reader.decoder(),
                    ) else {
                        continue;
                    };
                    psd_xmp_insert_scalar(metadata, &key, value.as_ref(), &mut inserted);
                }
            }
            Ok(quick_xml::events::Event::Text(event)) => {
                if let Some(value) = crate::common::xml::decode_xml_text(&event) {
                    if text.chars().count() < PSD_XMP_MAX_VALUE_CHARS {
                        text.push_str(&value);
                    }
                }
            }
            Ok(quick_xml::events::Event::GeneralRef(event)) => {
                if let Some(value) = crate::common::xml::decode_xml_ref(&event) {
                    if text.chars().count() < PSD_XMP_MAX_VALUE_CHARS {
                        text.push_str(&value);
                    }
                }
            }
            Ok(quick_xml::events::Event::End(_)) => {
                if let Some(key) = psd_xmp_text_key(&stack) {
                    psd_xmp_insert_scalar(metadata, key, &text, &mut inserted);
                }
                text.clear();
                stack.pop();
            }
            Ok(quick_xml::events::Event::Eof) => break,
            Err(_) => {
                parse_error = true;
                break;
            }
            _ => {}
        }
        if inserted >= PSD_XMP_MAX_SCALARS {
            break;
        }
    }

    if inserted > 0 {
        metadata.insert(
            "psd.image_resource.1060.xmp.scalar_count".into(),
            crate::common::metadata::MetadataValue::Int(inserted as i64),
        );
    }
    if parse_error {
        metadata.insert(
            "psd.image_resource.1060.xmp.parse_status".into(),
            crate::common::metadata::MetadataValue::String("parse_error".into()),
        );
    }
}

fn load_psd(path: &Path) -> Result<(ImageMetadata, Vec<u8>)> {
    let mut f = File::open(path).map_err(BioFormatsError::Io)?;
    let mut data = Vec::new();
    f.read_to_end(&mut data).map_err(BioFormatsError::Io)?;
    let mut r = Cur::new(&data);

    // Check magic
    if r.read_bytes(4) != b"8BPS" {
        return Err(BioFormatsError::Format("Not a PSD file".into()));
    }

    let version = r.read_u16();
    if !matches!(version, 1 | 2) {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "PSD unsupported version {version}"
        )));
    }
    let psb = version == 2;

    // Skip reserved 6 bytes
    r.skip(6);

    let channels = r.read_u16() as u32;
    let height = r.read_u32();
    let width = r.read_u32();
    let depth = r.read_u16();
    let color_mode = r.read_u16();
    if channels == 0 {
        return Err(BioFormatsError::UnsupportedFormat(
            "PSD channel count is non-positive".into(),
        ));
    }
    if matches!(color_mode, 3 | 4) && channels < 3 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "PSD RGB/CMYK channel count is too small ({channels})"
        )));
    }
    if width == 0 || height == 0 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "PSD dimensions are non-positive ({width}x{height})"
        )));
    }
    let pixel_type = pixel_type_from_depth(depth)?;

    let _ = psb; // Java's PSDReader uses 4-byte lengths regardless of version.

    // Color Mode Data section. For palette images (mode 2) this holds a
    // 768-byte (3 x 256) RGB lookup table, stored plane-by-plane.
    let mode_data_len = r.read_i32() as i64;
    let fp = r.fp();
    let mut lookup_table = None;
    if mode_data_len != 0 {
        if color_mode == 2 {
            let lut = r.read_bytes(768);
            if lut.len() == 768 {
                let mut red = vec![0u16; 256];
                let mut green = vec![0u16; 256];
                let mut blue = vec![0u16; 256];
                for i in 0..256 {
                    red[i] = lut[i] as u16;
                    green[i] = lut[256 + i] as u16;
                    blue[i] = lut[512 + i] as u16;
                }
                lookup_table = Some(crate::common::metadata::LookupTable { red, green, blue });
            }
        }
        r.seek((fp as i64 + mode_data_len).max(0) as usize);
    }

    // Image Resources section: capture a bounded directory of resource IDs and
    // short textual payloads while keeping Java's final offset semantics.
    let image_resources_len = r.read_u32() as usize;
    let image_resources_start = r.fp();
    let image_resources_end = image_resources_start.saturating_add(image_resources_len);
    let mut image_resource_count = 0usize;
    let mut image_resource_ids = Vec::new();
    let mut image_resource_metadata = HashMap::new();
    while r.fp().saturating_add(12) <= image_resources_end && r.read_bytes(4) == b"8BIM" {
        let tag = r.read_u16();
        image_resource_count += 1;
        image_resource_ids.push(tag.to_string());

        let name_len = r.read_u8() as usize;
        let name = r.read_bytes(name_len);
        if (1 + name_len) % 2 == 1 {
            r.skip(1);
        }
        let resource_name = String::from_utf8_lossy(name).trim().to_string();
        if !resource_name.is_empty() {
            image_resource_metadata.insert(
                format!("psd.image_resource.{tag}.name"),
                crate::common::metadata::MetadataValue::String(resource_name),
            );
        }

        let size = r.read_i32().max(0) as usize;
        let payload = r.read_bytes(size).to_vec();
        if size % 2 == 1 {
            r.skip(1);
        }
        image_resource_metadata.insert(
            format!("psd.image_resource.{tag}.type"),
            crate::common::metadata::MetadataValue::String(psd_resource_name(tag).into()),
        );
        image_resource_metadata.insert(
            format!("psd.image_resource.{tag}.bytes"),
            crate::common::metadata::MetadataValue::Int(size as i64),
        );
        if matches!(tag, 1035 | 1060 | 7000 | 7001) {
            if let Some(text) = psd_clean_resource_text(&payload) {
                image_resource_metadata.insert(
                    format!("psd.image_resource.{tag}.text"),
                    crate::common::metadata::MetadataValue::String(text),
                );
            }
        }
        if tag == 1034 {
            decode_psd_copyright_flag(&payload, &mut image_resource_metadata);
        }
        if tag == 1037 {
            decode_psd_global_angle(&payload, &mut image_resource_metadata);
        }
        if tag == 1039 {
            decode_psd_icc_profile(&payload, &mut image_resource_metadata);
        }
        if tag == 1007 {
            decode_psd_display_info(&payload, &mut image_resource_metadata);
        }
        if tag == 1011 {
            decode_psd_print_flags(&payload, &mut image_resource_metadata);
        }
        if tag == 10000 {
            decode_psd_print_flags_information(&payload, &mut image_resource_metadata);
        }
        if tag == 1064 {
            decode_psd_pixel_aspect_ratio(&payload, &mut image_resource_metadata);
        }
        if tag == 1005 {
            decode_psd_resolution_info(&payload, &mut image_resource_metadata);
        }
        if tag == 1006 {
            decode_psd_alpha_channel_names(&payload, &mut image_resource_metadata);
        }
        if tag == 1057 {
            decode_psd_version_info(&payload, &mut image_resource_metadata);
        }
        if tag == 1060 {
            decode_psd_xmp_metadata(&payload, &mut image_resource_metadata);
        }
    }
    r.seek(image_resources_end);

    // Layer and Mask Info section. Java derives the image-data offset through a
    // sequence of heuristics; we mirror them byte-for-byte so the resulting
    // (sometimes slightly misaligned) offset matches the Java reference output.
    let block_len = r.read_i32();
    // Start of the layer+mask block (just past the 4-byte length). The simple
    // fallback offset for the image-data section is `block_start + block_len`.
    let block_start = r.fp();
    let offset;
    if block_len == 0 {
        offset = r.fp();
    } else {
        let layer_len = r.read_i32();
        let layer_count = r.read_i16();
        if layer_count < 0 {
            // Vector/large-document layer data: Java rejects this, but we still
            // expose the flattened composite image. Skip the whole layer+mask
            // block and read the image-data section that follows it.
            r.seek(block_start.saturating_add(block_len.max(0) as usize));
            offset = r.fp();
            return finish_psd(
                &data,
                &mut r,
                offset,
                version,
                channels,
                height,
                width,
                depth,
                color_mode,
                pixel_type,
                lookup_table,
                image_resources_len,
                image_resource_count,
                image_resource_ids,
                image_resource_metadata,
            );
        }
        if layer_len == 0 && layer_count == 0 {
            r.skip(2);
            let check = r.read_i16();
            r.seek(r.fp().saturating_sub(if check == 0 { 4 } else { 2 }));
        }

        let lc = layer_count as usize;
        let mut lw = vec![0i32; lc];
        let mut lh = vec![0i32; lc];
        let mut lcc = vec![0i32; lc];
        for i in 0..lc {
            let top = r.read_i32();
            let left = r.read_i32();
            let bottom = r.read_i32();
            let right = r.read_i32();
            lw[i] = right - left;
            lh[i] = bottom - top;
            lcc[i] = r.read_i16() as i32;
            r.skip((lcc[i] * 6 + 12).max(0) as usize);
            let mut len = r.read_i32();
            if len % 2 == 1 {
                len += 1;
            }
            r.skip(len.max(0) as usize);
        }
        // Skip over each layer's per-channel pixel data.
        for i in 0..lc {
            if lh[i] < 0 {
                continue;
            }
            for _cc in 0..lcc[i] {
                let compressed = r.read_i16() == 1;
                if !compressed {
                    r.skip((lw[i] as i64 * lh[i] as i64).max(0) as usize);
                } else {
                    let mut lens = vec![0usize; lh[i] as usize];
                    for y in 0..lh[i] as usize {
                        lens[y] = r.read_u16() as usize;
                    }
                    for y in 0..lh[i] as usize {
                        r.skip(lens[y]);
                    }
                }
            }
        }
        let start = r.fp();
        while r.read_u8() != b'8' && r.fp() < r.len() {}
        r.skip(7);
        if r.fp() - start > 1024 {
            r.seek(start);
        }
        let mut len = r.read_i32();
        if len % 4 != 0 {
            len += 4 - (len % 4);
        }
        if (len as i64) > (r.len() as i64 - r.fp() as i64) || (len & 0xff_0000) >> 16 == 1 {
            r.seek(start);
            len = 0;
        }
        r.skip(len.max(0) as usize);

        let mut s = r.read_bytes(4).to_vec();
        while s == b"8BIM" {
            r.skip(4);
            let mut len = r.read_i32();
            if len % 4 != 0 {
                len += 4 - (len % 4);
            }
            r.skip(len.max(0) as usize);
            s = r.read_bytes(4).to_vec();
        }
        offset = r.fp().saturating_sub(4);
    }

    finish_psd(
        &data,
        &mut r,
        offset,
        version,
        channels,
        height,
        width,
        depth,
        color_mode,
        pixel_type,
        lookup_table,
        image_resources_len,
        image_resource_count,
        image_resource_ids,
        image_resource_metadata,
    )
}

/// Decode the PSD image-data section starting at `offset` and assemble metadata.
/// `offset` points at the compression word (Java's pre-read position).
#[allow(clippy::too_many_arguments)]
fn finish_psd(
    data: &[u8],
    r: &mut Cur,
    mut offset: usize,
    version: u16,
    channels: u32,
    height: u32,
    width: u32,
    depth: u16,
    color_mode: u16,
    pixel_type: PixelType,
    lookup_table: Option<crate::common::metadata::LookupTable>,
    image_resources_len: usize,
    image_resource_count: usize,
    image_resource_ids: Vec<String>,
    image_resource_metadata: HashMap<String, crate::common::metadata::MetadataValue>,
) -> Result<(ImageMetadata, Vec<u8>)> {
    // Image Data section. Java reads the compression word at `offset`, then sets
    // `offset = filePointer` (just past the word).
    r.seek(offset);
    let compression = r.read_u16();
    let bytes_per_sample = pixel_type.bytes_per_sample();
    let row_bytes = (width as usize)
        .checked_mul(bytes_per_sample)
        .ok_or_else(|| BioFormatsError::Format("PSD row byte count overflows".into()))?;
    let plane_bytes = row_bytes
        .checked_mul(height as usize)
        .ok_or_else(|| BioFormatsError::Format("PSD plane byte count overflows".into()))?;
    let total_bytes = plane_bytes
        .checked_mul(channels as usize)
        .ok_or_else(|| BioFormatsError::Format("PSD pixel byte count overflows".into()))?;

    let compressed = compression == 1;
    // Java stores per-(channel,row) RLE byte counts read immediately after the
    // compression word; `offset` then points at the first compressed byte.
    let mut row_counts: Vec<usize> = Vec::new();
    if compressed {
        for _ in 0..(channels as usize * height as usize) {
            row_counts.push(r.read_u16() as usize);
        }
    }
    offset = r.fp();

    let pixel_data: Vec<u8> = if compressed {
        // RLE: decode each row from `offset` using its byte count. Java decodes
        // exactly `lens[c][row]` bytes per row into a `sizeX*bpp` output row.
        let mut out = Vec::with_capacity(total_bytes);
        let mut pos = offset;
        for &rc in &row_counts {
            let end = (pos + rc).min(data.len());
            let src = &data[pos.min(data.len())..end];
            let decoded = decode_packbits(src, row_bytes).unwrap_or_else(|_| {
                let mut v = src.to_vec();
                v.resize(row_bytes, 0);
                v
            });
            out.extend_from_slice(&decoded);
            pos += rc;
        }
        out
    } else if compression == 0 {
        // Raw planar data starting at `offset`. Like Java's readPlane, require
        // the full plane to be present rather than zero-padding a truncated one.
        let end = offset.checked_add(total_bytes).unwrap_or(usize::MAX);
        if end > data.len() {
            return Err(BioFormatsError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "failed to fill whole buffer",
            )));
        }
        data[offset..end].to_vec()
    } else {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "PSD unsupported compression {compression}"
        )));
    };

    // Color-mode semantics per the Java PSDReader:
    //   RGB(3) / CMYK(4) -> rgb = true
    //   palette(2)       -> indexed
    // sizeC keeps the full channel count (RGB+alpha=4, CMYK=4, Lab=3, ...).
    let is_rgb = color_mode == 3 || color_mode == 4;
    let is_indexed = color_mode == 2;
    let output_channels = channels as usize;

    // Keep the composite data **planar** (channel-separated): channel 0's plane,
    // then channel 1's, etc. Java's PSDReader is interleaved=false and emits
    // channels separately, so storing planar lets the region crop mirror Java's
    // byte layout exactly. Normalize to the full expected size (pad/truncate).
    let mut pixels = pixel_data;
    pixels.resize(total_bytes, 0u8);

    // Java: imageCount = sizeC / (isRGB ? 3 : 1).
    let image_count = (output_channels as u32 / if is_rgb { 3 } else { 1 }).max(1);

    let mut series_metadata = image_resource_metadata;
    series_metadata.insert(
        "psd.version".into(),
        crate::common::metadata::MetadataValue::Int(version as i64),
    );
    series_metadata.insert(
        "psd.channels".into(),
        crate::common::metadata::MetadataValue::Int(channels as i64),
    );
    series_metadata.insert(
        "psd.depth".into(),
        crate::common::metadata::MetadataValue::Int(depth as i64),
    );
    series_metadata.insert(
        "psd.color_mode".into(),
        crate::common::metadata::MetadataValue::String(psd_color_mode_name(color_mode).into()),
    );
    series_metadata.insert(
        "psd.compression".into(),
        crate::common::metadata::MetadataValue::String(
            match compression {
                0 => "Raw",
                1 => "PackBits",
                _ => "Unsupported",
            }
            .into(),
        ),
    );
    series_metadata.insert(
        "psd.image_resources.bytes".into(),
        crate::common::metadata::MetadataValue::Int(image_resources_len as i64),
    );
    series_metadata.insert(
        "psd.image_resources.count".into(),
        crate::common::metadata::MetadataValue::Int(image_resource_count as i64),
    );
    if !image_resource_ids.is_empty() {
        series_metadata.insert(
            "psd.image_resources.ids".into(),
            crate::common::metadata::MetadataValue::String(image_resource_ids.join(",")),
        );
    }

    let meta = ImageMetadata {
        size_x: width,
        size_y: height,
        size_z: 1,
        size_c: output_channels as u32,
        size_t: 1,
        pixel_type,
        bits_per_pixel: depth as u8,
        image_count,
        dimension_order: DimensionOrder::XYCZT,
        is_rgb,
        is_interleaved: false,
        is_indexed,
        is_little_endian: false, // PSD is big-endian
        resolution_count: 1,
        thumbnail: false,
        series_metadata,
        lookup_table,
        modulo_z: None,
        modulo_c: None,
        modulo_t: None,
    };

    Ok((meta, pixels))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::metadata::MetadataValue;

    #[test]
    fn packbits_rejects_truncated_literal_payload() {
        let err = decode_packbits(&[2, 10, 11], 3)
            .expect_err("short PackBits literal should be rejected");
        assert!(
            matches!(err, BioFormatsError::InvalidData(message) if message.contains("truncated"))
        );
    }

    #[test]
    fn packbits_rejects_short_decoded_row() {
        let err = decode_packbits(&[0, 10], 2).expect_err("short PackBits row should be rejected");
        assert!(
            matches!(err, BioFormatsError::InvalidData(message) if message.contains("shorter"))
        );
    }

    #[test]
    fn alpha_channel_names_resource_decodes_pascal_strings() {
        let mut metadata = HashMap::new();
        decode_psd_alpha_channel_names(
            &[
                4, b'M', b'a', b's', b'k', 6, b'M', b'a', b't', b't', b'e', b'1',
            ],
            &mut metadata,
        );

        assert!(matches!(
            metadata.get("psd.image_resource.1006.alpha_channel_count"),
            Some(MetadataValue::Int(2))
        ));
        assert!(matches!(
            metadata.get("psd.image_resource.1006.alpha_channel_names"),
            Some(MetadataValue::String(value)) if value == "Mask|Matte1"
        ));
        assert!(!metadata.contains_key("psd.image_resource.1006.parse_status"));
    }

    #[test]
    fn alpha_channel_names_resource_records_truncated_payload() {
        let mut metadata = HashMap::new();
        decode_psd_alpha_channel_names(&[5, b'M', b'a'], &mut metadata);

        assert!(matches!(
            metadata.get("psd.image_resource.1006.alpha_channel_count"),
            Some(MetadataValue::Int(0))
        ));
        assert!(matches!(
            metadata.get("psd.image_resource.1006.parse_status"),
            Some(MetadataValue::String(value)) if value == "truncated"
        ));
    }

    #[test]
    fn display_info_resource_records_trailing_payload() {
        let mut metadata = HashMap::new();
        let mut payload = Vec::new();
        payload.extend_from_slice(&1u16.to_be_bytes());
        payload.extend_from_slice(&65535u16.to_be_bytes());
        payload.extend_from_slice(&0u16.to_be_bytes());
        payload.extend_from_slice(&0u16.to_be_bytes());
        payload.extend_from_slice(&0u16.to_be_bytes());
        payload.extend_from_slice(&75u16.to_be_bytes());
        payload.push(2);
        payload.push(0);
        payload.push(99);

        decode_psd_display_info(&payload, &mut metadata);

        assert!(matches!(
            metadata.get("psd.image_resource.1007.display_info_count"),
            Some(MetadataValue::Int(1))
        ));
        assert!(matches!(
            metadata.get("psd.image_resource.1007.display_info.0.color_space"),
            Some(MetadataValue::Int(1))
        ));
        assert!(matches!(
            metadata.get("psd.image_resource.1007.display_info.0.color_components"),
            Some(MetadataValue::String(value)) if value == "65535,0,0,0"
        ));
        assert!(matches!(
            metadata.get("psd.image_resource.1007.display_info.0.opacity"),
            Some(MetadataValue::Int(75))
        ));
        assert!(matches!(
            metadata.get("psd.image_resource.1007.display_info.0.kind"),
            Some(MetadataValue::Int(2))
        ));
        assert!(matches!(
            metadata.get("psd.image_resource.1007.parse_status"),
            Some(MetadataValue::String(value)) if value == "trailing_1_bytes"
        ));
    }

    #[test]
    fn print_flags_resource_records_truncated_payload() {
        let mut metadata = HashMap::new();
        decode_psd_print_flags(&[1, 0, 1], &mut metadata);

        assert!(matches!(
            metadata.get("psd.image_resource.1011.labels"),
            Some(MetadataValue::Bool(true))
        ));
        assert!(matches!(
            metadata.get("psd.image_resource.1011.crop_marks"),
            Some(MetadataValue::Bool(false))
        ));
        assert!(matches!(
            metadata.get("psd.image_resource.1011.color_bars"),
            Some(MetadataValue::Bool(true))
        ));
        assert!(matches!(
            metadata.get("psd.image_resource.1011.parse_status"),
            Some(MetadataValue::String(value)) if value == "truncated"
        ));
    }

    #[test]
    fn xmp_resource_extracts_bounded_semantic_scalars() {
        let mut metadata = HashMap::new();
        let payload = br#"<?xpacket begin=''?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
  <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
           xmlns:xmp="http://ns.adobe.com/xap/1.0/"
           xmlns:photoshop="http://ns.adobe.com/photoshop/1.0/"
           xmlns:dc="http://purl.org/dc/elements/1.1/">
    <rdf:Description rdf:about=""
        xmp:CreatorTool="Photoshop 2026"
        photoshop:DateCreated="2026-05-04">
      <dc:creator><rdf:Seq><rdf:li>Ada Lovelace</rdf:li></rdf:Seq></dc:creator>
      <dc:title><rdf:Alt><rdf:li xml:lang="x-default">Specimen A</rdf:li></rdf:Alt></dc:title>
    </rdf:Description>
  </rdf:RDF>
</x:xmpmeta>"#;

        decode_psd_xmp_metadata(payload, &mut metadata);

        assert!(matches!(
            metadata.get("psd.image_resource.1060.xmp.xmp.CreatorTool"),
            Some(MetadataValue::String(value)) if value == "Photoshop 2026"
        ));
        assert!(matches!(
            metadata.get("psd.image_resource.1060.xmp.photoshop.DateCreated"),
            Some(MetadataValue::String(value)) if value == "2026-05-04"
        ));
        assert!(matches!(
            metadata.get("psd.image_resource.1060.xmp.dc.creator"),
            Some(MetadataValue::String(value)) if value == "Ada Lovelace"
        ));
        assert!(matches!(
            metadata.get("psd.image_resource.1060.xmp.dc.title"),
            Some(MetadataValue::String(value)) if value == "Specimen A"
        ));
        assert!(matches!(
            metadata.get("psd.image_resource.1060.xmp.scalar_count"),
            Some(MetadataValue::Int(4))
        ));
    }

    #[test]
    fn xmp_resource_ignores_non_xmp_text_payloads() {
        let mut metadata = HashMap::new();
        decode_psd_xmp_metadata(b"CreatorTool=not xml", &mut metadata);

        assert!(!metadata
            .keys()
            .any(|key| key.starts_with("psd.image_resource.1060.xmp.")));
    }
}

impl FormatReader for PsdReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("psd") | Some("psb"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.starts_with(b"8BPS")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let (meta, pixels) = load_psd(path)?;
        self.path = Some(path.to_path_buf());
        self.meta = Some(meta);
        self.pixels = Some(pixels);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.pixels = None;
        Ok(())
    }

    fn series_count(&self) -> usize {
        usize::from(self.meta.is_some())
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.meta.is_none() {
            return Err(BioFormatsError::NotInitialized);
        }
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

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        if plane_index != 0 {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        self.pixels.clone().ok_or(BioFormatsError::NotInitialized)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        if plane_index != 0 {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let full = self
            .pixels
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?;

        validate_region("PSD", meta.size_x, meta.size_y, x, y, w, h)?;

        let bps = meta.pixel_type.bytes_per_sample();
        let channels = meta.size_c as usize;
        let row_bytes = (meta.size_x as usize)
            .checked_mul(bps)
            .ok_or_else(|| BioFormatsError::Format("PSD row size overflows".into()))?;
        let plane_bytes = row_bytes
            .checked_mul(meta.size_y as usize)
            .ok_or_else(|| BioFormatsError::Format("PSD plane size overflows".into()))?;
        let out_row = (w as usize)
            .checked_mul(bps)
            .ok_or_else(|| BioFormatsError::Format("PSD output row size overflows".into()))?;

        // Channel-separated (planar) output, matching Java's openBytes layout:
        // for each channel, copy its cropped region rows, then the next channel.
        let mut out = Vec::with_capacity(channels * (h as usize) * out_row);
        let start_x = (x as usize) * bps;
        for c in 0..channels {
            let chan_base = c * plane_bytes;
            for row in 0..h as usize {
                let src = chan_base + (y as usize + row) * row_bytes + start_x;
                out.extend_from_slice(&full[src..src + out_row]);
            }
        }
        Ok(out)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn ome_metadata(&self) -> Option<OmeMetadata> {
        let meta = self.meta.as_ref()?;
        let mut ome = OmeMetadata::from_image_metadata(meta);
        // Java sets the image name to the source file's basename.
        if let (Some(path), Some(image)) = (self.path.as_ref(), ome.images.first_mut()) {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                image.name = Some(name.to_string());
            }
        }
        let _ = ome.add_original_metadata_annotations(meta, 0);
        Some(ome)
    }
}
