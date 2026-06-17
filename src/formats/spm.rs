//! Scanning Probe Microscopy (SPM) and related format readers.
//!
//! Includes binary readers for PicoQuant TCSPC and several SPM/AFM platform
//! layouts. Formats without a decoded native layout require explicit strict raw
//! fixtures instead of heuristic dimensions.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::{crop_full_plane, validate_region};

// ===========================================================================
// Binary reader — PicoQuant TCSPC / FLIM
// ===========================================================================

/// PicoQuant PTU/PQRES time-correlated single-photon counting format.
///
/// Magic: first 6 bytes == `PQTTTR`. Image dimensions parsed from text header.
pub struct PicoQuantReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    pixels: Option<Vec<u8>>,
    reconstruction_error: Option<String>,
}

#[derive(Debug, Clone)]
struct PicoQuantTag {
    ident: String,
    index: i32,
    tag_type: u32,
    value: i64,
    payload: Option<Vec<u8>>,
}

struct PicoQuantReconstruction {
    pixels: Vec<u8>,
    detector_channels: u32,
    lifetime_bins: u32,
    bidirectional: bool,
    pixel_type: PixelType,
    bits_per_pixel: u8,
    histogram_layout: Option<&'static str>,
    description: String,
}

#[derive(Debug, Clone, Copy)]
struct PicoQuantRecordKind {
    label: &'static str,
    family: &'static str,
    acquisition_mode: &'static str,
    record_layout: &'static str,
    hydraharp_layout: bool,
    marker_raster_layout: bool,
}

const PTU_HEADER_LEN: usize = 16;
const PTU_TAG_LEN: usize = 48;
const PTU_TAG_INT8: u32 = 0x1000_0008;
const PTU_TAG_BOOL8: u32 = 0x0000_0008;
const PTU_TAG_FLOAT8: u32 = 0x2000_0008;
const PTU_TAG_EMPTY8: u32 = 0xffff_0008;
const PTU_TAG_ANSI_STRING: u32 = 0x4001_ffff;
const PTU_TAG_WIDE_STRING: u32 = 0x4002_ffff;
const PTU_TAG_BINARY_BLOB: u32 = 0xffff_ffff;
const PTU_RECORD_PICOHARP_T2: i64 = 0x0001_0203;
const PTU_RECORD_PICOHARP_T3: i64 = 0x0001_0303;
const PTU_RECORD_HYDRAHARP_T2: i64 = 0x0001_0204;
const PTU_RECORD_HYDRAHARP_T3: i64 = 0x0001_0304;
const PTU_RECORD_TIMEHARP260N_T2: i64 = 0x0001_0205;
const PTU_RECORD_TIMEHARP260N_T3: i64 = 0x0001_0305;
const PTU_RECORD_TIMEHARP260P_T2: i64 = 0x0001_0206;
const PTU_RECORD_TIMEHARP260P_T3: i64 = 0x0001_0306;
const PTU_RECORD_MULTIHARP_T2: i64 = 0x0001_0207;
const PTU_RECORD_MULTIHARP_T3: i64 = 0x0001_0307;
const PTU_RECORD_HYDRAHARP2_T2: i64 = 0x0101_0204;
const PTU_RECORD_HYDRAHARP2_T3: i64 = 0x0101_0304;
const PTU_T2_SYNC_PERIOD: u64 = 1 << 25;
const PTU_T3_SYNC_PERIOD: u64 = 1024;

fn picoquant_event_stream_unsupported(reason: &str) -> BioFormatsError {
    BioFormatsError::UnsupportedFormat(format!(
        "PicoQuant TTTR image reconstruction unavailable: {reason}"
    ))
}

impl PicoQuantReader {
    pub fn new() -> Self {
        PicoQuantReader {
            path: None,
            meta: None,
            pixels: None,
            reconstruction_error: None,
        }
    }

    fn parse_unified_tags(data: &[u8]) -> Result<(Vec<PicoQuantTag>, usize)> {
        if data.len() < PTU_HEADER_LEN || &data[0..6] != b"PQTTTR" {
            return Err(BioFormatsError::UnsupportedFormat(
                "PicoQuant PTU missing PQTTTR magic".into(),
            ));
        }

        let mut tags = Vec::new();
        let mut offset = PTU_HEADER_LEN;
        loop {
            let record_end = offset.checked_add(PTU_TAG_LEN).ok_or_else(|| {
                BioFormatsError::UnsupportedFormat("PicoQuant PTU tag offset overflows".into())
            })?;
            if record_end > data.len() {
                return Err(BioFormatsError::UnsupportedFormat(
                    "PicoQuant PTU tag table is truncated".into(),
                ));
            }

            let ident_bytes = &data[offset..offset + 32];
            let ident_len = ident_bytes
                .iter()
                .position(|b| *b == 0)
                .unwrap_or(ident_bytes.len());
            let ident = String::from_utf8_lossy(&ident_bytes[..ident_len]).into_owned();
            let index = i32::from_le_bytes(data[offset + 32..offset + 36].try_into().unwrap());
            let tag_type = u32::from_le_bytes(data[offset + 36..offset + 40].try_into().unwrap());
            let value = i64::from_le_bytes(data[offset + 40..offset + 48].try_into().unwrap());
            offset = record_end;

            let payload = if matches!(
                tag_type,
                PTU_TAG_ANSI_STRING | PTU_TAG_WIDE_STRING | PTU_TAG_BINARY_BLOB
            ) {
                let len = usize::try_from(value).map_err(|_| {
                    BioFormatsError::UnsupportedFormat(format!(
                        "PicoQuant PTU tag {ident} has negative payload length"
                    ))
                })?;
                let payload_end = offset.checked_add(len).ok_or_else(|| {
                    BioFormatsError::UnsupportedFormat(format!(
                        "PicoQuant PTU tag {ident} payload offset overflows"
                    ))
                })?;
                if payload_end > data.len() {
                    return Err(BioFormatsError::UnsupportedFormat(format!(
                        "PicoQuant PTU tag {ident} payload is truncated"
                    )));
                }
                let payload = data[offset..payload_end].to_vec();
                offset = payload_end;
                Some(payload)
            } else {
                None
            };

            let is_end = ident == "Header_End";
            tags.push(PicoQuantTag {
                ident,
                index,
                tag_type,
                value,
                payload,
            });
            if is_end {
                return Ok((tags, offset));
            }
        }
    }

    fn int_tag(tags: &[PicoQuantTag], names: &[&str]) -> Option<i64> {
        tags.iter()
            .find(|tag| {
                tag.tag_type == PTU_TAG_INT8 && tag.index < 0 && names.contains(&tag.ident.as_str())
            })
            .map(|tag| tag.value)
    }

    fn float_tag(tags: &[PicoQuantTag], names: &[&str]) -> Option<f64> {
        tags.iter()
            .find(|tag| {
                tag.tag_type == PTU_TAG_FLOAT8
                    && tag.index < 0
                    && names.contains(&tag.ident.as_str())
            })
            .map(|tag| f64::from_bits(tag.value as u64))
    }

    fn tttr_record_kind(record_type: i64) -> Option<PicoQuantRecordKind> {
        let (
            label,
            family,
            acquisition_mode,
            record_layout,
            hydraharp_layout,
            marker_raster_layout,
        ) = match record_type {
            PTU_RECORD_PICOHARP_T2 => (
                "PicoHarp T2",
                "PicoHarp",
                "tttr_t2",
                "picoharp",
                false,
                false,
            ),
            PTU_RECORD_PICOHARP_T3 => (
                "PicoHarp T3",
                "PicoHarp",
                "tttr_t3",
                "picoharp",
                false,
                false,
            ),
            PTU_RECORD_HYDRAHARP_T2 => (
                "HydraHarp T2",
                "HydraHarp",
                "tttr_t2",
                "hydraharp",
                true,
                true,
            ),
            PTU_RECORD_HYDRAHARP_T3 => (
                "HydraHarp T3",
                "HydraHarp",
                "tttr_t3",
                "hydraharp",
                true,
                true,
            ),
            PTU_RECORD_TIMEHARP260N_T2 => (
                "TimeHarp 260N T2",
                "TimeHarp 260N",
                "tttr_t2",
                "hydraharp-compatible",
                false,
                true,
            ),
            PTU_RECORD_TIMEHARP260N_T3 => (
                "TimeHarp 260N T3",
                "TimeHarp 260N",
                "tttr_t3",
                "hydraharp-compatible",
                false,
                true,
            ),
            PTU_RECORD_TIMEHARP260P_T2 => (
                "TimeHarp 260P T2",
                "TimeHarp 260P",
                "tttr_t2",
                "hydraharp-compatible",
                false,
                true,
            ),
            PTU_RECORD_TIMEHARP260P_T3 => (
                "TimeHarp 260P T3",
                "TimeHarp 260P",
                "tttr_t3",
                "hydraharp-compatible",
                false,
                true,
            ),
            PTU_RECORD_MULTIHARP_T2 => (
                "MultiHarp T2",
                "MultiHarp",
                "tttr_t2",
                "hydraharp-compatible",
                false,
                true,
            ),
            PTU_RECORD_MULTIHARP_T3 => (
                "MultiHarp T3",
                "MultiHarp",
                "tttr_t3",
                "hydraharp-compatible",
                false,
                true,
            ),
            PTU_RECORD_HYDRAHARP2_T2 => (
                "HydraHarp 2 T2",
                "HydraHarp 2",
                "tttr_t2",
                "hydraharp",
                true,
                true,
            ),
            PTU_RECORD_HYDRAHARP2_T3 => (
                "HydraHarp 2 T3",
                "HydraHarp 2",
                "tttr_t3",
                "hydraharp",
                true,
                true,
            ),
            _ => return None,
        };
        Some(PicoQuantRecordKind {
            label,
            family,
            acquisition_mode,
            record_layout,
            hydraharp_layout,
            marker_raster_layout,
        })
    }

    fn annotate_tttr_acquisition_mode(
        series_metadata: &mut HashMap<String, MetadataValue>,
        tttr_record_type: Option<i64>,
    ) -> Option<PicoQuantRecordKind> {
        fn insert_layout_provenance(
            series_metadata: &mut HashMap<String, MetadataValue>,
            source: &str,
            provenance: &str,
        ) {
            series_metadata.insert(
                "ptu.tttr_record_layout_source".into(),
                MetadataValue::String(source.into()),
            );
            series_metadata.insert(
                "ptu.tttr_record_layout_provenance".into(),
                MetadataValue::String(provenance.into()),
            );
        }

        let Some(record_type) = tttr_record_type else {
            series_metadata.insert(
                "ptu.acquisition_mode".into(),
                MetadataValue::String("unspecified".into()),
            );
            series_metadata.insert(
                "ptu.acquisition_mode_ambiguous".into(),
                MetadataValue::Bool(true),
            );
            series_metadata.insert(
                "ptu.acquisition_mode_source".into(),
                MetadataValue::String("missing TTResultFormat_TTTRRecType".into()),
            );
            series_metadata.insert(
                "ptu.tttr_record_layout".into(),
                MetadataValue::String("unspecified".into()),
            );
            insert_layout_provenance(
                series_metadata,
                "missing TTResultFormat_TTTRRecType",
                "no TTTR record type tag is present, so record bit layout is not decoded",
            );
            return None;
        };

        series_metadata.insert(
            "ptu.tttr_record_type_code_hex".into(),
            MetadataValue::String(format!("0x{record_type:08x}")),
        );

        let record_kind = Self::tttr_record_kind(record_type);
        if let Some(record_kind) = record_kind {
            series_metadata.insert(
                "ptu.acquisition_mode".into(),
                MetadataValue::String(record_kind.acquisition_mode.into()),
            );
            series_metadata.insert(
                "ptu.acquisition_mode_ambiguous".into(),
                MetadataValue::Bool(false),
            );
            series_metadata.insert(
                "ptu.acquisition_mode_source".into(),
                MetadataValue::String("TTResultFormat_TTTRRecType".into()),
            );
            series_metadata.insert(
                "ptu.tttr_record_type".into(),
                MetadataValue::String(record_kind.label.into()),
            );
            series_metadata.insert(
                "ptu.tttr_record_family".into(),
                MetadataValue::String(record_kind.family.into()),
            );
            series_metadata.insert(
                "ptu.tttr_record_layout".into(),
                MetadataValue::String(record_kind.record_layout.into()),
            );
            let layout_provenance = if record_kind.family == "PicoHarp" {
                "recognized from TTResultFormat_TTTRRecType metadata; PicoHarp marker-raster pixel reconstruction is disabled pending original-code evidence"
            } else if record_kind.marker_raster_layout {
                "recognized from TTResultFormat_TTTRRecType metadata; marker-raster reconstruction uses the supported local HydraHarp-compatible record path"
            } else {
                "recognized from TTResultFormat_TTTRRecType metadata; record layout is metadata-only until a reconstruction branch is translated"
            };
            insert_layout_provenance(
                series_metadata,
                "TTResultFormat_TTTRRecType",
                layout_provenance,
            );
            series_metadata.insert(
                "ptu.tttr_hydraharp_layout".into(),
                MetadataValue::Bool(record_kind.hydraharp_layout),
            );
            series_metadata.insert(
                "ptu.tttr_marker_raster_layout".into(),
                MetadataValue::Bool(record_kind.marker_raster_layout),
            );
        } else if let Some(mode) = Self::infer_tttr_acquisition_mode(record_type) {
            series_metadata.insert(
                "ptu.acquisition_mode".into(),
                MetadataValue::String(mode.into()),
            );
            series_metadata.insert(
                "ptu.acquisition_mode_ambiguous".into(),
                MetadataValue::Bool(true),
            );
            series_metadata.insert(
                "ptu.acquisition_mode_source".into(),
                MetadataValue::String(
                    "inferred from unrecognized TTResultFormat_TTTRRecType mode byte".into(),
                ),
            );
            series_metadata.insert(
                "ptu.tttr_record_type".into(),
                MetadataValue::String(format!(
                    "Unknown {} TTTR record type",
                    if mode == "tttr_t2" { "T2" } else { "T3" }
                )),
            );
            series_metadata.insert(
                "ptu.tttr_record_family".into(),
                MetadataValue::String("Unknown".into()),
            );
            series_metadata.insert(
                "ptu.tttr_record_layout".into(),
                MetadataValue::String("unknown".into()),
            );
            insert_layout_provenance(
                series_metadata,
                "unrecognized TTResultFormat_TTTRRecType",
                "acquisition mode is inferred from the mode byte only; record bit layout is not decoded",
            );
            series_metadata.insert(
                "ptu.tttr_hydraharp_layout".into(),
                MetadataValue::Bool(false),
            );
        } else {
            series_metadata.insert(
                "ptu.acquisition_mode".into(),
                MetadataValue::String("tttr_unknown_record".into()),
            );
            series_metadata.insert(
                "ptu.acquisition_mode_ambiguous".into(),
                MetadataValue::Bool(true),
            );
            series_metadata.insert(
                "ptu.acquisition_mode_source".into(),
                MetadataValue::String("unrecognized TTResultFormat_TTTRRecType".into()),
            );
            series_metadata.insert(
                "ptu.tttr_record_type".into(),
                MetadataValue::String("Unknown TTTR record type".into()),
            );
            series_metadata.insert(
                "ptu.tttr_record_family".into(),
                MetadataValue::String("Unknown".into()),
            );
            series_metadata.insert(
                "ptu.tttr_record_layout".into(),
                MetadataValue::String("unknown".into()),
            );
            insert_layout_provenance(
                series_metadata,
                "unrecognized TTResultFormat_TTTRRecType",
                "record type is unrecognized and no T2/T3 mode byte could be inferred; record bit layout is not decoded",
            );
            series_metadata.insert(
                "ptu.tttr_hydraharp_layout".into(),
                MetadataValue::Bool(false),
            );
        }

        record_kind
    }

    fn infer_tttr_acquisition_mode(record_type: i64) -> Option<&'static str> {
        match ((record_type as u64) >> 8) & 0xff {
            0x02 => Some("tttr_t2"),
            0x03 => Some("tttr_t3"),
            _ => None,
        }
    }

    fn is_histogram_acquisition(tags: &[PicoQuantTag]) -> bool {
        tags.iter().any(|tag| Self::is_histogram_tag(&tag.ident))
    }

    fn is_histogram_tag(ident: &str) -> bool {
        ident.starts_with("HistResDscr_")
            || ident.starts_with("HistoResult_")
            || ident.starts_with("HistoResultFormat_")
            || ident.to_ascii_lowercase().contains("histogram")
    }

    fn positive_int_tag_any_index(tags: &[PicoQuantTag], names: &[&str]) -> Option<u32> {
        tags.iter()
            .find(|tag| tag.tag_type == PTU_TAG_INT8 && names.contains(&tag.ident.as_str()))
            .and_then(|tag| {
                if tag.value > 0 {
                    u32::try_from(tag.value).ok()
                } else {
                    None
                }
            })
    }

    fn histogram_payload_header_bytes(
        tags: &[PicoQuantTag],
        payload_bytes: usize,
    ) -> Result<usize> {
        let Some(offset) = tags
            .iter()
            .find(|tag| {
                tag.tag_type == PTU_TAG_INT8
                    && tag.index < 0
                    && Self::is_histogram_payload_offset_tag(&tag.ident)
            })
            .map(|tag| tag.value)
        else {
            return Ok(0);
        };
        if offset < 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "PicoQuant histogram payload offset must be non-negative".into(),
            ));
        }
        let offset = usize::try_from(offset).map_err(|_| {
            BioFormatsError::UnsupportedFormat(
                "PicoQuant histogram payload offset is too large".into(),
            )
        })?;
        if offset > payload_bytes {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "PicoQuant histogram payload offset {offset} exceeds payload size {payload_bytes}"
            )));
        }
        Ok(offset)
    }

    fn histogram_sample_bytes_hint(tags: &[PicoQuantTag]) -> Option<usize> {
        for tag in tags.iter().filter(|tag| tag.tag_type == PTU_TAG_INT8) {
            match tag.ident.as_str() {
                "HistResDscr_BitsPerBin"
                | "HistResDscr_BitsPerPoint"
                | "HistoResult_BitsPerBin"
                | "HistoResult_BitsPerPoint" => match tag.value {
                    8 => return Some(1),
                    16 => return Some(2),
                    32 => return Some(4),
                    _ => {}
                },
                "HistResDscr_BytesPerBin"
                | "HistResDscr_BytesPerPoint"
                | "HistoResult_BytesPerBin"
                | "HistoResult_BytesPerPoint" => match tag.value {
                    1 => return Some(1),
                    2 => return Some(2),
                    4 => return Some(4),
                    _ => {}
                },
                _ => {}
            }
        }
        None
    }

    fn is_histogram_payload_offset_tag(ident: &str) -> bool {
        matches!(
            ident,
            "HistResDscr_DataOffset"
                | "HistResDscr_PayloadOffset"
                | "HistoResult_DataOffset"
                | "HistoResult_PayloadOffset"
                | "HistoResultFormat_DataOffset"
                | "HistoResultFormat_PayloadOffset"
        )
    }

    fn histogram_indexed_payload_offsets(tags: &[PicoQuantTag]) -> Result<Vec<(u32, usize)>> {
        let mut offsets = Vec::new();
        for tag in tags.iter().filter(|tag| {
            tag.tag_type == PTU_TAG_INT8
                && tag.index >= 0
                && Self::is_histogram_payload_offset_tag(&tag.ident)
        }) {
            if tag.value < 0 {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "PicoQuant histogram indexed payload offset {} must be non-negative",
                    Self::ptu_tag_name(tag)
                )));
            }
            offsets.push((
                u32::try_from(tag.index).map_err(|_| {
                    BioFormatsError::UnsupportedFormat(
                        "PicoQuant histogram indexed payload offset index is too large".into(),
                    )
                })?,
                usize::try_from(tag.value).map_err(|_| {
                    BioFormatsError::UnsupportedFormat(
                        "PicoQuant histogram indexed payload offset is too large".into(),
                    )
                })?,
            ));
        }
        offsets.sort_unstable_by_key(|(index, _)| *index);
        offsets.dedup_by_key(|(index, _)| *index);
        Ok(offsets)
    }

    fn ptu_tag_name(tag: &PicoQuantTag) -> String {
        if tag.index >= 0 {
            format!("{}[{}]", tag.ident, tag.index)
        } else {
            tag.ident.clone()
        }
    }

    fn ptu_tag_value_text(tag: &PicoQuantTag) -> String {
        match tag.tag_type {
            PTU_TAG_BOOL8 => (tag.value != 0).to_string(),
            PTU_TAG_FLOAT8 => f64::from_bits(tag.value as u64).to_string(),
            PTU_TAG_ANSI_STRING | PTU_TAG_WIDE_STRING => {
                Self::ptu_string_value(tag).unwrap_or_default()
            }
            PTU_TAG_BINARY_BLOB => format!("{} bytes", tag.payload.as_ref().map_or(0, Vec::len)),
            _ => tag.value.to_string(),
        }
    }

    fn histogram_compression_tag_is_enabled(tag: &PicoQuantTag) -> bool {
        match tag.tag_type {
            PTU_TAG_BOOL8 => tag.value != 0,
            PTU_TAG_INT8 => tag.value != 0,
            PTU_TAG_ANSI_STRING | PTU_TAG_WIDE_STRING => {
                let value = Self::ptu_string_value(tag)
                    .unwrap_or_default()
                    .trim()
                    .to_ascii_lowercase();
                !matches!(
                    value.as_str(),
                    "" | "0" | "false" | "no" | "none" | "plain" | "raw" | "uncompressed"
                )
            }
            _ => false,
        }
    }

    fn histogram_compression_hints(tags: &[PicoQuantTag]) -> Vec<String> {
        tags.iter()
            .filter(|tag| tag.ident.to_ascii_lowercase().contains("compress"))
            .filter(|tag| Self::histogram_compression_tag_is_enabled(tag))
            .map(|tag| {
                format!(
                    "{}={}",
                    Self::ptu_tag_name(tag),
                    Self::ptu_tag_value_text(tag)
                )
            })
            .collect()
    }

    fn histogram_compression_codec(tags: &[PicoQuantTag]) -> Option<&'static str> {
        let mut codec = None;
        for tag in tags
            .iter()
            .filter(|tag| tag.ident.to_ascii_lowercase().contains("compress"))
            .filter(|tag| Self::histogram_compression_tag_is_enabled(tag))
        {
            let value = Self::ptu_tag_value_text(tag)
                .trim()
                .to_ascii_lowercase()
                .replace(['-', '_', ' '], "");
            let candidate = match value.as_str() {
                "zlib" | "zip" | "deflatezlib" => Some("zlib"),
                "gzip" | "gz" => Some("gzip"),
                "deflate" | "rawdeflate" | "inflate" => Some("deflate"),
                _ => None,
            };
            let candidate = candidate?;
            if codec.is_some_and(|existing| existing != candidate) {
                return None;
            }
            codec = Some(candidate);
        }
        codec
    }

    fn decompress_histogram_payload(payload: &[u8], codec: &str) -> Result<Vec<u8>> {
        use std::io::Read;

        let mut decoded = Vec::new();
        let result = match codec {
            "zlib" => flate2::read::ZlibDecoder::new(payload).read_to_end(&mut decoded),
            "gzip" => flate2::read::GzDecoder::new(payload).read_to_end(&mut decoded),
            "deflate" => flate2::read::DeflateDecoder::new(payload).read_to_end(&mut decoded),
            _ => {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "PicoQuant histogram compression {codec} is unsupported"
                )))
            }
        };
        result.map_err(BioFormatsError::Io)?;
        Ok(decoded)
    }

    fn histogram_payload_signature(payload: &[u8]) -> &'static str {
        if payload.is_empty() {
            "empty payload"
        } else if payload.starts_with(&[0x1f, 0x8b]) {
            "gzip stream"
        } else if payload.len() >= 2
            && payload[0] & 0x0f == 8
            && payload[0] >> 4 <= 7
            && (((payload[0] as u16) << 8) | payload[1] as u16) % 31 == 0
        {
            "zlib stream"
        } else if payload.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
            "Zstandard frame"
        } else if payload.starts_with(&[0xfd, b'7', b'z', b'X', b'Z', 0x00]) {
            "XZ stream"
        } else if payload.starts_with(b"BZh") {
            "bzip2 stream"
        } else if payload.starts_with(&[0x04, 0x22, 0x4d, 0x18]) {
            "LZ4 frame"
        } else {
            "unknown payload"
        }
    }

    fn histogram_payload_prefix(payload: &[u8]) -> String {
        payload
            .iter()
            .take(8)
            .map(|byte| format!("{byte:02x}"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn histogram_bins(tags: &[PicoQuantTag]) -> Option<u32> {
        Self::positive_int_tag_any_index(
            tags,
            &[
                "HistResDscr_HistogramBins",
                "HistResDscr_Bins",
                "HistResDscr_DataBins",
                "HistoResult_NumberOfBins",
                "HistoResult_HistogramBins",
                "HistoResult_Bins",
                "HistoResult_DataBins",
            ],
        )
    }

    fn is_histogram_bin_count_tag(ident: &str) -> bool {
        matches!(
            ident,
            "HistResDscr_HistogramBins"
                | "HistResDscr_Bins"
                | "HistResDscr_DataBins"
                | "HistoResult_NumberOfBins"
                | "HistoResult_HistogramBins"
                | "HistoResult_Bins"
                | "HistoResult_DataBins"
        )
    }

    fn histogram_indexed_bin_counts(tags: &[PicoQuantTag]) -> Vec<(u32, u32)> {
        let mut counts: Vec<(u32, u32)> = tags
            .iter()
            .filter(|tag| {
                tag.tag_type == PTU_TAG_INT8
                    && tag.index >= 0
                    && tag.value > 0
                    && Self::is_histogram_bin_count_tag(&tag.ident)
            })
            .filter_map(|tag| {
                Some((
                    u32::try_from(tag.index).ok()?,
                    u32::try_from(tag.value).ok()?,
                ))
            })
            .collect();
        counts.sort_unstable_by_key(|(index, _)| *index);
        counts.dedup_by_key(|(index, _)| *index);
        counts
    }

    fn histogram_indexed_bin_counts_are_consistent(counts: &[(u32, u32)]) -> bool {
        let Some((_, first_count)) = counts.first() else {
            return true;
        };
        counts.iter().all(|(_, bin_count)| bin_count == first_count)
    }

    fn histogram_consistent_indexed_bin_count(counts: &[(u32, u32)]) -> Option<u32> {
        if Self::histogram_indexed_bin_counts_are_consistent(counts) {
            counts.first().map(|(_, bin_count)| *bin_count)
        } else {
            None
        }
    }

    fn histogram_curve_count(tags: &[PicoQuantTag]) -> u32 {
        if let Some(curve_count) = Self::positive_int_tag_any_index(
            tags,
            &[
                "HistResDscr_NumberOfCurves",
                "HistResDscr_CurveCount",
                "HistoResult_NumberOfCurves",
                "HistoResult_CurveCount",
            ],
        ) {
            return curve_count;
        }
        tags.iter()
            .filter(|tag| Self::is_histogram_tag(&tag.ident) && tag.index >= 0)
            .filter_map(|tag| u32::try_from(tag.index).ok())
            .max()
            .map_or(1, |max_index| max_index.saturating_add(1))
    }

    fn histogram_indexed_descriptors(tags: &[PicoQuantTag]) -> Vec<u32> {
        let mut indices: Vec<u32> = tags
            .iter()
            .filter(|tag| Self::is_histogram_tag(&tag.ident) && tag.index >= 0)
            .filter_map(|tag| u32::try_from(tag.index).ok())
            .collect();
        indices.sort_unstable();
        indices.dedup();
        indices
    }

    fn histogram_indices_are_contiguous(indices: &[u32]) -> bool {
        indices
            .iter()
            .copied()
            .enumerate()
            .all(|(expected, index)| {
                u32::try_from(expected).is_ok_and(|expected| index == expected)
            })
    }

    fn histogram_supported_expected_bytes(histogram_bins: u32, curve_count: u32) -> Result<String> {
        let samples = (histogram_bins as usize)
            .checked_mul(curve_count as usize)
            .ok_or_else(|| {
                BioFormatsError::UnsupportedFormat(
                    "PicoQuant histogram sample count overflows".into(),
                )
            })?;
        let mut expected = Vec::with_capacity(3);
        for sample_bytes in [1usize, 2, 4] {
            let bytes = samples.checked_mul(sample_bytes).ok_or_else(|| {
                BioFormatsError::UnsupportedFormat(
                    "PicoQuant histogram payload size overflows".into(),
                )
            })?;
            expected.push(bytes.to_string());
        }
        Ok(expected.join(", "))
    }

    fn decode_histogram_payload(
        data: &[u8],
        data_offset: usize,
        tags: &[PicoQuantTag],
    ) -> Result<Option<PicoQuantReconstruction>> {
        let Some(payload) = data.get(data_offset..) else {
            return Err(BioFormatsError::UnsupportedFormat(
                "PicoQuant histogram payload offset is outside file".into(),
            ));
        };
        Self::decode_histogram_payload_bytes(payload, tags)
    }

    fn decode_histogram_payload_bytes(
        payload: &[u8],
        tags: &[PicoQuantTag],
    ) -> Result<Option<PicoQuantReconstruction>> {
        let histogram_indices = Self::histogram_indexed_descriptors(tags);
        if !Self::histogram_indices_are_contiguous(&histogram_indices) {
            return Ok(None);
        }
        let indexed_bin_counts = Self::histogram_indexed_bin_counts(tags);
        if !Self::histogram_indexed_bin_counts_are_consistent(&indexed_bin_counts) {
            return Ok(None);
        }
        let Some(histogram_bins) =
            Self::histogram_consistent_indexed_bin_count(&Self::histogram_indexed_bin_counts(tags))
                .or_else(|| Self::histogram_bins(tags))
        else {
            return Ok(None);
        };
        let curve_count = Self::histogram_curve_count(tags);
        let expected_samples = (histogram_bins as usize)
            .checked_mul(curve_count as usize)
            .ok_or_else(|| {
                BioFormatsError::UnsupportedFormat(
                    "PicoQuant histogram sample count overflows".into(),
                )
            })?;

        let sample_bytes_hint = Self::histogram_sample_bytes_hint(tags);
        let mut matching_layouts = [
            (1usize, PixelType::Uint8, 8u8, "uint8 bins"),
            (2usize, PixelType::Uint16, 16u8, "little-endian uint16 bins"),
            (4usize, PixelType::Uint32, 32u8, "little-endian uint32 bins"),
        ]
        .into_iter()
        .filter(|(sample_bytes, _, _, _)| {
            if sample_bytes_hint.is_some_and(|hint| hint != *sample_bytes) {
                return false;
            }
            expected_samples
                .checked_mul(*sample_bytes)
                .is_some_and(|expected_bytes| expected_bytes == payload.len())
        });
        let Some((sample_bytes, pixel_type, bits_per_pixel, layout)) = matching_layouts.next()
        else {
            return Ok(None);
        };
        if matching_layouts.next().is_some() {
            return Ok(None);
        }

        let expected_bytes = expected_samples.checked_mul(sample_bytes).ok_or_else(|| {
            BioFormatsError::UnsupportedFormat("PicoQuant histogram payload size overflows".into())
        })?;
        let mut pixels = Vec::with_capacity(expected_bytes);
        for sample in payload.chunks_exact(sample_bytes) {
            pixels.extend_from_slice(sample);
        }
        let description = if curve_count == 1 {
            format!("PicoQuant histogram payload decoded as {histogram_bins} {layout}")
        } else {
            format!(
                "PicoQuant histogram payload decoded as {curve_count} curves of {histogram_bins} {layout}"
            )
        };
        Ok(Some(PicoQuantReconstruction {
            pixels,
            detector_channels: curve_count,
            lifetime_bins: 1,
            bidirectional: false,
            pixel_type,
            bits_per_pixel,
            histogram_layout: Some(layout),
            description,
        }))
    }

    fn decode_indexed_offset_histogram_payload(
        data: &[u8],
        data_offset: usize,
        tags: &[PicoQuantTag],
    ) -> Result<Option<PicoQuantReconstruction>> {
        let offsets = Self::histogram_indexed_payload_offsets(tags)?;
        if offsets.is_empty() {
            return Ok(None);
        }
        let histogram_indices = Self::histogram_indexed_descriptors(tags);
        if !Self::histogram_indices_are_contiguous(&histogram_indices) {
            return Ok(None);
        }
        let indexed_bin_counts = Self::histogram_indexed_bin_counts(tags);
        if !Self::histogram_indexed_bin_counts_are_consistent(&indexed_bin_counts) {
            return Ok(None);
        }
        let Some(histogram_bins) =
            Self::histogram_consistent_indexed_bin_count(&indexed_bin_counts)
                .or_else(|| Self::histogram_bins(tags))
        else {
            return Ok(None);
        };
        let curve_count = Self::histogram_curve_count(tags);
        if offsets.len() != curve_count as usize
            || offsets
                .iter()
                .enumerate()
                .any(|(expected, (index, _))| u32::try_from(expected) != Ok(*index))
        {
            return Ok(None);
        }
        let Some(payload) = data.get(data_offset..) else {
            return Err(BioFormatsError::UnsupportedFormat(
                "PicoQuant histogram payload offset is outside file".into(),
            ));
        };
        if offsets.iter().any(|(_, offset)| *offset > payload.len()) {
            return Err(BioFormatsError::UnsupportedFormat(
                "PicoQuant histogram indexed payload offset is outside file".into(),
            ));
        }
        let samples_per_curve = histogram_bins as usize;
        let mut selected = None;
        let sample_bytes_hint = Self::histogram_sample_bytes_hint(tags);
        for (sample_bytes, pixel_type, bits_per_pixel, layout) in [
            (1usize, PixelType::Uint8, 8u8, "indexed-offset uint8 bins"),
            (
                2usize,
                PixelType::Uint16,
                16u8,
                "indexed-offset little-endian uint16 bins",
            ),
            (
                4usize,
                PixelType::Uint32,
                32u8,
                "indexed-offset little-endian uint32 bins",
            ),
        ] {
            if sample_bytes_hint.is_some_and(|hint| hint != sample_bytes) {
                continue;
            }
            let bytes_per_curve = samples_per_curve.checked_mul(sample_bytes).ok_or_else(|| {
                BioFormatsError::UnsupportedFormat(
                    "PicoQuant histogram payload size overflows".into(),
                )
            })?;
            let fits = offsets.iter().enumerate().all(|(i, (_, start))| {
                let end = offsets
                    .get(i + 1)
                    .map(|(_, offset)| *offset)
                    .unwrap_or(payload.len());
                *start <= end && end.saturating_sub(*start) >= bytes_per_curve
            });
            if fits {
                selected = Some((sample_bytes, pixel_type, bits_per_pixel, layout));
            }
        }
        let Some((sample_bytes, pixel_type, bits_per_pixel, layout)) = selected else {
            return Ok(None);
        };
        let bytes_per_curve = samples_per_curve.checked_mul(sample_bytes).ok_or_else(|| {
            BioFormatsError::UnsupportedFormat("PicoQuant histogram payload size overflows".into())
        })?;
        let expected_bytes = bytes_per_curve
            .checked_mul(curve_count as usize)
            .ok_or_else(|| {
                BioFormatsError::UnsupportedFormat(
                    "PicoQuant histogram payload size overflows".into(),
                )
            })?;
        let mut pixels = Vec::with_capacity(expected_bytes);
        for (_, start) in &offsets {
            let end = start.checked_add(bytes_per_curve).ok_or_else(|| {
                BioFormatsError::UnsupportedFormat(
                    "PicoQuant histogram payload size overflows".into(),
                )
            })?;
            pixels.extend_from_slice(&payload[*start..end]);
        }
        let description = format!(
            "PicoQuant histogram payload decoded as {curve_count} indexed-offset curves of {histogram_bins} {layout}"
        );
        Ok(Some(PicoQuantReconstruction {
            pixels,
            detector_channels: curve_count,
            lifetime_bins: 1,
            bidirectional: false,
            pixel_type,
            bits_per_pixel,
            histogram_layout: Some(layout),
            description,
        }))
    }

    fn ptu_string_value(tag: &PicoQuantTag) -> Option<String> {
        let payload = tag.payload.as_ref()?;
        match tag.tag_type {
            PTU_TAG_ANSI_STRING => {
                let len = payload
                    .iter()
                    .position(|byte| *byte == 0)
                    .unwrap_or(payload.len());
                Some(String::from_utf8_lossy(&payload[..len]).into_owned())
            }
            PTU_TAG_WIDE_STRING => {
                let mut values = Vec::new();
                for chunk in payload.chunks_exact(2) {
                    let value = u16::from_le_bytes([chunk[0], chunk[1]]);
                    if value == 0 {
                        break;
                    }
                    values.push(value);
                }
                Some(String::from_utf16_lossy(&values))
            }
            _ => None,
        }
    }

    fn reconstruct_tttr_marker_raster(
        data: &[u8],
        data_offset: usize,
        tags: &[PicoQuantTag],
        width: u32,
        height: u32,
        frames: u32,
    ) -> Result<PicoQuantReconstruction> {
        let record_count = Self::int_tag(tags, &["TTResult_NumberOfRecords"]).ok_or_else(|| {
            picoquant_event_stream_unsupported("missing TTResult_NumberOfRecords tag")
        })?;
        let record_type =
            Self::int_tag(tags, &["TTResultFormat_TTTRRecType"]).ok_or_else(|| {
                picoquant_event_stream_unsupported("missing TTResultFormat_TTTRRecType tag")
            })?;
        let Some(record_kind) = Self::tttr_record_kind(record_type) else {
            return Err(picoquant_event_stream_unsupported(&format!(
                "unsupported TTTR record type 0x{record_type:08x}"
            )));
        };
        let record_label = record_kind.label;
        if !record_kind.marker_raster_layout {
            if record_kind.family == "PicoHarp" {
                return Err(picoquant_event_stream_unsupported(&format!(
                    "{record_label} record layout is recognized for metadata only; PicoHarp T2/T3 marker-raster pixel reconstruction is not present in the local Bio-Formats Java reader set and is not inferred from HydraHarp-compatible bit packing"
                )));
            }
            return Err(picoquant_event_stream_unsupported(&format!(
                "{record_label} record layout is recognized for metadata but not supported for marker-rasterized image reconstruction"
            )));
        }
        let is_t2 = record_kind.acquisition_mode == "tttr_t2";
        let line_start_marker =
            Self::int_tag(tags, &["ImgHdr_LineStart", "ImgHdr_LineStartMarker"]).ok_or_else(
                || {
                    picoquant_event_stream_unsupported(&format!(
                        "{record_label} missing line-start marker tag"
                    ))
                },
            )?;
        let line_stop_marker = Self::int_tag(tags, &["ImgHdr_LineStop", "ImgHdr_LineStopMarker"])
            .ok_or_else(|| {
            picoquant_event_stream_unsupported(&format!(
                "{record_label} missing line-stop marker tag"
            ))
        })?;
        if record_count < 0 || line_start_marker <= 0 || line_stop_marker <= 0 {
            return Err(picoquant_event_stream_unsupported(
                "record count and line marker values must be positive",
            ));
        }
        let explicit_detector_channels = Self::int_tag(
            tags,
            &[
                "ImgHdr_DetectorChannels",
                "ImgHdr_Channels",
                "TTResult_NumberOfRoutingChannels",
            ],
        );
        let detector_channels = if let Some(detector_channels) = explicit_detector_channels {
            if detector_channels <= 0 {
                return Err(picoquant_event_stream_unsupported(
                    "detector channel count must be positive",
                ));
            }
            u32::try_from(detector_channels).map_err(|_| {
                picoquant_event_stream_unsupported("detector channel count is too large")
            })?
        } else {
            1
        };
        let explicit_lifetime_bins = Self::int_tag(
            tags,
            &[
                "ImgHdr_LifetimeBins",
                "ImgHdr_TauBins",
                "TTResult_NumberOfLifetimeBins",
            ],
        );
        if is_t2 && explicit_lifetime_bins.is_some() {
            return Err(picoquant_event_stream_unsupported(
                "T2 records do not carry lifetime dtime values",
            ));
        }
        let lifetime_bins = if let Some(lifetime_bins) = explicit_lifetime_bins {
            if lifetime_bins <= 0 {
                return Err(picoquant_event_stream_unsupported(
                    "lifetime bin count must be positive",
                ));
            }
            u32::try_from(lifetime_bins).map_err(|_| {
                picoquant_event_stream_unsupported("lifetime bin count is too large")
            })?
        } else {
            1
        };
        let lifetime_bin_width =
            Self::int_tag(tags, &["ImgHdr_LifetimeBinWidth", "ImgHdr_TauBinWidth"]).unwrap_or(1);
        if lifetime_bin_width <= 0 {
            return Err(picoquant_event_stream_unsupported(
                "lifetime bin width must be positive",
            ));
        }
        let lifetime_bin_width = u32::try_from(lifetime_bin_width)
            .map_err(|_| picoquant_event_stream_unsupported("lifetime bin width is too large"))?;
        let bidirectional = Self::int_tag(
            tags,
            &[
                "ImgHdr_BiDirectional",
                "ImgHdr_Bidirectional",
                "ImgHdr_Bidir",
            ],
        )
        .unwrap_or(0);
        let bidirectional = match bidirectional {
            0 => false,
            1 => true,
            _ => {
                return Err(picoquant_event_stream_unsupported(
                    "bidirectional scan tag must be 0 or 1",
                ));
            }
        };
        let record_count = usize::try_from(record_count).map_err(|_| {
            picoquant_event_stream_unsupported("TTTR record count is too large for this platform")
        })?;
        let record_bytes = record_count.checked_mul(4).ok_or_else(|| {
            picoquant_event_stream_unsupported("TTTR record byte count overflows")
        })?;
        let records_end = data_offset.checked_add(record_bytes).ok_or_else(|| {
            picoquant_event_stream_unsupported("TTTR record data offset overflows")
        })?;
        if records_end > data.len() {
            return Err(picoquant_event_stream_unsupported(
                "TTTR record stream is truncated",
            ));
        }

        let pixel_count = (width as usize)
            .checked_mul(height as usize)
            .and_then(|n| n.checked_mul(frames as usize))
            .and_then(|n| n.checked_mul(detector_channels as usize))
            .and_then(|n| n.checked_mul(lifetime_bins as usize))
            .ok_or_else(|| picoquant_event_stream_unsupported("output image size overflows"))?;
        let mut counts = vec![0u32; pixel_count];
        let mut sync_overflow = 0u64;
        let mut line_start_sync: Option<u64> = None;
        let mut line_photons: Vec<(u64, u32, u32)> = Vec::new();
        let mut frame = 0u32;
        let mut line = 0u32;

        for record in data[data_offset..records_end].chunks_exact(4) {
            let raw = u32::from_le_bytes([record[0], record[1], record[2], record[3]]);
            let (nsync, dtime, channel) = if is_t2 {
                (u64::from(raw & 0x01ff_ffff), 0, ((raw >> 25) & 0x3f) as u8)
            } else {
                (
                    u64::from(raw & 0x03ff),
                    (raw >> 10) & 0x7fff,
                    ((raw >> 25) & 0x3f) as u8,
                )
            };
            let special = (raw & 0x8000_0000) != 0;
            let absolute_sync = sync_overflow
                .checked_add(nsync)
                .ok_or_else(|| picoquant_event_stream_unsupported("TTTR sync time overflows"))?;

            if special {
                if channel == 0x3f {
                    let overflow_count = if nsync == 0 { 1 } else { nsync };
                    let overflow_period = if is_t2 {
                        PTU_T2_SYNC_PERIOD
                    } else {
                        PTU_T3_SYNC_PERIOD
                    };
                    sync_overflow = sync_overflow
                        .checked_add(overflow_count.checked_mul(overflow_period).ok_or_else(
                            || picoquant_event_stream_unsupported("TTTR overflow count overflows"),
                        )?)
                        .ok_or_else(|| {
                            picoquant_event_stream_unsupported("TTTR overflow overflows")
                        })?;
                    continue;
                }

                let marker = i64::from(channel);
                if marker & line_start_marker != 0 {
                    line_start_sync = Some(absolute_sync);
                    line_photons.clear();
                }
                if marker & line_stop_marker != 0 {
                    let Some(start_sync) = line_start_sync else {
                        return Err(picoquant_event_stream_unsupported(
                            "line-stop marker appeared before a line-start marker",
                        ));
                    };
                    if absolute_sync <= start_sync {
                        return Err(picoquant_event_stream_unsupported(
                            "line-stop marker does not advance sync time",
                        ));
                    }
                    if frame >= frames {
                        return Err(picoquant_event_stream_unsupported(
                            "TTTR stream contains more frames than declared",
                        ));
                    }
                    if line >= height {
                        return Err(picoquant_event_stream_unsupported(
                            "TTTR stream contains more lines than declared",
                        ));
                    }
                    let line_ticks = absolute_sync - start_sync;
                    for (photon_sync, detector_channel, lifetime_bin) in line_photons.drain(..) {
                        if photon_sync < start_sync || photon_sync >= absolute_sync {
                            continue;
                        }
                        let x = ((photon_sync - start_sync) * u64::from(width)) / line_ticks;
                        if x >= u64::from(width) {
                            continue;
                        }
                        let x = if bidirectional && line % 2 == 1 {
                            u64::from(width - 1) - x
                        } else {
                            x
                        };
                        let plane = (frame as usize)
                            .checked_mul(detector_channels as usize)
                            .and_then(|n| n.checked_add(detector_channel as usize))
                            .and_then(|n| n.checked_mul(lifetime_bins as usize))
                            .and_then(|n| n.checked_add(lifetime_bin as usize))
                            .ok_or_else(|| {
                                picoquant_event_stream_unsupported("output plane index overflows")
                            })?;
                        let idx = plane
                            .checked_mul(height as usize)
                            .and_then(|n| n.checked_add(line as usize))
                            .and_then(|n| n.checked_mul(width as usize))
                            .and_then(|n| n.checked_add(x as usize))
                            .ok_or_else(|| {
                                picoquant_event_stream_unsupported("output pixel index overflows")
                            })?;
                        counts[idx] = counts[idx].saturating_add(1);
                    }
                    line_start_sync = None;
                    line += 1;
                    if line == height {
                        line = 0;
                        frame += 1;
                    }
                }
            } else if line_start_sync.is_some() {
                let detector_channel = if explicit_detector_channels.is_some() {
                    u32::from(channel)
                } else {
                    0
                };
                if explicit_detector_channels.is_some() && detector_channel >= detector_channels {
                    return Err(picoquant_event_stream_unsupported(&format!(
                        "photon detector channel {detector_channel} exceeds declared detector channel count {detector_channels}"
                    )));
                }
                let lifetime_bin = if explicit_lifetime_bins.is_some() {
                    dtime / lifetime_bin_width
                } else {
                    0
                };
                if explicit_lifetime_bins.is_some() && lifetime_bin >= lifetime_bins {
                    return Err(picoquant_event_stream_unsupported(&format!(
                        "photon lifetime bin {lifetime_bin} exceeds declared lifetime bin count {lifetime_bins}"
                    )));
                }
                line_photons.push((absolute_sync, detector_channel, lifetime_bin));
            }
        }

        if line_start_sync.is_some() {
            return Err(picoquant_event_stream_unsupported(
                "TTTR stream ended before the current line-stop marker",
            ));
        }

        let mut out = Vec::with_capacity(pixel_count * 4);
        for count in counts {
            out.extend_from_slice(&count.to_le_bytes());
        }
        let mut description = match (detector_channels, lifetime_bins) {
            (1, 1) => format!("{record_label} marker-rasterized photon counts"),
            (_, 1) => format!(
                "{record_label} marker-rasterized photon counts split into {detector_channels} detector channels"
            ),
            (1, _) => format!(
                "{record_label} marker-rasterized photon counts split into {lifetime_bins} lifetime bins"
            ),
            _ => format!(
                "{record_label} marker-rasterized photon counts split into {detector_channels} detector channels and {lifetime_bins} lifetime bins"
            ),
        };
        if bidirectional {
            description.push_str(" with bidirectional scan correction");
        }
        Ok(PicoQuantReconstruction {
            pixels: out,
            detector_channels,
            lifetime_bins,
            bidirectional,
            pixel_type: PixelType::Uint32,
            bits_per_pixel: 32,
            histogram_layout: None,
            description,
        })
    }
}

impl Default for PicoQuantReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for PicoQuantReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("ptu") | Some("pqres"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.len() >= 6 && &header[0..6] == b"PQTTTR"
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.pixels = None;
        self.reconstruction_error = None;
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        let (tags, data_offset) = Self::parse_unified_tags(&data)?;
        let histogram_acquisition = Self::is_histogram_acquisition(&tags);
        let histogram_bins = if histogram_acquisition {
            Self::histogram_bins(&tags)
        } else {
            None
        };
        let width = Self::int_tag(&tags, &["ImgHdr_PixX", "ImgHdr_Pixels"])
            .or_else(|| {
                if histogram_acquisition {
                    histogram_bins.map(i64::from)
                } else {
                    None
                }
            })
            .ok_or_else(|| {
                if histogram_acquisition {
                    BioFormatsError::UnsupportedFormat(
                        "PicoQuant histogram acquisition missing bounded histogram bin descriptor or explicit image width".into(),
                    )
                } else {
                    BioFormatsError::UnsupportedFormat(
                        "PicoQuant PTU missing explicit image width".into(),
                    )
                }
            })?;
        let height = Self::int_tag(&tags, &["ImgHdr_PixY", "ImgHdr_Lines"])
            .or_else(|| if histogram_acquisition { Some(1) } else { None })
            .ok_or_else(|| {
                BioFormatsError::UnsupportedFormat(
                    "PicoQuant PTU missing explicit image height".into(),
                )
            })?;
        let frames = Self::int_tag(&tags, &["ImgHdr_Frames", "ImgHdr_Frame"]).unwrap_or(1);
        if width <= 0 || height <= 0 || frames <= 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "PicoQuant PTU image dimensions must be positive".into(),
            ));
        }
        let width = u32::try_from(width).map_err(|_| {
            BioFormatsError::UnsupportedFormat("PicoQuant PTU image width is too large".into())
        })?;
        let height = u32::try_from(height).map_err(|_| {
            BioFormatsError::UnsupportedFormat("PicoQuant PTU image height is too large".into())
        })?;
        let frames = u32::try_from(frames).map_err(|_| {
            BioFormatsError::UnsupportedFormat("PicoQuant PTU frame count is too large".into())
        })?;

        let mut series_metadata = HashMap::new();
        series_metadata.insert(
            "ptu.data_offset".into(),
            MetadataValue::Int(data_offset as i64),
        );
        for tag in &tags {
            if matches!(
                tag.tag_type,
                PTU_TAG_INT8
                    | PTU_TAG_BOOL8
                    | PTU_TAG_FLOAT8
                    | PTU_TAG_EMPTY8
                    | PTU_TAG_ANSI_STRING
                    | PTU_TAG_WIDE_STRING
                    | PTU_TAG_BINARY_BLOB
            ) {
                let key = if tag.index >= 0 {
                    format!("ptu.{}[{}]", tag.ident, tag.index)
                } else {
                    format!("ptu.{}", tag.ident)
                };
                let value = match tag.tag_type {
                    PTU_TAG_BOOL8 => MetadataValue::Bool(tag.value != 0),
                    PTU_TAG_FLOAT8 => MetadataValue::Float(f64::from_bits(tag.value as u64)),
                    PTU_TAG_ANSI_STRING | PTU_TAG_WIDE_STRING => {
                        MetadataValue::String(Self::ptu_string_value(tag).unwrap_or_default())
                    }
                    PTU_TAG_BINARY_BLOB => {
                        MetadataValue::Bytes(tag.payload.clone().unwrap_or_default())
                    }
                    _ => MetadataValue::Int(tag.value),
                };
                series_metadata.insert(key, value);
            }
        }

        let mut detector_channels = 1u32;
        let mut lifetime_bins = 1u32;
        let mut pixel_type = PixelType::Uint32;
        let mut bits_per_pixel = 32u8;
        if histogram_acquisition {
            series_metadata.insert(
                "ptu.acquisition_mode".into(),
                MetadataValue::String("histogram".into()),
            );
            series_metadata.insert(
                "ptu.acquisition_mode_ambiguous".into(),
                MetadataValue::Bool(false),
            );
            series_metadata.insert(
                "ptu.acquisition_mode_source".into(),
                MetadataValue::String("HistResDscr metadata".into()),
            );
            let histogram_curve_count = Self::histogram_curve_count(&tags);
            series_metadata.insert(
                "ptu.histogram_curves".into(),
                MetadataValue::Int(i64::from(histogram_curve_count)),
            );
            let histogram_indices = Self::histogram_indexed_descriptors(&tags);
            if !histogram_indices.is_empty() {
                let descriptor_indices = histogram_indices
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(",");
                let descriptor_indices_contiguous =
                    Self::histogram_indices_are_contiguous(&histogram_indices);
                series_metadata.insert(
                    "ptu.histogram_indexed_descriptor_indices".into(),
                    MetadataValue::String(descriptor_indices),
                );
                series_metadata.insert(
                    "ptu.histogram_indexed_descriptors_contiguous".into(),
                    MetadataValue::Bool(descriptor_indices_contiguous),
                );
                if !descriptor_indices_contiguous {
                    series_metadata.insert(
                        "ptu.histogram_descriptor_layout".into(),
                        MetadataValue::String("non-contiguous indexed descriptors".into()),
                    );
                } else {
                    series_metadata.insert(
                        "ptu.histogram_descriptor_layout".into(),
                        MetadataValue::String("contiguous indexed descriptors".into()),
                    );
                }
            }
            let indexed_bin_counts = Self::histogram_indexed_bin_counts(&tags);
            let indexed_bin_counts_consistent =
                Self::histogram_indexed_bin_counts_are_consistent(&indexed_bin_counts);
            if !indexed_bin_counts.is_empty() {
                let bin_counts = indexed_bin_counts
                    .iter()
                    .map(|(_, bin_count)| bin_count.to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                series_metadata.insert(
                    "ptu.histogram_indexed_bin_counts".into(),
                    MetadataValue::String(bin_counts),
                );
                series_metadata.insert(
                    "ptu.histogram_indexed_bin_counts_consistent".into(),
                    MetadataValue::Bool(indexed_bin_counts_consistent),
                );
                if !indexed_bin_counts_consistent {
                    series_metadata.insert(
                        "ptu.histogram_descriptor_layout".into(),
                        MetadataValue::String("mixed indexed bin counts".into()),
                    );
                } else if !indexed_bin_counts.is_empty()
                    && Self::histogram_indices_are_contiguous(&histogram_indices)
                {
                    series_metadata.insert(
                        "ptu.histogram_descriptor_layout".into(),
                        MetadataValue::String("contiguous equal-width indexed descriptors".into()),
                    );
                }
            }
            let indexed_payload_offsets = Self::histogram_indexed_payload_offsets(&tags)?;
            if !indexed_payload_offsets.is_empty() {
                let offset_indices = indexed_payload_offsets
                    .iter()
                    .map(|(index, _)| index.to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                let offsets = indexed_payload_offsets
                    .iter()
                    .map(|(_, offset)| offset.to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                let offset_indices_contiguous = indexed_payload_offsets
                    .iter()
                    .enumerate()
                    .all(|(expected, (index, _))| u32::try_from(expected) == Ok(*index));
                series_metadata.insert(
                    "ptu.histogram_indexed_payload_offset_indices".into(),
                    MetadataValue::String(offset_indices),
                );
                series_metadata.insert(
                    "ptu.histogram_indexed_payload_offsets".into(),
                    MetadataValue::String(offsets),
                );
                series_metadata.insert(
                    "ptu.histogram_indexed_payload_offsets_contiguous".into(),
                    MetadataValue::Bool(offset_indices_contiguous),
                );
            }
            let histogram_payload_actual_bytes = data
                .get(data_offset..)
                .map_or(0usize, |payload| payload.len());
            let histogram_payload_header_bytes =
                Self::histogram_payload_header_bytes(&tags, histogram_payload_actual_bytes)?;
            let histogram_payload_sample_actual_bytes = histogram_payload_actual_bytes
                .checked_sub(histogram_payload_header_bytes)
                .ok_or_else(|| {
                    BioFormatsError::UnsupportedFormat(
                        "PicoQuant histogram payload offset exceeds payload size".into(),
                    )
                })?;
            let histogram_payload_actual_bytes_i64 = i64::try_from(histogram_payload_actual_bytes)
                .map_err(|_| {
                    BioFormatsError::UnsupportedFormat(
                        "PicoQuant histogram payload byte count is too large".into(),
                    )
                })?;
            series_metadata.insert(
                "ptu.histogram_payload_actual_bytes".into(),
                MetadataValue::Int(histogram_payload_actual_bytes_i64),
            );
            if histogram_payload_header_bytes > 0 {
                let header_bytes_i64 =
                    i64::try_from(histogram_payload_header_bytes).map_err(|_| {
                        BioFormatsError::UnsupportedFormat(
                            "PicoQuant histogram payload offset is too large".into(),
                        )
                    })?;
                let sample_bytes_i64 = i64::try_from(histogram_payload_sample_actual_bytes)
                    .map_err(|_| {
                        BioFormatsError::UnsupportedFormat(
                            "PicoQuant histogram payload byte count is too large".into(),
                        )
                    })?;
                series_metadata.insert(
                    "ptu.histogram_payload_header_bytes".into(),
                    MetadataValue::Int(header_bytes_i64),
                );
                series_metadata.insert(
                    "ptu.histogram_payload_sample_actual_bytes".into(),
                    MetadataValue::Int(sample_bytes_i64),
                );
            }
            if let Some(histogram_bins) = histogram_bins {
                series_metadata.insert(
                    "ptu.histogram_bins".into(),
                    MetadataValue::Int(i64::from(histogram_bins)),
                );
                let histogram_payload_expected_bytes = (histogram_bins as usize)
                    .checked_mul(histogram_curve_count as usize)
                    .and_then(|samples| samples.checked_mul(4))
                    .ok_or_else(|| {
                        BioFormatsError::UnsupportedFormat(
                            "PicoQuant histogram payload size overflows".into(),
                        )
                    })?;
                let histogram_payload_expected_bytes_i64 =
                    i64::try_from(histogram_payload_expected_bytes).map_err(|_| {
                        BioFormatsError::UnsupportedFormat(
                            "PicoQuant histogram expected payload byte count is too large".into(),
                        )
                    })?;
                series_metadata.insert(
                    "ptu.histogram_payload_expected_bytes".into(),
                    MetadataValue::Int(histogram_payload_expected_bytes_i64),
                );
                series_metadata.insert(
                    "ptu.histogram_supported_payload_bytes".into(),
                    MetadataValue::String(Self::histogram_supported_expected_bytes(
                        histogram_bins,
                        histogram_curve_count,
                    )?),
                );
            }
            let histogram_sample_data_offset = data_offset
                .checked_add(histogram_payload_header_bytes)
                .ok_or_else(|| {
                    BioFormatsError::UnsupportedFormat(
                        "PicoQuant histogram payload offset overflows".into(),
                    )
                })?;
            let histogram_sample_payload =
                data.get(histogram_sample_data_offset..).ok_or_else(|| {
                    BioFormatsError::UnsupportedFormat(
                        "PicoQuant histogram payload offset is outside file".into(),
                    )
                })?;
            let histogram_compression_hints = Self::histogram_compression_hints(&tags);
            let histogram_payload_signature =
                Self::histogram_payload_signature(histogram_sample_payload);
            if !histogram_compression_hints.is_empty() {
                series_metadata
                    .insert("ptu.histogram_compressed".into(), MetadataValue::Bool(true));
                series_metadata.insert(
                    "ptu.histogram_compression".into(),
                    MetadataValue::String(histogram_compression_hints.join("; ")),
                );
            }
            if !histogram_sample_payload.is_empty()
                && (!histogram_compression_hints.is_empty()
                    || histogram_payload_signature != "unknown payload")
            {
                series_metadata.insert(
                    "ptu.histogram_payload_signature".into(),
                    MetadataValue::String(histogram_payload_signature.into()),
                );
                series_metadata.insert(
                    "ptu.histogram_payload_first_bytes".into(),
                    MetadataValue::String(Self::histogram_payload_prefix(histogram_sample_payload)),
                );
            }
            let histogram_compression_codec = Self::histogram_compression_codec(&tags);
            let mut decoded_histogram_payload = None;
            let reconstruction = if let Some(codec) = histogram_compression_codec {
                let decoded = Self::decompress_histogram_payload(histogram_sample_payload, codec)?;
                series_metadata.insert(
                    "ptu.histogram_compression_codec".into(),
                    MetadataValue::String(codec.into()),
                );
                series_metadata.insert(
                    "ptu.histogram_decompressed_payload_bytes".into(),
                    MetadataValue::Int(i64::try_from(decoded.len()).map_err(|_| {
                        BioFormatsError::UnsupportedFormat(
                            "PicoQuant histogram decompressed byte count is too large".into(),
                        )
                    })?),
                );
                let reconstruction = Self::decode_histogram_payload_bytes(&decoded, &tags)?;
                decoded_histogram_payload = Some(decoded);
                reconstruction
            } else if histogram_compression_hints.is_empty() {
                Self::decode_indexed_offset_histogram_payload(&data, data_offset, &tags)?.or(
                    Self::decode_histogram_payload(&data, histogram_sample_data_offset, &tags)?,
                )
            } else {
                None
            };
            if let Some(reconstruction) = reconstruction {
                detector_channels = reconstruction.detector_channels;
                lifetime_bins = reconstruction.lifetime_bins;
                pixel_type = reconstruction.pixel_type;
                bits_per_pixel = reconstruction.bits_per_pixel;
                series_metadata.insert(
                    "ptu.reconstruction".into(),
                    MetadataValue::String(reconstruction.description),
                );
                series_metadata.insert(
                    "ptu.histogram_curves".into(),
                    MetadataValue::Int(i64::from(detector_channels)),
                );
                series_metadata.insert(
                    "ptu.histogram_payload_ambiguous".into(),
                    MetadataValue::Bool(false),
                );
                series_metadata.insert(
                    "ptu.histogram_payload_layout".into(),
                    MetadataValue::String(
                        reconstruction.histogram_layout.unwrap_or("unknown").into(),
                    ),
                );
                let selected_histogram_payload_bytes =
                    match i64::try_from(reconstruction.pixels.len()) {
                        Ok(value) => value,
                        Err(_) => {
                            return Err(BioFormatsError::UnsupportedFormat(
                                "PicoQuant histogram expected payload byte count is too large"
                                    .into(),
                            ));
                        }
                    };
                series_metadata.insert(
                    "ptu.histogram_payload_expected_bytes".into(),
                    MetadataValue::Int(selected_histogram_payload_bytes),
                );
                series_metadata.insert(
                    "ptu.histogram_sample_bytes".into(),
                    MetadataValue::Int(pixel_type.bytes_per_sample() as i64),
                );
                self.pixels = Some(reconstruction.pixels);
            } else {
                series_metadata.insert(
                    "ptu.histogram_payload_ambiguous".into(),
                    MetadataValue::Bool(true),
                );
                let message = if let Some(histogram_bins) = histogram_bins {
                    let expected_bytes = Self::histogram_supported_expected_bytes(
                        histogram_bins,
                        histogram_curve_count,
                    )?;
                    let found_bytes = if histogram_payload_header_bytes > 0 {
                        format!(
                            "{histogram_payload_sample_actual_bytes} sample payload bytes found after {histogram_payload_header_bytes} header bytes"
                        )
                    } else {
                        format!("{histogram_payload_actual_bytes} payload bytes found")
                    };
                    if !Self::histogram_indices_are_contiguous(&histogram_indices) {
                        format!(
                            "PicoQuant histogram acquisition image-plane decoding is unsupported; non-contiguous indexed histogram descriptors require structured payload interpretation before decoding ({expected_bytes} bytes supported for exact contiguous uint8, uint16, or uint32 payloads; {found_bytes})"
                        )
                    } else if !indexed_bin_counts_consistent {
                        format!(
                            "PicoQuant histogram acquisition image-plane decoding is unsupported; mixed indexed histogram bin counts require structured payload interpretation before decoding ({expected_bytes} bytes supported only for equal-width contiguous uint8, uint16, or uint32 payloads; {found_bytes})"
                        )
                    } else if !histogram_compression_hints.is_empty() {
                        if let Some(decoded) = decoded_histogram_payload.as_ref() {
                            format!(
                                "PicoQuant histogram acquisition image-plane decoding is unsupported; compressed histogram payload decoded with {} to {} bytes, but did not match exact contiguous uint8, uint16, or uint32 histogram payload sizes ({expected_bytes} bytes supported; {found_bytes})",
                                histogram_compression_codec.unwrap_or("unknown"),
                                decoded.len()
                            )
                        } else {
                            format!(
                                "PicoQuant histogram acquisition image-plane decoding is unsupported; compressed histogram payload decoding is unsupported for declared compression {} (payload signature {}; first bytes [{}]; {expected_bytes} bytes supported only for exact contiguous uint8, uint16, or uint32 payloads; {found_bytes})",
                                histogram_compression_hints.join("; "),
                                histogram_payload_signature,
                                Self::histogram_payload_prefix(histogram_sample_payload)
                            )
                        }
                    } else {
                        format!(
                            "PicoQuant histogram acquisition image-plane decoding is unsupported; expected an exact contiguous uint8, uint16, or uint32 histogram payload matching descriptor bins and curves ({expected_bytes} bytes supported; {found_bytes})"
                        )
                    }
                } else {
                    "PicoQuant histogram acquisition image-plane decoding is unsupported; missing bounded histogram bin descriptor".to_string()
                };
                series_metadata.insert(
                    "ptu.reconstruction_unsupported".into(),
                    MetadataValue::String(message.clone()),
                );
                self.reconstruction_error = Some(message);
            }
        } else {
            let tttr_record_type = Self::int_tag(&tags, &["TTResultFormat_TTTRRecType"]);
            let tttr_record_kind =
                Self::annotate_tttr_acquisition_mode(&mut series_metadata, tttr_record_type);
            if let Some(sync_resolution) = Self::float_tag(
                &tags,
                &["MeasDesc_GlobalResolution", "MeasDesc_SyncResolution"],
            ) {
                if sync_resolution > 0.0 {
                    series_metadata.insert(
                        "ptu.sync_resolution_seconds".into(),
                        MetadataValue::Float(sync_resolution),
                    );
                }
            }
            let reconstruction = Self::reconstruct_tttr_marker_raster(
                &data,
                data_offset,
                &tags,
                width,
                height,
                frames,
            );
            match reconstruction {
                Ok(reconstruction) => {
                    detector_channels = reconstruction.detector_channels;
                    lifetime_bins = reconstruction.lifetime_bins;
                    pixel_type = reconstruction.pixel_type;
                    bits_per_pixel = reconstruction.bits_per_pixel;
                    series_metadata.insert(
                        "ptu.reconstruction".into(),
                        MetadataValue::String(reconstruction.description),
                    );
                    series_metadata.insert(
                        "ptu.detector_channels".into(),
                        MetadataValue::Int(i64::from(detector_channels)),
                    );
                    series_metadata.insert(
                        "ptu.lifetime_bins".into(),
                        MetadataValue::Int(i64::from(lifetime_bins)),
                    );
                    series_metadata.insert(
                        "ptu.bidirectional".into(),
                        MetadataValue::Bool(reconstruction.bidirectional),
                    );
                    if matches!(
                        tttr_record_kind,
                        Some(PicoQuantRecordKind {
                            acquisition_mode: "tttr_t3",
                            ..
                        })
                    ) {
                        if let Some(lifetime_resolution) = Self::float_tag(
                            &tags,
                            &["MeasDesc_Resolution", "MeasDesc_DTimeResolution"],
                        ) {
                            if lifetime_resolution > 0.0 {
                                series_metadata.insert(
                                    "ptu.lifetime_dtime_resolution_seconds".into(),
                                    MetadataValue::Float(lifetime_resolution),
                                );
                                let lifetime_bin_width = Self::int_tag(
                                    &tags,
                                    &["ImgHdr_LifetimeBinWidth", "ImgHdr_TauBinWidth"],
                                )
                                .unwrap_or(1);
                                if lifetime_bin_width > 0 {
                                    series_metadata.insert(
                                        "ptu.lifetime_bin_width_dtime".into(),
                                        MetadataValue::Int(lifetime_bin_width),
                                    );
                                    let lifetime_bin_width_seconds =
                                        lifetime_resolution * lifetime_bin_width as f64;
                                    series_metadata.insert(
                                        "ptu.lifetime_bin_width_seconds".into(),
                                        MetadataValue::Float(lifetime_bin_width_seconds),
                                    );
                                    if lifetime_bins > 1 {
                                        series_metadata.insert(
                                            "ptu.lifetime_range_seconds".into(),
                                            MetadataValue::Float(
                                                lifetime_bin_width_seconds
                                                    * f64::from(lifetime_bins),
                                            ),
                                        );
                                    }
                                }
                            }
                        }
                    }
                    self.pixels = Some(reconstruction.pixels);
                }
                Err(BioFormatsError::UnsupportedFormat(message)) => {
                    series_metadata.insert(
                        "ptu.reconstruction_unsupported".into(),
                        MetadataValue::String(message.clone()),
                    );
                    self.reconstruction_error = Some(message);
                }
                Err(err) => return Err(err),
            }
        }
        let size_c = detector_channels
            .checked_mul(lifetime_bins)
            .ok_or_else(|| {
                BioFormatsError::UnsupportedFormat("PicoQuant PTU channel count overflows".into())
            })?;
        let image_count = frames.checked_mul(size_c).ok_or_else(|| {
            BioFormatsError::UnsupportedFormat(
                "PicoQuant PTU frame/channel plane count overflows".into(),
            )
        })?;

        self.path = Some(path.to_path_buf());
        self.meta = Some(ImageMetadata {
            size_x: width,
            size_y: height,
            size_z: 1,
            size_c,
            size_t: frames,
            pixel_type,
            bits_per_pixel,
            image_count,
            dimension_order: DimensionOrder::XYCZT,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: true,
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
        self.pixels = None;
        self.reconstruction_error = None;
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
        let pixels = self
            .pixels
            .as_ref()
            .ok_or_else(|| {
                BioFormatsError::UnsupportedFormat(
                    self.reconstruction_error
                        .clone()
                        .unwrap_or_else(|| {
                            "PicoQuant TTTR image reconstruction unavailable: no supported TTTR reconstruction path was initialized".into()
                        }),
                )
            })?;
        let plane_bytes = (meta.size_x as usize)
            .checked_mul(meta.size_y as usize)
            .and_then(|n| n.checked_mul(meta.pixel_type.bytes_per_sample()))
            .ok_or_else(|| BioFormatsError::Format("PicoQuant plane size overflows".into()))?;
        let start = (plane_index as usize)
            .checked_mul(plane_bytes)
            .ok_or_else(|| BioFormatsError::Format("PicoQuant plane offset overflows".into()))?;
        let end = start
            .checked_add(plane_bytes)
            .ok_or_else(|| BioFormatsError::Format("PicoQuant plane end overflows".into()))?;
        pixels
            .get(start..end)
            .map(|plane| plane.to_vec())
            .ok_or_else(|| {
                BioFormatsError::InvalidData("PicoQuant cached plane is truncated".into())
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
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        validate_region("PicoQuant", meta.size_x, meta.size_y, x, y, w, h)?;
        let meta = meta.clone();
        let full = self.open_bytes(plane_index)?;
        crop_full_plane("PicoQuant", &full, &meta, 1, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn resolution_count(&self) -> usize {
        1
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if level != 0 {
            Err(BioFormatsError::Format(format!(
                "resolution {} out of range",
                level
            )))
        } else {
            Ok(())
        }
    }
}

// ===========================================================================
// Helpers for strict raw SPM subsets.
// ===========================================================================

fn unsupported_raw_spm(format_name: &str) -> BioFormatsError {
    BioFormatsError::UnsupportedFormat(format!(
        "{format_name} native binary layout is unsupported unless explicit strict raw data is present; refusing heuristic dimensions"
    ))
}

#[derive(Debug, Clone, Copy)]
struct SpmStrictRawLayout {
    data_offset: u64,
    plane_bytes: u64,
}

fn read_le_u32_spm(data: &[u8], offset: usize, label: &str, format_name: &str) -> Result<u32> {
    let bytes = data.get(offset..offset + 4).ok_or_else(|| {
        BioFormatsError::UnsupportedFormat(format!("{format_name} strict header missing {label}"))
    })?;
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn read_le_u16_spm(data: &[u8], offset: usize, label: &str, format_name: &str) -> Result<u16> {
    let bytes = data.get(offset..offset + 2).ok_or_else(|| {
        BioFormatsError::UnsupportedFormat(format!("{format_name} strict header missing {label}"))
    })?;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn read_le_u64_spm(data: &[u8], offset: usize, label: &str, format_name: &str) -> Result<u64> {
    let bytes = data.get(offset..offset + 8).ok_or_else(|| {
        BioFormatsError::UnsupportedFormat(format!("{format_name} strict header missing {label}"))
    })?;
    Ok(u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ]))
}

fn parse_strict_spm_raw(
    path: &Path,
    magic: &[u8],
    format_name: &str,
) -> Result<(ImageMetadata, SpmStrictRawLayout)> {
    let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
    if !data.starts_with(magic) {
        return Err(unsupported_raw_spm(format_name));
    }

    let width_offset = magic.len();
    let height_offset = width_offset + 4;
    let planes_offset = height_offset + 4;
    let pixel_type_offset = planes_offset + 4;
    let reserved_offset = pixel_type_offset + 2;
    let data_offset_offset = reserved_offset + 2;
    let fixed_header_len = data_offset_offset + 8;

    let width = read_le_u32_spm(&data, width_offset, "width", format_name)?;
    let height = read_le_u32_spm(&data, height_offset, "height", format_name)?;
    let planes = read_le_u32_spm(&data, planes_offset, "plane count", format_name)?;
    if width == 0 || height == 0 || planes == 0 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "{format_name} strict header dimensions must be non-zero"
        )));
    }

    let pixel_type_code = read_le_u16_spm(&data, pixel_type_offset, "pixel type", format_name)?;
    let (pixel_type, bits_per_pixel) = match pixel_type_code {
        1 => (PixelType::Uint8, 8),
        2 => (PixelType::Uint16, 16),
        _ => {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "{format_name} strict header has unsupported pixel type code {pixel_type_code}"
            )));
        }
    };
    let reserved = read_le_u16_spm(&data, reserved_offset, "reserved field", format_name)?;
    if reserved != 0 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "{format_name} strict header reserved field must be zero"
        )));
    }

    let data_offset = read_le_u64_spm(&data, data_offset_offset, "data offset", format_name)?;
    if data_offset < fixed_header_len as u64 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "{format_name} strict data offset points into header"
        )));
    }

    let plane_bytes = (width as u64)
        .checked_mul(height as u64)
        .and_then(|n| n.checked_mul(pixel_type.bytes_per_sample() as u64))
        .ok_or_else(|| BioFormatsError::Format(format!("{format_name} plane size overflows")))?;
    let payload_len = plane_bytes
        .checked_mul(planes as u64)
        .ok_or_else(|| BioFormatsError::Format(format!("{format_name} payload size overflows")))?;
    let expected_len = data_offset
        .checked_add(payload_len)
        .ok_or_else(|| BioFormatsError::Format(format!("{format_name} file size overflows")))?;
    if data.len() as u64 != expected_len {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "{format_name} strict payload length mismatch: got {}, expected {expected_len}",
            data.len()
        )));
    }

    Ok((
        ImageMetadata {
            size_x: width,
            size_y: height,
            size_z: 1,
            size_c: 1,
            size_t: planes,
            pixel_type,
            bits_per_pixel,
            image_count: planes,
            dimension_order: DimensionOrder::XYCZT,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: true,
            resolution_count: 1,
            thumbnail: false,
            series_metadata: HashMap::new(),
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        },
        SpmStrictRawLayout {
            data_offset,
            plane_bytes,
        },
    ))
}

fn read_strict_spm_raw_plane(
    path: &Path,
    layout: SpmStrictRawLayout,
    plane_index: u32,
) -> Result<Vec<u8>> {
    let offset = layout
        .data_offset
        .checked_add(
            layout
                .plane_bytes
                .checked_mul(plane_index as u64)
                .ok_or_else(|| {
                    BioFormatsError::Format("SPM strict plane offset overflows".into())
                })?,
        )
        .ok_or_else(|| BioFormatsError::Format("SPM strict plane offset overflows".into()))?;
    let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
    f.seek(SeekFrom::Start(offset))
        .map_err(BioFormatsError::Io)?;
    let mut buf = vec![0u8; layout.plane_bytes as usize];
    f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
    Ok(buf)
}

// ===========================================================================
// Real binary reader — RHK Technology SPM
// ===========================================================================

/// RHK Technology SPM reader (`.sm2`, `.sm3`, `.sm4`).
///
/// Port of Bio-Formats `RHKReader.java`. The file begins with a 512-byte
/// page header. There are two layouts:
///
///   * **XPM** (binary): the first little-endian `short` equals `0xaa`.
///     Integer fields live at fixed offsets (image/page/data/line type at 40,
///     `sizeX`/`sizeY` after them, then the pixel offset; float X/Y scales
///     follow).
///   * **text**: a space-separated ASCII record at offset 32 carries the same
///     type codes and dimensions; pixels start at the fixed 512-byte boundary
///     and the X/Y scales come from two further 32-byte axis records.
///
/// `dataType` selects the pixel type (0=float32, 1=int16, 2=int32, 3=uint8).
/// In the text layout the X/Y scale signs drive `invertX`/`invertY`, which
/// mirror the stored plane horizontally/vertically when reading pixels.
pub struct RhkReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    pixel_offset: u64,
    invert_x: bool,
    invert_y: bool,
}

impl RhkReader {
    const HEADER_SIZE: u64 = 512;

    pub fn new() -> Self {
        RhkReader {
            path: None,
            meta: None,
            pixel_offset: 0,
            invert_x: false,
            invert_y: false,
        }
    }

    fn read_i32_le(data: &[u8], offset: usize, label: &str) -> Result<i32> {
        let bytes = data.get(offset..offset + 4).ok_or_else(|| {
            BioFormatsError::UnsupportedFormat(format!("RHK SPM header missing {label}"))
        })?;
        Ok(i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn read_f32_le(data: &[u8], offset: usize, label: &str) -> Result<f32> {
        let bytes = data.get(offset..offset + 4).ok_or_else(|| {
            BioFormatsError::UnsupportedFormat(format!("RHK SPM header missing {label}"))
        })?;
        Ok(f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    /// Read a fixed-width ASCII record (Java `readString(len).trim()`).
    fn read_string(data: &[u8], offset: usize, len: usize) -> String {
        let end = (offset + len).min(data.len());
        let slice = data.get(offset..end).unwrap_or(&[]);
        // Stop at the first NUL like Java's String construction over the bytes,
        // then trim surrounding whitespace.
        let nul = slice.iter().position(|&b| b == 0).unwrap_or(slice.len());
        String::from_utf8_lossy(&slice[..nul]).trim().to_string()
    }

    /// Map RHK dataType code → (PixelType, bits-per-pixel).
    fn pixel_type_from_data_type(data_type: i32) -> Result<(PixelType, u8)> {
        match data_type {
            0 => Ok((PixelType::Float32, 32)),
            1 => Ok((PixelType::Int16, 16)),
            2 => Ok((PixelType::Int32, 32)),
            3 => Ok((PixelType::Uint8, 8)),
            other => Err(BioFormatsError::UnsupportedFormat(format!(
                "RHK SPM unsupported data type: {other}"
            ))),
        }
    }
}

impl Default for RhkReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for RhkReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("sm2") | Some("sm3") | Some("sm4"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        if data.len() < Self::HEADER_SIZE as usize {
            return Err(BioFormatsError::UnsupportedFormat(
                "RHK SPM file is shorter than the 512-byte page header".into(),
            ));
        }

        // Java: little-endian; xpm = (readShort() == 0xaa).
        let first_short = i16::from_le_bytes([data[0], data[1]]);
        let xpm = first_short == 0xaa;

        let mut width: u32;
        let mut height: u32;
        let pixel_offset: u64;
        let data_type: i32;
        let mut invert_x = false;
        let mut invert_y = false;
        let x_scale: f64;
        let y_scale: f64;

        if xpm {
            // seek(40): imageType, pageType, dataType, lineType ints.
            let _image_type = Self::read_i32_le(&data, 40, "image type")?;
            let _page_type = Self::read_i32_le(&data, 44, "page type")?;
            data_type = Self::read_i32_le(&data, 48, "data type")?;
            let _line_type = Self::read_i32_le(&data, 52, "line type")?;
            // skipBytes(8) → offset 56..64.
            width = Self::read_i32_le(&data, 64, "width")? as u32;
            height = Self::read_i32_le(&data, 68, "height")? as u32;
            // skipBytes(16) → offset 72..88.
            pixel_offset = Self::read_i32_le(&data, 88, "pixel offset")? as u32 as u64;
            // After the int read, the stream is at offset 92; skipBytes(8) → 100.
            x_scale = Self::read_f32_le(&data, 100, "x scale")? as f64 * 1_000_000.0;
            y_scale = Self::read_f32_le(&data, 104, "y scale")? as f64 * 1_000_000.0;
        } else {
            // seek(32): 32-byte space-separated ASCII type/dimension record.
            let type_record = Self::read_string(&data, 32, 32);
            let type_data: Vec<&str> = type_record.split_whitespace().collect();
            let parse = |idx: usize, label: &str| -> Result<i32> {
                type_data
                    .get(idx)
                    .and_then(|v| v.parse::<i32>().ok())
                    .ok_or_else(|| {
                        BioFormatsError::UnsupportedFormat(format!(
                            "RHK SPM text header missing {label}"
                        ))
                    })
            };
            let _image_type = parse(0, "image type")?;
            data_type = parse(1, "data type")?;
            let _line_type = parse(2, "line type")?;
            width = parse(3, "width")? as u32;
            height = parse(4, "height")? as u32;
            let _page_type = parse(6, "page type")?;
            pixel_offset = Self::HEADER_SIZE;

            // Two further 32-byte axis records (X then Y); field [1] is the scale.
            let x_axis = Self::read_string(&data, 64, 32);
            let y_axis = Self::read_string(&data, 96, 32);
            let x_axis_fields: Vec<&str> = x_axis.split_whitespace().collect();
            let y_axis_fields: Vec<&str> = y_axis.split_whitespace().collect();
            let x_raw = x_axis_fields
                .get(1)
                .and_then(|v| v.parse::<f64>().ok())
                .ok_or_else(|| {
                    BioFormatsError::UnsupportedFormat("RHK SPM text header missing X scale".into())
                })?;
            let y_raw = y_axis_fields
                .get(1)
                .and_then(|v| v.parse::<f64>().ok())
                .ok_or_else(|| {
                    BioFormatsError::UnsupportedFormat("RHK SPM text header missing Y scale".into())
                })?;
            x_scale = x_raw * 1_000_000.0;
            y_scale = y_raw * 1_000_000.0;
            invert_x = x_scale < 0.0;
            invert_y = y_scale > 0.0;
        }

        if width == 0 || height == 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "RHK SPM header contains invalid image dimensions".into(),
            ));
        }
        let _ = (&mut width, &mut height);

        let (pixel_type, bits_per_pixel) = Self::pixel_type_from_data_type(data_type)?;
        let bps = pixel_type.bytes_per_sample() as u64;
        let expected = pixel_offset
            .checked_add(
                (width as u64)
                    .checked_mul(height as u64)
                    .and_then(|p| p.checked_mul(bps))
                    .ok_or_else(|| {
                        BioFormatsError::Format("RHK SPM plane size overflows".into())
                    })?,
            )
            .ok_or_else(|| BioFormatsError::Format("RHK SPM plane size overflows".into()))?;
        if expected > data.len() as u64 {
            return Err(BioFormatsError::UnsupportedFormat(
                "RHK SPM pixel payload is shorter than declared dimensions".into(),
            ));
        }

        // seek(352): 32-byte description string.
        let description = Self::read_string(&data, 352, 32);
        let mut series_metadata = HashMap::new();
        if !description.is_empty() {
            series_metadata.insert(
                "Description".into(),
                crate::common::metadata::MetadataValue::String(description),
            );
        }
        series_metadata.insert(
            "X scale (um)".into(),
            crate::common::metadata::MetadataValue::Float(x_scale),
        );
        series_metadata.insert(
            "Y scale (um)".into(),
            crate::common::metadata::MetadataValue::Float(y_scale),
        );

        self.pixel_offset = pixel_offset;
        self.invert_x = invert_x;
        self.invert_y = invert_y;
        self.path = Some(path.to_path_buf());
        self.meta = Some(ImageMetadata {
            size_x: width,
            size_y: height,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type,
            bits_per_pixel,
            image_count: 1,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: true,
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
        self.pixel_offset = 0;
        self.invert_x = false;
        self.invert_y = false;
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
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let (sx, sy) = (meta.size_x, meta.size_y);
        self.open_bytes_region(plane_index, 0, 0, sx, sy)
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
            .meta
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?
            .clone();
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let bps = meta.pixel_type.bytes_per_sample();
        let sx = meta.size_x as usize;
        let sy = meta.size_y as usize;
        let n_bytes = sx
            .checked_mul(sy)
            .and_then(|p| p.checked_mul(bps))
            .ok_or_else(|| BioFormatsError::Format("RHK SPM plane size overflows".into()))?;

        let path = self.path.clone().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = std::fs::File::open(&path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(self.pixel_offset))
            .map_err(BioFormatsError::Io)?;
        let mut plane = vec![0u8; n_bytes];
        f.read_exact(&mut plane).map_err(BioFormatsError::Io)?;

        // RHKReader.java reads pixels from the mirrored corner and then flips
        // the returned tile. Mirroring the whole stored plane (per axis) before
        // cropping at (x,y,w,h) is equivalent and reuses the crop helper.
        let row_len = sx * bps;
        if self.invert_y {
            for row in 0..sy / 2 {
                let top = row * row_len;
                let bottom = (sy - row - 1) * row_len;
                for i in 0..row_len {
                    plane.swap(top + i, bottom + i);
                }
            }
        }
        if self.invert_x {
            for row in 0..sy {
                let base = row * row_len;
                for col in 0..sx / 2 {
                    let left = base + col * bps;
                    let right = base + (sx - col - 1) * bps;
                    for i in 0..bps {
                        plane.swap(left + i, right + i);
                    }
                }
            }
        }

        crop_full_plane("RHK SPM", &plane, &meta, 1, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn resolution_count(&self) -> usize {
        1
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if level != 0 {
            Err(BioFormatsError::Format(format!(
                "resolution {} out of range",
                level
            )))
        } else {
            Ok(())
        }
    }
}

// ===========================================================================
// Raw binary reader — Quesant AFM
// ===========================================================================

/// Quesant AFM reader (`.afm`).
///
pub struct QuesantReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    layout: Option<QuesantLayout>,
}

impl QuesantReader {
    const STRICT_RAW_MAGIC: &'static [u8] = b"BFQUESANTAFMRAW!";
    const MAX_HEADER_SIZE: usize = 1024;

    pub fn new() -> Self {
        QuesantReader {
            path: None,
            meta: None,
            layout: None,
        }
    }

    fn parse_native(path: &Path) -> Result<(ImageMetadata, QuesantLayout)> {
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        if data.len() < 10 {
            return Err(unsupported_raw_spm("Quesant AFM"));
        }

        let header_len = data.len().min(Self::MAX_HEADER_SIZE);
        let mut pixels_offset: Option<usize> = None;
        let mut series_metadata = HashMap::new();
        let mut pos = 0usize;
        while pos + 8 <= header_len {
            let code = &data[pos..pos + 4];
            let offset = u32::from_le_bytes(data[pos + 4..pos + 8].try_into().unwrap()) as usize;
            pos += 8;
            if offset == 0 || offset >= data.len() {
                continue;
            }

            match code {
                b"IMAG" => pixels_offset = Some(offset),
                b"SDES" | b"DATE" => {
                    let end = data[offset..]
                        .iter()
                        .position(|b| *b == 0)
                        .map(|n| offset + n)
                        .unwrap_or(data.len());
                    let value = String::from_utf8_lossy(&data[offset..end])
                        .trim()
                        .to_string();
                    let key = if code == b"SDES" {
                        "Quesant description"
                    } else {
                        "Quesant acquisition date"
                    };
                    if !value.is_empty() {
                        series_metadata.insert(key.into(), MetadataValue::String(value));
                    }
                }
                b"DESC" if offset + 2 <= data.len() => {
                    let len =
                        u16::from_le_bytes(data[offset..offset + 2].try_into().unwrap()) as usize;
                    if offset + 2 + len <= data.len() {
                        let value = String::from_utf8_lossy(&data[offset + 2..offset + 2 + len])
                            .trim()
                            .to_string();
                        if !value.is_empty() {
                            series_metadata
                                .insert("Quesant description".into(), MetadataValue::String(value));
                        }
                    }
                }
                b"HARD" if offset + 42 <= data.len() => {
                    let x_size = f32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
                    let scan_rate =
                        f32::from_le_bytes(data[offset + 4..offset + 8].try_into().unwrap());
                    let tunnel_current =
                        f32::from_le_bytes(data[offset + 8..offset + 12].try_into().unwrap())
                            * 10.0
                            / 32768.0;
                    let integral_gain =
                        f32::from_le_bytes(data[offset + 24..offset + 28].try_into().unwrap());
                    let proportional_gain =
                        f32::from_le_bytes(data[offset + 28..offset + 32].try_into().unwrap());
                    let is_stm =
                        u16::from_le_bytes(data[offset + 32..offset + 34].try_into().unwrap())
                            == 10;
                    let dynamic_range =
                        f32::from_le_bytes(data[offset + 34..offset + 38].try_into().unwrap());
                    series_metadata.insert("Scan size".into(), MetadataValue::Float(x_size as f64));
                    series_metadata.insert(
                        "Scan rate (Hz)".into(),
                        MetadataValue::Float(scan_rate as f64),
                    );
                    series_metadata.insert(
                        "Tunnel current".into(),
                        MetadataValue::Float(tunnel_current as f64),
                    );
                    series_metadata.insert("Is STM image".into(), MetadataValue::Bool(is_stm));
                    series_metadata.insert(
                        "Integral gain".into(),
                        MetadataValue::Float(integral_gain as f64),
                    );
                    series_metadata.insert(
                        "Proportional gain".into(),
                        MetadataValue::Float(proportional_gain as f64),
                    );
                    series_metadata.insert(
                        "Z dynamic range".into(),
                        MetadataValue::Float(dynamic_range as f64),
                    );
                }
                _ => {}
            }
        }

        let pixels_offset = pixels_offset.ok_or_else(|| unsupported_raw_spm("Quesant AFM"))?;
        if pixels_offset + 2 > data.len() {
            return Err(BioFormatsError::UnsupportedFormat(
                "Quesant AFM image header is truncated".into(),
            ));
        }
        let size_x =
            u16::from_le_bytes(data[pixels_offset..pixels_offset + 2].try_into().unwrap()) as u32;
        if size_x == 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "Quesant AFM image dimension must be non-zero".into(),
            ));
        }
        let data_offset = pixels_offset as u64 + 2;
        let plane_bytes = (size_x as u64)
            .checked_mul(size_x as u64)
            .and_then(|n| n.checked_mul(2))
            .ok_or_else(|| BioFormatsError::Format("Quesant AFM plane size overflows".into()))?;
        let expected = data_offset
            .checked_add(plane_bytes)
            .ok_or_else(|| BioFormatsError::Format("Quesant AFM file size overflows".into()))?;
        if (data.len() as u64) < expected {
            return Err(BioFormatsError::UnsupportedFormat(
                "Quesant AFM pixel payload is shorter than declared dimensions".into(),
            ));
        }

        Ok((
            ImageMetadata {
                size_x,
                size_y: size_x,
                size_z: 1,
                size_c: 1,
                size_t: 1,
                pixel_type: PixelType::Uint16,
                bits_per_pixel: 16,
                image_count: 1,
                dimension_order: DimensionOrder::XYZCT,
                is_rgb: false,
                is_interleaved: false,
                is_indexed: false,
                is_little_endian: true,
                resolution_count: 1,
                thumbnail: false,
                series_metadata,
                lookup_table: None,
                modulo_z: None,
                modulo_c: None,
                modulo_t: None,
            },
            QuesantLayout::Native {
                data_offset,
                plane_bytes,
            },
        ))
    }
}

#[derive(Debug, Clone, Copy)]
enum QuesantLayout {
    Strict(SpmStrictRawLayout),
    Native { data_offset: u64, plane_bytes: u64 },
}

impl Default for QuesantReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for QuesantReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        // Note: .afm is also used by VeecoReader (Nanoscope). Quesant AFM
        // files lack the NANOSCOPE header, so this reader is a fallback.
        matches!(ext.as_deref(), Some("afm"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        _header.starts_with(Self::STRICT_RAW_MAGIC)
            || _header[.._header.len().min(Self::MAX_HEADER_SIZE)]
                .windows(8)
                .any(|w| &w[..4] == b"IMAG")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.layout = None;
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        let (meta, layout) = if data.starts_with(Self::STRICT_RAW_MAGIC) {
            let (meta, layout) = parse_strict_spm_raw(path, Self::STRICT_RAW_MAGIC, "Quesant AFM")?;
            (meta, QuesantLayout::Strict(layout))
        } else {
            Self::parse_native(path)?
        };
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
        match self.layout.ok_or(BioFormatsError::NotInitialized)? {
            QuesantLayout::Strict(layout) => read_strict_spm_raw_plane(
                self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?,
                layout,
                plane_index,
            ),
            QuesantLayout::Native {
                data_offset,
                plane_bytes,
            } => {
                let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
                let offset = data_offset
                    .checked_add(plane_bytes.checked_mul(plane_index as u64).ok_or_else(|| {
                        BioFormatsError::Format("Quesant AFM plane offset overflows".into())
                    })?)
                    .ok_or_else(|| {
                        BioFormatsError::Format("Quesant AFM plane offset overflows".into())
                    })?;
                let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
                f.seek(SeekFrom::Start(offset))
                    .map_err(BioFormatsError::Io)?;
                let n_bytes = usize::try_from(plane_bytes).map_err(|_| {
                    BioFormatsError::Format("Quesant AFM plane size overflows".into())
                })?;
                let mut buf = vec![0; n_bytes];
                f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
                Ok(buf)
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
        let meta = self
            .meta
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?
            .clone();
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let full = self.open_bytes(plane_index)?;
        crop_full_plane("Quesant AFM", &full, &meta, 1, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn resolution_count(&self) -> usize {
        1
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if level != 0 {
            Err(BioFormatsError::Format(format!(
                "resolution {} out of range",
                level
            )))
        } else {
            Ok(())
        }
    }
}

// ===========================================================================
// TIFF reader — JPK Instruments AFM
// ===========================================================================

/// JPK Instruments AFM reader (`.jpk`).
///
/// Port of JPKReader.java: a `.jpk` file IS a TIFF (JPKReader extends
/// BaseTiffReader). Exposes two series: series 0 = IFD 0 (a single-plane
/// thumbnail), series 1 = IFDs 1..n grouped as a T-stack.
pub struct JpkReader {
    extracted_path: Option<PathBuf>,
    inner: crate::tiff::TiffReader,
    meta: Option<ImageMetadata>,
    is_tiff: bool,
}

impl JpkReader {
    pub fn new() -> Self {
        JpkReader {
            extracted_path: None,
            inner: crate::tiff::TiffReader::new(),
            meta: None,
            is_tiff: false,
        }
    }
}

impl Default for JpkReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for JpkReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("jpk"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let _ = self.inner.close();
        // A .jpk file is itself a TIFF; parse it directly.
        self.inner.set_id(path)?;

        let ifd_count = self.inner.ifd_count();
        if ifd_count == 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "JPK: TIFF contains no IFDs".to_string(),
            ));
        }

        // Build a per-IFD metadata lookup from the default series grouping so we
        // can reconstruct accurate dimensions/pixel-type for the JPK layout.
        // We clone existing TiffSeries values (the type is not re-exported) and
        // mutate their public fields rather than constructing literals.
        let default_series = self.inner.series_list();
        let mut meta_for_ifd: Vec<Option<ImageMetadata>> = vec![None; ifd_count];
        for series in default_series {
            for &idx in &series.ifd_indices {
                if idx < ifd_count {
                    meta_for_ifd[idx] = Some(series.metadata.clone());
                }
            }
        }
        // A template TiffSeries to clone (carries the unexported type).
        let template = default_series[0].clone();
        let ifd_meta = |idx: usize| -> ImageMetadata {
            meta_for_ifd
                .get(idx)
                .and_then(|m| m.clone())
                .unwrap_or_else(|| template.metadata.clone())
        };

        let mut new_series = Vec::new();

        // Series 0: IFD 0 only, a single-plane thumbnail.
        {
            let mut s = template.clone();
            let mut m = ifd_meta(0);
            m.size_z = 1;
            m.size_t = 1;
            m.image_count = 1;
            m.dimension_order = crate::common::metadata::DimensionOrder::XYCZT;
            s.ifd_indices = vec![0];
            s.plane_ifd_indices = Vec::new();
            s.sub_resolutions = Vec::new();
            s.metadata = m;
            new_series.push(s);
        }

        // Series 1 (only if there is more than one IFD): IFDs 1..n as a T-stack.
        if ifd_count > 1 {
            let t = (ifd_count - 1) as u32;
            let mut s = template.clone();
            let mut m = ifd_meta(1);
            m.size_z = 1;
            m.size_t = t;
            m.size_c = if m.is_rgb { m.size_c } else { 1 };
            m.image_count = t;
            m.dimension_order = crate::common::metadata::DimensionOrder::XYCZT;
            s.ifd_indices = (1..ifd_count).collect();
            s.plane_ifd_indices = Vec::new();
            s.sub_resolutions = Vec::new();
            s.metadata = m;
            new_series.push(s);
        }

        self.inner.replace_series(new_series);
        self.inner.set_series(0)?;
        self.meta = Some(self.inner.metadata().clone());
        self.is_tiff = true;

        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        if self.is_tiff {
            let _ = self.inner.close();
        }
        if let Some(p) = self.extracted_path.take() {
            let _ = std::fs::remove_file(p);
        }
        self.meta = None;
        self.is_tiff = false;
        Ok(())
    }

    fn series_count(&self) -> usize {
        if self.is_tiff {
            self.inner.series_count()
        } else {
            0
        }
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.is_tiff {
            self.inner.set_series(s)
        } else if self.meta.is_none() {
            Err(BioFormatsError::NotInitialized)
        } else if s != 0 {
            Err(BioFormatsError::SeriesOutOfRange(s))
        } else {
            Ok(())
        }
    }

    fn series(&self) -> usize {
        if self.is_tiff {
            self.inner.series()
        } else {
            0
        }
    }

    fn metadata(&self) -> &ImageMetadata {
        if self.is_tiff {
            self.inner.metadata()
        } else {
            self.meta
                .as_ref()
                .unwrap_or(crate::common::reader::uninitialized_metadata())
        }
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        if self.is_tiff {
            return self.inner.open_bytes(plane_index);
        }
        let _ = plane_index;
        Err(BioFormatsError::UnsupportedFormat(
            "JPK ZIP archive does not contain delegated TIFF data".to_string(),
        ))
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        if self.is_tiff {
            return self.inner.open_bytes_region(plane_index, x, y, w, h);
        }
        let _ = (plane_index, x, y, w, h);
        Err(BioFormatsError::UnsupportedFormat(
            "JPK ZIP archive does not contain delegated TIFF data".to_string(),
        ))
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        if self.is_tiff {
            return self.inner.open_thumb_bytes(plane_index);
        }
        let _ = plane_index;
        Err(BioFormatsError::UnsupportedFormat(
            "JPK ZIP archive does not contain delegated TIFF data".to_string(),
        ))
    }

    fn resolution_count(&self) -> usize {
        if self.is_tiff {
            self.inner.resolution_count()
        } else {
            1
        }
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if self.is_tiff {
            self.inner.set_resolution(level)
        } else if level != 0 {
            Err(BioFormatsError::Format(format!(
                "resolution {} out of range",
                level
            )))
        } else {
            Ok(())
        }
    }
}

// ===========================================================================
// Raw binary reader — WaTom SPM
// ===========================================================================

/// WA Technology TOP reader (`.wat`, plus legacy aliases).
///
/// Java Bio-Formats uses a 4864-byte little-endian header followed by raw
/// signed 16-bit pixels.
pub struct WatopReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl WatopReader {
    const HEADER_SIZE: usize = 4864;
    const MAGIC: &'static [u8] = b"0TOPSystem W.A.Technology";

    pub fn new() -> Self {
        WatopReader {
            path: None,
            meta: None,
        }
    }

    fn read_i32_le(data: &[u8], offset: usize, label: &str) -> Result<i32> {
        let bytes = data.get(offset..offset + 4).ok_or_else(|| {
            BioFormatsError::UnsupportedFormat(format!("WA Technology TOP header missing {label}"))
        })?;
        Ok(i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }
}

impl Default for WatopReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for WatopReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(
            ext.as_deref(),
            Some("wat") | Some("wap") | Some("opo") | Some("opz") | Some("opt")
        )
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.starts_with(Self::MAGIC)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        if data.len() < Self::HEADER_SIZE {
            return Err(BioFormatsError::UnsupportedFormat(
                "WA Technology TOP file is shorter than the 4864-byte header".into(),
            ));
        }
        if !data.starts_with(Self::MAGIC) {
            return Err(BioFormatsError::UnsupportedFormat(
                "WA Technology TOP file is missing 0TOPSystem W.A.Technology magic".into(),
            ));
        }

        let width = Self::read_i32_le(&data, 251, "width")?;
        let height = Self::read_i32_le(&data, 255, "height")?;
        if width <= 0 || height <= 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "WA Technology TOP header contains invalid image dimensions".into(),
            ));
        }
        let width = width as u32;
        let height = height as u32;
        let expected = (Self::HEADER_SIZE as u64)
            .checked_add(width as u64 * height as u64 * 2)
            .ok_or_else(|| BioFormatsError::Format("WA Technology TOP size overflows".into()))?;
        let file_len = data.len() as u64;
        if file_len < expected {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "WA Technology TOP pixel payload is shorter than declared dimensions {width}x{height}"
            )));
        }

        let comment_bytes = data.get(49..82).unwrap_or(&[]);
        let comment = String::from_utf8_lossy(comment_bytes)
            .trim_end_matches('\0')
            .trim()
            .to_string();
        let mut series_metadata = HashMap::new();
        if !comment.is_empty() {
            series_metadata.insert(
                "Comment".to_string(),
                crate::common::metadata::MetadataValue::String(comment),
            );
        }
        if let Ok(x_size) = Self::read_i32_le(&data, 239, "x size") {
            series_metadata.insert(
                "X size (in um)".to_string(),
                crate::common::metadata::MetadataValue::Float(x_size as f64 / 100.0),
            );
        }
        if let Ok(y_size) = Self::read_i32_le(&data, 243, "y size") {
            series_metadata.insert(
                "Y size (in um)".to_string(),
                crate::common::metadata::MetadataValue::Float(y_size as f64 / 100.0),
            );
        }
        if let Ok(z_size) = Self::read_i32_le(&data, 247, "z size") {
            series_metadata.insert(
                "Z size (in um)".to_string(),
                crate::common::metadata::MetadataValue::Float(z_size as f64 / 100.0),
            );
        }

        self.path = Some(path.to_path_buf());
        self.meta = Some(ImageMetadata {
            size_x: width,
            size_y: height,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type: PixelType::Int16,
            bits_per_pixel: 16,
            image_count: 1,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: true,
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
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(Self::HEADER_SIZE as u64))
            .map_err(BioFormatsError::Io)?;
        let n_bytes = meta.size_x as usize * meta.size_y as usize * 2;
        let mut buf = vec![0; n_bytes];
        f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
        Ok(buf)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        _x: u32,
        _y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let meta = self
            .meta
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?
            .clone();
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let full = self.open_bytes(plane_index)?;
        crop_full_plane("WA Technology TOP", &full, &meta, 1, _x, _y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn resolution_count(&self) -> usize {
        1
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if level != 0 {
            Err(BioFormatsError::Format(format!(
                "resolution {} out of range",
                level
            )))
        } else {
            Ok(())
        }
    }
}

// ===========================================================================
// Raw binary reader — VG SAM
// ===========================================================================

/// VG SAM reader (`.dti`, plus legacy `.vgsam` alias).
///
/// Java Bio-Formats uses `VGS` magic, big-endian dimensions at offsets
/// 348/352, bytes-per-pixel at 360, and pixels at offset 368.
pub struct VgSamReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl VgSamReader {
    const PIXEL_OFFSET: usize = 368;
    const MAGIC: &'static [u8] = b"VGS";

    pub fn new() -> Self {
        VgSamReader {
            path: None,
            meta: None,
        }
    }

    fn read_i32_be(data: &[u8], offset: usize, label: &str) -> Result<i32> {
        let bytes = data.get(offset..offset + 4).ok_or_else(|| {
            BioFormatsError::UnsupportedFormat(format!("VG SAM header missing {label}"))
        })?;
        Ok(i32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn pixel_type_from_bpp(bytes_per_pixel: i32) -> Result<(PixelType, u8)> {
        match bytes_per_pixel {
            1 => Ok((PixelType::Uint8, 8)),
            2 => Ok((PixelType::Uint16, 16)),
            4 => Ok((PixelType::Float32, 32)),
            _ => Err(BioFormatsError::UnsupportedFormat(format!(
                "VG SAM unsupported bytes per pixel: {bytes_per_pixel}"
            ))),
        }
    }
}

impl Default for VgSamReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for VgSamReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("dti") | Some("vgsam"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.starts_with(Self::MAGIC)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        if data.len() < Self::PIXEL_OFFSET {
            return Err(BioFormatsError::UnsupportedFormat(
                "VG SAM file is shorter than the 368-byte header".into(),
            ));
        }
        if !data.starts_with(Self::MAGIC) {
            return Err(BioFormatsError::UnsupportedFormat(
                "VG SAM file is missing VGS magic".into(),
            ));
        }
        let width = Self::read_i32_be(&data, 348, "width")?;
        let height = Self::read_i32_be(&data, 352, "height")?;
        let bytes_per_pixel = Self::read_i32_be(&data, 360, "bytes per pixel")?;
        if width <= 0 || height <= 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "VG SAM header contains invalid image dimensions".into(),
            ));
        }
        let (pixel_type, bits_per_pixel) = Self::pixel_type_from_bpp(bytes_per_pixel)?;
        let width = width as u32;
        let height = height as u32;
        let expected = (Self::PIXEL_OFFSET as u64)
            .checked_add(width as u64 * height as u64 * bytes_per_pixel as u64)
            .ok_or_else(|| BioFormatsError::Format("VG SAM size overflows".into()))?;
        if (data.len() as u64) < expected {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "VG SAM pixel payload is shorter than declared dimensions {width}x{height}"
            )));
        }

        let mut series_metadata = HashMap::new();
        series_metadata.insert(
            "Bytes per pixel".into(),
            crate::common::metadata::MetadataValue::Int(bytes_per_pixel as i64),
        );
        self.path = Some(path.to_path_buf());
        self.meta = Some(ImageMetadata {
            size_x: width,
            size_y: height,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type,
            bits_per_pixel,
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
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(Self::PIXEL_OFFSET as u64))
            .map_err(BioFormatsError::Io)?;
        let mut buf =
            vec![
                0u8;
                meta.size_x as usize * meta.size_y as usize * meta.pixel_type.bytes_per_sample()
            ];
        f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
        Ok(buf)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        _x: u32,
        _y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let meta = self
            .meta
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?
            .clone();
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let full = self.open_bytes(plane_index)?;
        crop_full_plane("VG SAM", &full, &meta, 1, _x, _y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn resolution_count(&self) -> usize {
        1
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if level != 0 {
            Err(BioFormatsError::Format(format!(
                "resolution {} out of range",
                level
            )))
        } else {
            Ok(())
        }
    }
}

// ===========================================================================
// Raw binary reader — UBM Messtechnik
// ===========================================================================

/// UBM reader (`.pr3`, plus legacy `.ubm` alias).
///
/// Java Bio-Formats stores dimensions at offsets 44/48 in a 128-byte
/// little-endian header, followed by uint32 pixels with optional row padding.
pub struct UbmReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    padding_pixels: usize,
}

impl UbmReader {
    const HEADER_SIZE: usize = 128;

    pub fn new() -> Self {
        UbmReader {
            path: None,
            meta: None,
            padding_pixels: 0,
        }
    }

    fn read_i32_le(data: &[u8], offset: usize, label: &str) -> Result<i32> {
        let bytes = data.get(offset..offset + 4).ok_or_else(|| {
            BioFormatsError::UnsupportedFormat(format!("UBM header missing {label}"))
        })?;
        Ok(i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }
}

impl Default for UbmReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for UbmReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("pr3") | Some("ubm"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        if data.len() < Self::HEADER_SIZE {
            return Err(BioFormatsError::UnsupportedFormat(
                "UBM file is shorter than the 128-byte header".into(),
            ));
        }
        let width = Self::read_i32_le(&data, 44, "width")?;
        let height = Self::read_i32_le(&data, 48, "height")?;
        if width <= 0 || height <= 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "UBM header contains invalid image dimensions".into(),
            ));
        }
        let width = width as u32;
        let height = height as u32;
        let plane_bytes = width as u64 * height as u64 * 4;
        let min_len = Self::HEADER_SIZE as u64 + plane_bytes;
        if (data.len() as u64) < min_len {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "UBM pixel payload is shorter than declared dimensions {width}x{height}"
            )));
        }
        let extra = data.len() as u64 - min_len;
        let row_padding_bytes = extra
            .checked_div(height as u64)
            .ok_or_else(|| BioFormatsError::Format("UBM row padding overflows".into()))?;
        if row_padding_bytes % 4 != 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "UBM row padding is not aligned to uint32 pixels".into(),
            ));
        }
        let padding_pixels = (row_padding_bytes / 4) as usize;

        self.path = Some(path.to_path_buf());
        self.padding_pixels = padding_pixels;
        let mut series_metadata = HashMap::new();
        series_metadata.insert(
            "Padding pixels".to_string(),
            crate::common::metadata::MetadataValue::Int(padding_pixels as i64),
        );
        self.meta = Some(ImageMetadata {
            size_x: width,
            size_y: height,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type: PixelType::Uint32,
            bits_per_pixel: 32,
            image_count: 1,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: true,
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
        self.padding_pixels = 0;
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
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        self.open_bytes_region(plane_index, 0, 0, meta.size_x, meta.size_y)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        _x: u32,
        _y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        crate::common::region::validate_region("UBM", meta.size_x, meta.size_y, _x, _y, w, h)?;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
        let row_stride = (meta.size_x as usize + self.padding_pixels)
            .checked_mul(4)
            .ok_or_else(|| BioFormatsError::Format("UBM row stride overflows".into()))?;
        let out_row = (w as usize)
            .checked_mul(4)
            .ok_or_else(|| BioFormatsError::Format("UBM output row size overflows".into()))?;
        let mut out = Vec::with_capacity(out_row * h as usize);
        for row in 0..h as usize {
            let source_row = _y as usize + row;
            let offset =
                Self::HEADER_SIZE as u64 + source_row as u64 * row_stride as u64 + _x as u64 * 4;
            f.seek(SeekFrom::Start(offset))
                .map_err(BioFormatsError::Io)?;
            let start = out.len();
            out.resize(start + out_row, 0);
            f.read_exact(&mut out[start..start + out_row])
                .map_err(BioFormatsError::Io)?;
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

    fn resolution_count(&self) -> usize {
        1
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if level != 0 {
            Err(BioFormatsError::Format(format!(
                "resolution {} out of range",
                level
            )))
        } else {
            Ok(())
        }
    }
}

// ===========================================================================
// Raw binary reader — Seiko SPM
// ===========================================================================

/// Seiko SPM reader (`.xqd`, `.xqf`).
///
/// Java Bio-Formats stores dimensions at offset 1402 in a 2944-byte
/// little-endian header, followed by raw uint16 pixels.
pub struct SeikoReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl SeikoReader {
    const HEADER_SIZE: usize = 2944;

    pub fn new() -> Self {
        SeikoReader {
            path: None,
            meta: None,
        }
    }

    fn read_u16_le(data: &[u8], offset: usize, label: &str) -> Result<u16> {
        let bytes = data.get(offset..offset + 2).ok_or_else(|| {
            BioFormatsError::UnsupportedFormat(format!("Seiko SPM header missing {label}"))
        })?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn read_f32_le(data: &[u8], offset: usize) -> Option<f32> {
        let bytes = data.get(offset..offset + 4)?;
        Some(f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }
}

impl Default for SeikoReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for SeikoReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("xqd") | Some("xqf"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        if data.len() < Self::HEADER_SIZE {
            return Err(BioFormatsError::UnsupportedFormat(
                "Seiko SPM file is shorter than the 2944-byte header".into(),
            ));
        }
        let width = Self::read_u16_le(&data, 1402, "width")? as u32;
        let height = Self::read_u16_le(&data, 1404, "height")? as u32;
        if width == 0 || height == 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "Seiko SPM header contains invalid image dimensions".into(),
            ));
        }
        let expected = (Self::HEADER_SIZE as u64)
            .checked_add(width as u64 * height as u64 * 2)
            .ok_or_else(|| BioFormatsError::Format("Seiko SPM size overflows".into()))?;
        if (data.len() as u64) < expected {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "Seiko SPM pixel payload is shorter than declared dimensions {width}x{height}"
            )));
        }

        let mut series_metadata = HashMap::new();
        let comment_bytes = &data[40..data.len().min(156)];
        let nul = comment_bytes
            .iter()
            .position(|b| *b == 0)
            .unwrap_or(comment_bytes.len());
        let comment = String::from_utf8_lossy(&comment_bytes[..nul])
            .trim()
            .to_string();
        if !comment.is_empty() {
            series_metadata.insert(
                "Comment".into(),
                crate::common::metadata::MetadataValue::String(comment),
            );
        }
        if let Some(x_size) = Self::read_f32_le(&data, 156) {
            series_metadata.insert(
                "X size".into(),
                crate::common::metadata::MetadataValue::Float(x_size as f64),
            );
        }
        if let Some(y_size) = Self::read_f32_le(&data, 164) {
            series_metadata.insert(
                "Y size".into(),
                crate::common::metadata::MetadataValue::Float(y_size as f64),
            );
        }

        self.path = Some(path.to_path_buf());
        self.meta = Some(ImageMetadata {
            size_x: width,
            size_y: height,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type: PixelType::Uint16,
            bits_per_pixel: 16,
            image_count: 1,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: true,
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
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(Self::HEADER_SIZE as u64))
            .map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; meta.size_x as usize * meta.size_y as usize * 2];
        f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
        Ok(buf)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        _x: u32,
        _y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let meta = self
            .meta
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?
            .clone();
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let full = self.open_bytes(plane_index)?;
        crop_full_plane("Seiko SPM", &full, &meta, 1, _x, _y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn resolution_count(&self) -> usize {
        1
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if level != 0 {
            Err(BioFormatsError::Format(format!(
                "resolution {} out of range",
                level
            )))
        } else {
            Ok(())
        }
    }
}

// ===========================================================================
// Binary reader — PicoQuant Bin (.bin FLIM histogram cube)
// ===========================================================================

/// PicoQuant `.bin` FLIM reader.
///
/// Faithful port of Java Bio-Formats `PQBinReader`. The format holds a FLIM
/// data cube `(x, y, t)` where each pixel's decay (all `timeBins` values) is
/// stored contiguously. The 20-byte little-endian header is:
///   `sizeX:i32`, `sizeY:i32`, `pixResol:f32` (µm), `sizeT:i32`, then a
///   trailing `timeResol:f32` (ns) is read from the stream during init (the
///   `isThisType` check stops after `sizeT`). Pixels are `UINT32`.
///
/// Each output plane corresponds to a single time bin (an `(x, y)` slice).
pub struct PqBinReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    /// Number of time bins in the lifetime histogram (mirrors Java `timeBins`).
    time_bins: u32,
    /// Default number of bins kept in the load buffer (mirrors Java `blockLength`).
    block_length: u32,
}

impl PqBinReader {
    /// 20-byte little-endian header (mirrors Java `HEADER_SIZE`).
    const HEADER_SIZE: usize = 20;

    pub fn new() -> Self {
        PqBinReader {
            path: None,
            meta: None,
            time_bins: 0,
            block_length: 0,
        }
    }

    /// Mirrors Java `isThisType(RandomAccessInputStream)`.
    ///
    /// Reads the little-endian header and accepts the file only when
    /// `sizeX * sizeY * sizeT * 4 + HEADER_SIZE == fileLength`. This is strict
    /// enough to reject arbitrary `.bin` files (`suffixSufficient = false`).
    fn is_this_type(header: &[u8], file_length: u64) -> bool {
        const BPP: i64 = 4; // FormatTools.getBytesPerPixel(UINT32)
                            // Header: sizeX:i32, sizeY:i32, resolution:f32 (skipped), sizeT:i32.
        if header.len() < 16 {
            return false;
        }
        let size_x = i32::from_le_bytes([header[0], header[1], header[2], header[3]]);
        let size_y = i32::from_le_bytes([header[4], header[5], header[6], header[7]]);
        // header[8..12] is the time-axis resolution float (readFloat, value unused here)
        let size_t = i32::from_le_bytes([header[12], header[13], header[14], header[15]]);

        // Java multiplies as 32-bit ints; replicate the wrapping product so a
        // forged header cannot match via 64-bit promotion.
        let product = (size_x as i32)
            .wrapping_mul(size_y as i32)
            .wrapping_mul(size_t as i32)
            .wrapping_mul(BPP as i32);
        (product as i64 + Self::HEADER_SIZE as i64) == file_length as i64
    }

    /// Mirrors Java `initFile(String)`: parses the header and builds metadata.
    fn init_file(&mut self, path: &Path) -> Result<()> {
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        if data.len() < Self::HEADER_SIZE {
            return Err(BioFormatsError::UnsupportedFormat(
                "PicoQuant Bin file is shorter than the 20-byte header".into(),
            ));
        }

        // Header (little-endian).
        let size_x = i32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        let size_y = i32::from_le_bytes([data[4], data[5], data[6], data[7]]);
        let pix_resol = f32::from_le_bytes([data[8], data[9], data[10], data[11]]); // µm
        let size_t = i32::from_le_bytes([data[12], data[13], data[14], data[15]]);
        let time_resol = f32::from_le_bytes([data[16], data[17], data[18], data[19]]); // ns

        if size_x <= 0 || size_y <= 0 || size_t <= 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "PicoQuant Bin header contains invalid dimensions".into(),
            ));
        }
        let size_x = size_x as u32;
        let size_y = size_y as u32;
        let size_t = size_t as u32;

        // Verify the declared cube actually fits the file (Java relies on the
        // isThisType length check; we re-validate so open_bytes is bounded).
        let expected = (Self::HEADER_SIZE as u64)
            .checked_add(size_x as u64 * size_y as u64 * size_t as u64 * 4)
            .ok_or_else(|| BioFormatsError::Format("PicoQuant Bin size overflows".into()))?;
        if (data.len() as u64) < expected {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "PicoQuant Bin pixel payload is shorter than declared cube {size_x}x{size_y}x{size_t}"
            )));
        }

        self.time_bins = size_t;

        // moduloT: lifetime sub-dimension along T, step converted to ps.
        let step = (time_resol as f64) * 1000.0; // Convert to ps
        let modulo_t = crate::common::metadata::ModuloAnnotation {
            parent_dimension: "T".to_string(),
            modulo_type: "lifetime".to_string(),
            start: 0.0,
            step,
            end: step * (size_t as f64 - 1.0),
            unit: "ps".to_string(),
            labels: Vec::new(),
        };

        // blockLength selection (mirrors Java exactly).
        let size_threshold: u64 = 128 * 128 * 1024; // arbitrary buffer size limit
        let mut block_length: u32 = 2048; // default No of bins in buffer
        while (block_length as u64) * (size_x as u64) * (size_y as u64) > size_threshold {
            block_length /= 2;
        }
        if block_length > self.time_bins {
            block_length = self.time_bins;
        }
        self.block_length = block_length;

        let mut series_metadata = HashMap::new();
        series_metadata.insert(
            "Physical Size X (in um)".to_string(),
            MetadataValue::Float(pix_resol as f64),
        );
        series_metadata.insert(
            "Physical Size Y (in um)".to_string(),
            MetadataValue::Float(pix_resol as f64),
        );
        series_metadata.insert(
            "Time Resolution (in ns)".to_string(),
            MetadataValue::Float(time_resol as f64),
        );

        self.path = Some(path.to_path_buf());
        self.meta = Some(ImageMetadata {
            size_x,
            size_y,
            size_z: 1,
            size_c: 1,
            size_t,
            pixel_type: PixelType::Uint32,
            bits_per_pixel: 32,
            image_count: size_t,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: true,
            resolution_count: 1,
            thumbnail: false,
            series_metadata,
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: Some(modulo_t),
        });
        Ok(())
    }

    /// Reorder one time bin's `(x, y)` plane out of the contiguous-decay cube.
    ///
    /// Mirrors the data layout used by Java `openBytes`: for pixel `(col, row)`
    /// the decay of `timeBins` UINT32 values is stored contiguously, so plane
    /// `time_bin` at `(col, row)` sits at file byte offset
    /// `HEADER + ((row*sizeX + col)*timeBins + time_bin) * bpp`.
    fn plane_for_time_bin(&self, time_bin: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        const BPP: usize = 4;
        let size_x = meta.size_x as usize;
        let size_y = meta.size_y as usize;
        let time_bins = self.time_bins as usize;

        let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
        // Read the whole row of decays for each image row, then pick the bin.
        let row_decay_bytes = BPP * time_bins * size_x;
        let mut row_buf = vec![0u8; row_decay_bytes];
        let mut out = vec![0u8; size_x * size_y * BPP];

        f.seek(SeekFrom::Start(Self::HEADER_SIZE as u64))
            .map_err(BioFormatsError::Io)?;
        for row in 0..size_y {
            f.read_exact(&mut row_buf).map_err(BioFormatsError::Io)?;
            for col in 0..size_x {
                let output = (row * size_x + col) * BPP;
                let input = (col * time_bins + time_bin as usize) * BPP;
                out[output..output + BPP].copy_from_slice(&row_buf[input..input + BPP]);
            }
        }
        Ok(out)
    }
}

impl Default for PqBinReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for PqBinReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        // Java sets suffixSufficient = false, so name alone never suffices;
        // we still gate on the `.bin` extension for the name-based pre-filter.
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("bin"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        // The Java magic check needs the total file length, which the
        // header-only API cannot supply. Returning false here means detection
        // falls through to set_id, which performs the strict length check.
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        if !Self::is_this_type(&data, data.len() as u64) {
            return Err(BioFormatsError::UnsupportedFormat(
                "PicoQuant Bin header does not match sizeX*sizeY*sizeT*4 + 20 == file length"
                    .into(),
            ));
        }
        self.init_file(path)
    }

    fn close(&mut self) -> Result<()> {
        // init preLoading (mirrors Java close(fileOnly=false)).
        self.path = None;
        self.meta = None;
        self.time_bins = 0;
        self.block_length = 0;
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
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        // Plane index == time bin (mirrors Java `int timeBin = no;`).
        self.plane_for_time_bin(plane_index)
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
            .meta
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?
            .clone();
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let full = self.open_bytes(plane_index)?;
        crop_full_plane("PicoQuant Bin", &full, &meta, 1, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn resolution_count(&self) -> usize {
        1
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if level != 0 {
            Err(BioFormatsError::Format(format!(
                "resolution {} out of range",
                level
            )))
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod pqbin_tests {
    use super::*;

    /// Build a synthetic PicoQuant Bin cube to the Java magic/layout.
    ///
    /// Layout: header (sizeX:i32, sizeY:i32, pixResol:f32, sizeT:i32,
    /// timeResol:f32), then for each (row, col) a contiguous decay of
    /// `size_t` UINT32 values. Pixel value encodes (col, row, t) so plane
    /// reordering can be verified deterministically.
    fn synth_pqbin(size_x: u32, size_y: u32, size_t: u32, time_resol: f32) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(&(size_x as i32).to_le_bytes());
        data.extend_from_slice(&(size_y as i32).to_le_bytes());
        data.extend_from_slice(&(0.25f32).to_le_bytes()); // pixResol µm
        data.extend_from_slice(&(size_t as i32).to_le_bytes());
        data.extend_from_slice(&time_resol.to_le_bytes()); // timeResol ns
        for row in 0..size_y {
            for col in 0..size_x {
                for t in 0..size_t {
                    let v: u32 = (col << 20) | (row << 10) | t;
                    data.extend_from_slice(&v.to_le_bytes());
                }
            }
        }
        data
    }

    fn write_temp(name: &str, data: &[u8]) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "bioformats_pqbin_{}_{}.bin",
            std::process::id(),
            name
        ));
        std::fs::write(&path, data).unwrap();
        path
    }

    #[test]
    fn is_this_type_strict_length_check() {
        let good = synth_pqbin(3, 2, 4, 0.05);
        assert!(PqBinReader::is_this_type(&good, good.len() as u64));

        // Truncated cube must be rejected.
        let mut bad = good.clone();
        bad.truncate(bad.len() - 4);
        assert!(!PqBinReader::is_this_type(&bad, bad.len() as u64));

        // Arbitrary .bin garbage of the same length must be rejected (the
        // header dimensions won't satisfy the size equation).
        let garbage = vec![0xABu8; good.len()];
        assert!(!PqBinReader::is_this_type(&garbage, garbage.len() as u64));
    }

    #[test]
    fn set_id_and_metadata() {
        let data = synth_pqbin(5, 3, 7, 0.04);
        let path = write_temp("meta", &data);

        let mut reader = PqBinReader::new();
        reader.set_id(&path).unwrap();
        let meta = reader.metadata();
        assert_eq!(meta.size_x, 5);
        assert_eq!(meta.size_y, 3);
        assert_eq!(meta.size_z, 1);
        assert_eq!(meta.size_c, 1);
        assert_eq!(meta.size_t, 7);
        assert_eq!(meta.image_count, 7);
        assert_eq!(meta.pixel_type, PixelType::Uint32);
        assert!(meta.is_little_endian);
        assert_eq!(meta.dimension_order, DimensionOrder::XYZCT);

        let modulo = meta.modulo_t.as_ref().expect("moduloT present");
        assert_eq!(modulo.parent_dimension, "T");
        assert_eq!(modulo.unit, "ps");
        // step = timeResol(ns) * 1000 = 0.04 * 1000 = 40 ps
        assert!((modulo.step - 40.0).abs() < 1e-4);
        assert!((modulo.end - 40.0 * 6.0).abs() < 1e-3);

        reader.close().unwrap();
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn open_bytes_reorders_decay_cube() {
        let (sx, sy, st) = (5u32, 3u32, 7u32);
        let data = synth_pqbin(sx, sy, st, 0.04);
        let path = write_temp("planes", &data);

        let mut reader = PqBinReader::new();
        reader.set_id(&path).unwrap();

        for t in 0..st {
            let plane = reader.open_bytes(t).unwrap();
            assert_eq!(plane.len() as u32, sx * sy * 4);
            for row in 0..sy {
                for col in 0..sx {
                    let off = ((row * sx + col) * 4) as usize;
                    let got = u32::from_le_bytes([
                        plane[off],
                        plane[off + 1],
                        plane[off + 2],
                        plane[off + 3],
                    ]);
                    let expected = (col << 20) | (row << 10) | t;
                    assert_eq!(got, expected, "plane {t} pixel ({col},{row})");
                }
            }
        }

        // Region crop of plane 2 returns the requested sub-rectangle.
        let region = reader.open_bytes_region(2, 1, 1, 2, 2).unwrap();
        assert_eq!(region.len(), 2 * 2 * 4);
        let first = u32::from_le_bytes([region[0], region[1], region[2], region[3]]);
        assert_eq!(first, (1u32 << 20) | (1u32 << 10) | 2u32);

        // Out-of-range plane rejected.
        assert!(reader.open_bytes(st).is_err());

        reader.close().unwrap();
        std::fs::remove_file(&path).ok();
    }
}
