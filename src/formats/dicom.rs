//! DICOM format reader (medical imaging).
//!
//! Supports:
//! - Explicit VR Little Endian (most common, default)
//! - Implicit VR Little Endian (legacy)
//! - Unencapsulated (raw) pixel data
//! - JPEG 2000 encapsulated pixel data
//!
//! Does NOT support most compressed transfer syntaxes (JPEG baseline/lossless,
//! RLE, etc.).

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, LookupTable, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;

// ── VR codes that use 4-byte length (reserved 2 bytes + uint32) ──────────────
fn vr_has_long_length(vr: &[u8; 2]) -> bool {
    matches!(
        vr,
        b"OB" | b"OD" | b"OF" | b"OL" | b"OW" | b"SQ" | b"UC" | b"UN" | b"UR" | b"UT"
    )
}

fn is_valid_vr(vr: &[u8; 2]) -> bool {
    vr.iter().all(|b| b.is_ascii_uppercase())
}

// ── Read helpers ──────────────────────────────────────────────────────────────
fn read_u16_le(r: &mut impl Read) -> std::io::Result<u16> {
    let mut b = [0u8; 2];
    r.read_exact(&mut b)?;
    Ok(u16::from_le_bytes(b))
}
fn read_u32_le(r: &mut impl Read) -> std::io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}
fn read_u16_be(r: &mut impl Read) -> std::io::Result<u16> {
    let mut b = [0u8; 2];
    r.read_exact(&mut b)?;
    Ok(u16::from_be_bytes(b))
}
fn read_u32_be(r: &mut impl Read) -> std::io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_be_bytes(b))
}

// ── Collected attributes from parsing ────────────────────────────────────────
#[derive(Default)]
struct DicomAttrs {
    rows: u16,
    columns: u16,
    samples_per_pixel: u16,
    bits_allocated: u16,
    bits_stored: u16,
    pixel_representation: u16, // 0=unsigned, 1=signed
    number_of_frames: u32,
    photometric_interpretation: String,
    planar_configuration: u16,
    palette: PaletteLut,
    transfer_syntax: String,
    pixel_data_offset: u64,
    pixel_data_length: u64,
    encapsulated_frames: Vec<EncapsulatedFrame>,
    little_endian: bool,
    explicit_vr: bool,
    encapsulated: bool,
    extra: HashMap<String, String>,
}

#[derive(Clone, Default)]
struct EncapsulatedFrame {
    fragments: Vec<PixelFragment>,
}

#[derive(Clone)]
struct PixelFragment {
    offset: u64,
    length: u64,
}

#[derive(Clone, Default)]
struct PaletteLut {
    red: Option<LutChannel>,
    green: Option<LutChannel>,
    blue: Option<LutChannel>,
}

#[derive(Clone)]
struct LutChannel {
    entries: usize,
    first_mapped: i32,
    bits_per_entry: u16,
    data: Vec<u16>,
}

fn ascii_trim(v: &[u8]) -> String {
    std::str::from_utf8(v)
        .unwrap_or("")
        .trim_end_matches(['\0', ' '])
        .to_string()
}

fn read_u16_value(v: &[u8], little_endian: bool) -> u16 {
    if v.len() >= 2 {
        if little_endian {
            u16::from_le_bytes([v[0], v[1]])
        } else {
            u16::from_be_bytes([v[0], v[1]])
        }
    } else {
        0
    }
}

fn read_i16_value(v: &[u8], little_endian: bool) -> i16 {
    read_u16_value(v, little_endian) as i16
}

fn parse_lut_descriptor(value: &[u8], little_endian: bool) -> Option<(usize, i32, u16)> {
    if value.len() < 6 {
        return None;
    }
    let entries = read_u16_value(&value[0..2], little_endian);
    let first_mapped = read_i16_value(&value[2..4], little_endian) as i32;
    let bits_per_entry = read_u16_value(&value[4..6], little_endian);
    Some((
        if entries == 0 {
            65_536
        } else {
            entries as usize
        },
        first_mapped,
        bits_per_entry,
    ))
}

fn parse_lut_data(
    value: &[u8],
    entries: usize,
    bits_per_entry: u16,
    little_endian: bool,
) -> Vec<u16> {
    if bits_per_entry <= 8 && value.len() == entries {
        return value.iter().map(|&v| u16::from(v)).collect();
    }
    value
        .chunks_exact(2)
        .map(|chunk| {
            if little_endian {
                u16::from_le_bytes([chunk[0], chunk[1]])
            } else {
                u16::from_be_bytes([chunk[0], chunk[1]])
            }
        })
        .collect()
}

fn dicom_tag_info(group: u16, element: u16) -> Option<(&'static str, &'static str)> {
    Some(match (group, element) {
        (0x0002, 0x0010) => ("TransferSyntaxUID", "UI"),
        (0x0008, 0x0008) => ("ImageType", "CS"),
        (0x0008, 0x0016) => ("SOPClassUID", "UI"),
        (0x0008, 0x0018) => ("SOPInstanceUID", "UI"),
        (0x0008, 0x0020) => ("StudyDate", "DA"),
        (0x0008, 0x0021) => ("SeriesDate", "DA"),
        (0x0008, 0x0022) => ("AcquisitionDate", "DA"),
        (0x0008, 0x0023) => ("ContentDate", "DA"),
        (0x0008, 0x0030) => ("StudyTime", "TM"),
        (0x0008, 0x0031) => ("SeriesTime", "TM"),
        (0x0008, 0x0032) => ("AcquisitionTime", "TM"),
        (0x0008, 0x0033) => ("ContentTime", "TM"),
        (0x0008, 0x0050) => ("AccessionNumber", "SH"),
        (0x0008, 0x0060) => ("Modality", "CS"),
        (0x0008, 0x0070) => ("Manufacturer", "LO"),
        (0x0008, 0x0080) => ("InstitutionName", "LO"),
        (0x0008, 0x1030) => ("StudyDescription", "LO"),
        (0x0008, 0x103E) => ("SeriesDescription", "LO"),
        (0x0010, 0x0010) => ("PatientName", "PN"),
        (0x0010, 0x0020) => ("PatientID", "LO"),
        (0x0010, 0x0030) => ("PatientBirthDate", "DA"),
        (0x0010, 0x0040) => ("PatientSex", "CS"),
        (0x0018, 0x0050) => ("SliceThickness", "DS"),
        (0x0018, 0x0088) => ("SpacingBetweenSlices", "DS"),
        (0x0018, 0x5100) => ("PatientPosition", "CS"),
        (0x0020, 0x000D) => ("StudyInstanceUID", "UI"),
        (0x0020, 0x000E) => ("SeriesInstanceUID", "UI"),
        (0x0020, 0x0010) => ("StudyID", "SH"),
        (0x0020, 0x0011) => ("SeriesNumber", "IS"),
        (0x0020, 0x0013) => ("InstanceNumber", "IS"),
        (0x0020, 0x0032) => ("ImagePositionPatient", "DS"),
        (0x0020, 0x0037) => ("ImageOrientationPatient", "DS"),
        (0x0028, 0x0002) => ("SamplesPerPixel", "US"),
        (0x0028, 0x0004) => ("PhotometricInterpretation", "CS"),
        (0x0028, 0x0006) => ("PlanarConfiguration", "US"),
        (0x0028, 0x0008) => ("NumberOfFrames", "IS"),
        (0x0028, 0x0010) => ("Rows", "US"),
        (0x0028, 0x0011) => ("Columns", "US"),
        (0x0028, 0x0030) => ("PixelSpacing", "DS"),
        (0x0028, 0x0100) => ("BitsAllocated", "US"),
        (0x0028, 0x0101) => ("BitsStored", "US"),
        (0x0028, 0x0102) => ("HighBit", "US"),
        (0x0028, 0x0103) => ("PixelRepresentation", "US"),
        (0x0028, 0x1101) => ("RedPaletteColorLookupTableDescriptor", "US"),
        (0x0028, 0x1102) => ("GreenPaletteColorLookupTableDescriptor", "US"),
        (0x0028, 0x1103) => ("BluePaletteColorLookupTableDescriptor", "US"),
        (0x0028, 0x1201) => ("RedPaletteColorLookupTableData", "OW"),
        (0x0028, 0x1202) => ("GreenPaletteColorLookupTableData", "OW"),
        (0x0028, 0x1203) => ("BluePaletteColorLookupTableData", "OW"),
        _ => return None,
    })
}

fn decode_numeric_values<T, F>(
    value: &[u8],
    width: usize,
    little_endian: bool,
    mut decode: F,
) -> Option<String>
where
    T: std::fmt::Display,
    F: FnMut(&[u8], bool) -> T,
{
    if value.len() < width || !value.len().is_multiple_of(width) {
        return None;
    }
    Some(
        value
            .chunks_exact(width)
            .map(|chunk| decode(chunk, little_endian).to_string())
            .collect::<Vec<_>>()
            .join("\\"),
    )
}

fn decode_dicom_metadata_value(
    vr: &[u8; 2],
    group: u16,
    element: u16,
    value: &[u8],
    little_endian: bool,
) -> Option<String> {
    let effective_vr = if vr == b"??" {
        dicom_tag_info(group, element)?.1.as_bytes()
    } else {
        vr
    };

    match effective_vr {
        b"AE" | b"AS" | b"CS" | b"DA" | b"DS" | b"DT" | b"IS" | b"LO" | b"LT" | b"PN" | b"SH"
        | b"ST" | b"TM" | b"UC" | b"UI" | b"UR" | b"UT" => Some(ascii_trim(value)),
        b"US" => decode_numeric_values(value, 2, little_endian, |chunk, le| {
            if le {
                u16::from_le_bytes([chunk[0], chunk[1]])
            } else {
                u16::from_be_bytes([chunk[0], chunk[1]])
            }
        }),
        b"SS" => decode_numeric_values(value, 2, little_endian, |chunk, le| {
            if le {
                i16::from_le_bytes([chunk[0], chunk[1]])
            } else {
                i16::from_be_bytes([chunk[0], chunk[1]])
            }
        }),
        b"UL" => decode_numeric_values(value, 4, little_endian, |chunk, le| {
            if le {
                u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]])
            } else {
                u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]])
            }
        }),
        b"SL" => decode_numeric_values(value, 4, little_endian, |chunk, le| {
            if le {
                i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]])
            } else {
                i32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]])
            }
        }),
        b"FL" => decode_numeric_values(value, 4, little_endian, |chunk, le| {
            if le {
                f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]])
            } else {
                f32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]])
            }
        }),
        b"FD" => decode_numeric_values(value, 8, little_endian, |chunk, le| {
            if le {
                f64::from_le_bytes([
                    chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
                ])
            } else {
                f64::from_be_bytes([
                    chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
                ])
            }
        }),
        _ => None,
    }
}

fn store_dicom_metadata(
    attrs: &mut DicomAttrs,
    vr: &[u8; 2],
    group: u16,
    element: u16,
    value: &[u8],
) {
    let Some(decoded) = decode_dicom_metadata_value(vr, group, element, value, attrs.little_endian)
    else {
        return;
    };
    let key = format!("({:04X},{:04X})", group, element);
    attrs.extra.insert(key, decoded.clone());
    if let Some((name, _)) = dicom_tag_info(group, element) {
        attrs.extra.insert(name.to_string(), decoded);
    }
}

fn read_tag(r: &mut impl Read, little_endian: bool) -> std::io::Result<(u16, u16)> {
    let group = if little_endian {
        read_u16_le(r)?
    } else {
        read_u16_be(r)?
    };
    let element = if little_endian {
        read_u16_le(r)?
    } else {
        read_u16_be(r)?
    };
    Ok((group, element))
}

fn read_u32(r: &mut impl Read, little_endian: bool) -> std::io::Result<u32> {
    if little_endian {
        read_u32_le(r)
    } else {
        read_u32_be(r)
    }
}

fn read_element_length_after_tag(
    r: &mut impl Read,
    explicit_vr: bool,
    little_endian: bool,
) -> std::io::Result<([u8; 2], u64)> {
    if explicit_vr {
        let mut vr = [0u8; 2];
        r.read_exact(&mut vr)?;
        let length = if vr_has_long_length(&vr) {
            let mut reserved = [0u8; 2];
            r.read_exact(&mut reserved)?;
            read_u32(r, little_endian)? as u64
        } else if little_endian {
            read_u16_le(r)? as u64
        } else {
            read_u16_be(r)? as u64
        };
        Ok((vr, length))
    } else {
        Ok(([b'?', b'?'], read_u32(r, little_endian)? as u64))
    }
}

fn skip_value(
    r: &mut (impl Read + Seek),
    length: u64,
    explicit_vr: bool,
    little_endian: bool,
) -> Result<()> {
    if length == 0xFFFF_FFFF {
        skip_undefined_length_sequence(r, little_endian, explicit_vr)
    } else {
        r.seek(SeekFrom::Current(length as i64))
            .map_err(BioFormatsError::Io)?;
        Ok(())
    }
}

fn skip_undefined_length_item(
    r: &mut (impl Read + Seek),
    little_endian: bool,
    explicit_vr: bool,
) -> Result<()> {
    loop {
        let (group, element) = match read_tag(r, little_endian) {
            Ok(tag) => tag,
            Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(err) => return Err(BioFormatsError::Io(err)),
        };

        match (group, element) {
            (0xFFFE, 0xE00D) | (0xFFFE, 0xE0DD) => {
                let _length = read_u32(r, little_endian).map_err(BioFormatsError::Io)?;
                return Ok(());
            }
            (0xFFFE, 0xE000) => {
                let length = read_u32(r, little_endian).map_err(BioFormatsError::Io)? as u64;
                if length == 0xFFFF_FFFF {
                    skip_undefined_length_item(r, little_endian, explicit_vr)?;
                } else {
                    r.seek(SeekFrom::Current(length as i64))
                        .map_err(BioFormatsError::Io)?;
                }
            }
            _ => {
                let (_vr, length) = read_element_length_after_tag(r, explicit_vr, little_endian)
                    .map_err(BioFormatsError::Io)?;
                skip_value(r, length, explicit_vr, little_endian)?;
            }
        }
    }
}

fn skip_undefined_length_sequence(
    r: &mut (impl Read + Seek),
    little_endian: bool,
    explicit_vr: bool,
) -> Result<()> {
    loop {
        let (group, element) = match read_tag(r, little_endian) {
            Ok(tag) => tag,
            Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(err) => return Err(BioFormatsError::Io(err)),
        };

        match (group, element) {
            (0xFFFE, 0xE0DD) => {
                let _length = read_u32(r, little_endian).map_err(BioFormatsError::Io)?;
                return Ok(());
            }
            (0xFFFE, 0xE000) => {
                let length = read_u32(r, little_endian).map_err(BioFormatsError::Io)? as u64;
                if length == 0xFFFF_FFFF {
                    skip_undefined_length_item(r, little_endian, explicit_vr)?;
                } else {
                    r.seek(SeekFrom::Current(length as i64))
                        .map_err(BioFormatsError::Io)?;
                }
            }
            (0xFFFE, 0xE00D) => {
                let _length = read_u32(r, little_endian).map_err(BioFormatsError::Io)?;
                return Ok(());
            }
            _ => {
                let (_vr, length) = read_element_length_after_tag(r, explicit_vr, little_endian)
                    .map_err(BioFormatsError::Io)?;
                skip_value(r, length, explicit_vr, little_endian)?;
            }
        }
    }
}

fn parse_basic_offset_table(value: &[u8]) -> Vec<u32> {
    value
        .chunks_exact(4)
        .map(|chunk| u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect()
}

fn frames_from_fragments(
    fragments: &[(u64, PixelFragment)],
    offsets: &[u32],
    number_of_frames: u32,
) -> Vec<EncapsulatedFrame> {
    if fragments.is_empty() {
        return Vec::new();
    }
    if offsets.len() > 1 {
        let first_item = fragments[0].0;
        let mut frames = Vec::with_capacity(offsets.len());
        for (index, start) in offsets.iter().enumerate() {
            let end = offsets.get(index + 1).copied().map(u64::from);
            let start = u64::from(*start);
            let frame_fragments = fragments
                .iter()
                .filter_map(|(item_start, fragment)| {
                    let rel = item_start.saturating_sub(first_item);
                    if rel >= start && end.is_none_or(|end| rel < end) {
                        Some(fragment.clone())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>();
            frames.push(EncapsulatedFrame {
                fragments: frame_fragments,
            });
        }
        return frames;
    }

    if number_of_frames as usize == fragments.len() {
        return fragments
            .iter()
            .map(|(_, fragment)| EncapsulatedFrame {
                fragments: vec![fragment.clone()],
            })
            .collect();
    }

    vec![EncapsulatedFrame {
        fragments: fragments
            .iter()
            .map(|(_, fragment)| fragment.clone())
            .collect(),
    }]
}

fn parse_encapsulated_pixel_data(
    r: &mut (impl Read + Seek),
    number_of_frames: u32,
) -> Result<Vec<EncapsulatedFrame>> {
    let mut basic_offsets = Vec::new();
    let mut fragments = Vec::new();
    let mut saw_basic_offset_table = false;

    loop {
        let item_start = r.stream_position().map_err(BioFormatsError::Io)?;
        let (group, element) = match read_tag(r, true) {
            Ok(tag) => tag,
            Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(err) => return Err(BioFormatsError::Io(err)),
        };
        let length = read_u32_le(r).map_err(BioFormatsError::Io)? as u64;

        match (group, element) {
            (0xFFFE, 0xE0DD) => break,
            (0xFFFE, 0xE000) => {
                let value_offset = r.stream_position().map_err(BioFormatsError::Io)?;
                if !saw_basic_offset_table {
                    let mut value = vec![0u8; length as usize];
                    r.read_exact(&mut value).map_err(BioFormatsError::Io)?;
                    basic_offsets = parse_basic_offset_table(&value);
                    saw_basic_offset_table = true;
                } else {
                    fragments.push((
                        item_start,
                        PixelFragment {
                            offset: value_offset,
                            length,
                        },
                    ));
                    r.seek(SeekFrom::Current(length as i64))
                        .map_err(BioFormatsError::Io)?;
                }
            }
            _ => break,
        }
    }

    Ok(frames_from_fragments(
        &fragments,
        &basic_offsets,
        number_of_frames,
    ))
}

fn parse_dicom(path: &Path) -> Result<DicomAttrs> {
    let f = File::open(path).map_err(BioFormatsError::Io)?;
    let mut r = BufReader::new(f);

    let mut attrs = DicomAttrs {
        little_endian: true,
        explicit_vr: true,
        ..Default::default()
    };

    // DICOM Part 10 files have a 128-byte preamble followed by "DICM".
    // Raw datasets may legally start at the first data element instead.
    let mut preamble = [0u8; 132];
    let n = r.read(&mut preamble).map_err(BioFormatsError::Io)?;
    let dataset_start;
    if n >= 132 && &preamble[128..132] == b"DICM" {
        r.seek(SeekFrom::Start(132)).map_err(BioFormatsError::Io)?;
        dataset_start = 132;
    } else {
        r.seek(SeekFrom::Start(0)).map_err(BioFormatsError::Io)?;
        dataset_start = 0;
    }

    // ── Phase 1: Parse meta file information (group 0002) ───────────────────
    // Group 0002 is ALWAYS Explicit VR Little Endian
    loop {
        let pos = r.stream_position().map_err(BioFormatsError::Io)?;
        let group = match read_u16_le(&mut r) {
            Ok(g) => g,
            Err(_) => break,
        };
        let element = read_u16_le(&mut r).map_err(BioFormatsError::Io)?;

        if group != 0x0002 {
            // Rewind and parse rest with detected transfer syntax
            r.seek(SeekFrom::Start(pos)).map_err(BioFormatsError::Io)?;
            break;
        }

        // Explicit VR
        let mut vr = [0u8; 2];
        r.read_exact(&mut vr).map_err(BioFormatsError::Io)?;
        let length = if vr_has_long_length(&vr) {
            let mut reserved = [0u8; 2];
            r.read_exact(&mut reserved).map_err(BioFormatsError::Io)?;
            read_u32_le(&mut r).map_err(BioFormatsError::Io)? as u64
        } else {
            read_u16_le(&mut r).map_err(BioFormatsError::Io)? as u64
        };

        let mut value = vec![0u8; length as usize];
        r.read_exact(&mut value).map_err(BioFormatsError::Io)?;

        if group == 0x0002 && element == 0x0010 {
            // Transfer Syntax UID
            attrs.transfer_syntax = ascii_trim(&value);
        }
    }

    // Determine VR mode and endianness from transfer syntax
    match attrs.transfer_syntax.trim_end_matches('\0') {
        "1.2.840.10008.1.2" => {
            // Implicit VR Little Endian
            attrs.explicit_vr = false;
            attrs.little_endian = true;
        }
        "1.2.840.10008.1.2.2" => {
            // Explicit VR Big Endian (deprecated)
            attrs.explicit_vr = true;
            attrs.little_endian = false;
        }
        _ => {
            // Default: Explicit VR Little Endian (1.2.840.10008.1.2.1 or unknown)
            attrs.explicit_vr = true;
            attrs.little_endian = true;
        }
    }

    let mut palette_descriptors: [Option<(usize, i32, u16)>; 3] = [None, None, None];
    let mut palette_data: [Option<Vec<u16>>; 3] = [None, None, None];

    // ── Phase 2: Parse remaining data elements ──────────────────────────────
    loop {
        let pos = r.stream_position().map_err(BioFormatsError::Io)?;
        let (group, element) = match read_tag(&mut r, attrs.little_endian) {
            Ok(tag) => tag,
            Err(_) => break,
        };

        // Detect delimiter tags
        if group == 0xFFFE && (element == 0xE000 || element == 0xE00D || element == 0xE0DD) {
            // Item / Item Delimitation / Sequence Delimitation
            let _len = read_u32_le(&mut r).map_err(BioFormatsError::Io)?;
            continue;
        }

        let (vr, length) = if attrs.explicit_vr {
            let mut vr = [0u8; 2];
            r.read_exact(&mut vr).map_err(BioFormatsError::Io)?;
            if !is_valid_vr(&vr) && attrs.transfer_syntax.is_empty() && pos == dataset_start {
                attrs.explicit_vr = false;
                attrs.little_endian = true;
                r.seek(SeekFrom::Start(pos)).map_err(BioFormatsError::Io)?;
                continue;
            }
            let length = if vr_has_long_length(&vr) {
                let mut reserved = [0u8; 2];
                r.read_exact(&mut reserved).map_err(BioFormatsError::Io)?;
                if attrs.little_endian {
                    read_u32_le(&mut r).map_err(BioFormatsError::Io)? as u64
                } else {
                    read_u32_be(&mut r).map_err(BioFormatsError::Io)? as u64
                }
            } else if attrs.little_endian {
                read_u16_le(&mut r).map_err(BioFormatsError::Io)? as u64
            } else {
                read_u16_be(&mut r).map_err(BioFormatsError::Io)? as u64
            };
            (vr, length)
        } else {
            // Implicit VR: just 4-byte length
            let length = if attrs.little_endian {
                read_u32_le(&mut r).map_err(BioFormatsError::Io)? as u64
            } else {
                read_u32_be(&mut r).map_err(BioFormatsError::Io)? as u64
            };
            ([b'?', b'?'], length)
        };

        // Undefined length (0xFFFFFFFF) — only safe to handle for pixel data
        if length == 0xFFFFFFFF {
            if group == 0x7FE0 && element == 0x0010 {
                // Encapsulated pixel data: Basic Offset Table followed by fragments.
                attrs.pixel_data_offset = r.stream_position().map_err(BioFormatsError::Io)?;
                attrs.pixel_data_length = 0;
                attrs.encapsulated = true;
                attrs.encapsulated_frames =
                    parse_encapsulated_pixel_data(&mut r, attrs.number_of_frames)?;
                break;
            } else {
                skip_undefined_length_sequence(&mut r, attrs.little_endian, attrs.explicit_vr)?;
                continue;
            }
        }

        // Pixel data: record offset and length, stop parsing
        if group == 0x7FE0 && element == 0x0010 {
            attrs.pixel_data_offset = r.stream_position().map_err(BioFormatsError::Io)?;
            attrs.pixel_data_length = length;
            break;
        }

        // Read value bytes for other elements
        let value_start = r.stream_position().map_err(BioFormatsError::Io)?;
        let mut value = vec![0u8; length as usize];
        r.read_exact(&mut value).map_err(BioFormatsError::Io)?;
        store_dicom_metadata(&mut attrs, &vr, group, element, &value);

        // Decode key imaging tags
        let read_u16 = |v: &[u8]| -> u16 { read_u16_value(v, attrs.little_endian) };
        let _read_u32_val = |v: &[u8]| -> u32 {
            if v.len() >= 4 {
                if attrs.little_endian {
                    u32::from_le_bytes([v[0], v[1], v[2], v[3]])
                } else {
                    u32::from_be_bytes([v[0], v[1], v[2], v[3]])
                }
            } else {
                0
            }
        };

        match (group, element) {
            (0x0028, 0x0008) => {
                // Number of Frames (IS string)
                let s = ascii_trim(&value);
                attrs.number_of_frames = s.trim().parse().unwrap_or(1);
            }
            (0x0028, 0x0004) => attrs.photometric_interpretation = ascii_trim(&value),
            (0x0028, 0x0010) => attrs.rows = read_u16(&value),
            (0x0028, 0x0011) => attrs.columns = read_u16(&value),
            (0x0028, 0x0002) => attrs.samples_per_pixel = read_u16(&value),
            (0x0028, 0x0006) => attrs.planar_configuration = read_u16(&value),
            (0x0028, 0x0100) => attrs.bits_allocated = read_u16(&value),
            (0x0028, 0x0101) => attrs.bits_stored = read_u16(&value),
            (0x0028, 0x0103) => attrs.pixel_representation = read_u16(&value),
            (0x0028, 0x1101) => {
                palette_descriptors[0] = parse_lut_descriptor(&value, attrs.little_endian)
            }
            (0x0028, 0x1102) => {
                palette_descriptors[1] = parse_lut_descriptor(&value, attrs.little_endian)
            }
            (0x0028, 0x1103) => {
                palette_descriptors[2] = parse_lut_descriptor(&value, attrs.little_endian)
            }
            (0x0028, 0x1201) => {
                let (entries, _, bits) = palette_descriptors[0].unwrap_or((value.len() / 2, 0, 16));
                palette_data[0] = Some(parse_lut_data(&value, entries, bits, attrs.little_endian));
            }
            (0x0028, 0x1202) => {
                let (entries, _, bits) = palette_descriptors[1].unwrap_or((value.len() / 2, 0, 16));
                palette_data[1] = Some(parse_lut_data(&value, entries, bits, attrs.little_endian));
            }
            (0x0028, 0x1203) => {
                let (entries, _, bits) = palette_descriptors[2].unwrap_or((value.len() / 2, 0, 16));
                palette_data[2] = Some(parse_lut_data(&value, entries, bits, attrs.little_endian));
            }
            _ => {}
        }
        let _ = (pos, value_start);
    }

    if attrs.number_of_frames == 0 {
        attrs.number_of_frames = 1;
    }
    if attrs.samples_per_pixel == 0 {
        attrs.samples_per_pixel = 1;
    }
    if attrs.samples_per_pixel == 1 {
        attrs.planar_configuration = 0;
    }
    let make_channel = |index: usize| -> Option<LutChannel> {
        let (entries, first_mapped, bits_per_entry) = palette_descriptors[index]?;
        let data = palette_data[index].clone()?;
        Some(LutChannel {
            entries,
            first_mapped,
            bits_per_entry,
            data,
        })
    };
    attrs.palette = PaletteLut {
        red: make_channel(0),
        green: make_channel(1),
        blue: make_channel(2),
    };

    Ok(attrs)
}

fn build_metadata(a: &DicomAttrs) -> Result<ImageMetadata> {
    if a.rows == 0 || a.columns == 0 {
        return Err(BioFormatsError::Format(
            "DICOM: missing image dimensions".into(),
        ));
    }
    let has_palette =
        a.palette.red.is_some() && a.palette.green.is_some() && a.palette.blue.is_some();
    let palette_bits = a
        .palette
        .red
        .as_ref()
        .map(|lut| lut.bits_per_entry)
        .unwrap_or(0);
    let pixel_type = if has_palette {
        if palette_bits <= 8 {
            PixelType::Uint8
        } else {
            PixelType::Uint16
        }
    } else {
        match (a.bits_allocated, a.pixel_representation) {
            (1, _) => PixelType::Uint8,
            (2..=8, _) => PixelType::Uint8,
            (9..=16, 0) => PixelType::Uint16,
            (9..=16, 1) => PixelType::Int16,
            (32, 0) => PixelType::Uint32,
            (32, 1) => PixelType::Int32,
            _ => PixelType::Uint16,
        }
    };
    let source_bits = if a.bits_stored == 0 {
        a.bits_allocated
    } else {
        a.bits_stored
    };
    let bits_per_pixel = if has_palette {
        palette_bits.clamp(8, 16) as u8
    } else {
        source_bits.clamp(1, 32) as u8
    };

    let photometric = a.photometric_interpretation.trim();
    let is_rgb = matches!(photometric, "RGB" | "YBR_FULL" | "YBR_FULL_422")
        || has_palette
        || (photometric.is_empty() && a.samples_per_pixel == 3);
    let image_count = a.number_of_frames;
    let size_c = if has_palette {
        3
    } else {
        a.samples_per_pixel as u32
    };

    let mut meta = ImageMetadata {
        size_x: a.columns as u32,
        size_y: a.rows as u32,
        size_z: image_count,
        size_c,
        size_t: 1,
        pixel_type,
        bits_per_pixel,
        image_count,
        dimension_order: DimensionOrder::XYCZT,
        is_rgb,
        is_interleaved: true,
        is_indexed: false,
        is_little_endian: a.little_endian,
        resolution_count: 1,
        series_metadata: a
            .extra
            .iter()
            .map(|(k, v)| (k.clone(), MetadataValue::String(v.clone())))
            .collect(),
        lookup_table: palette_lookup_table(&a.palette),
        modulo_z: None,
        modulo_c: None,
        modulo_t: None,
    };

    if !a.transfer_syntax.is_empty() {
        meta.series_metadata.insert(
            "TransferSyntaxUID".into(),
            MetadataValue::String(a.transfer_syntax.clone()),
        );
    }
    if !a.photometric_interpretation.is_empty() {
        meta.series_metadata.insert(
            "PhotometricInterpretation".into(),
            MetadataValue::String(a.photometric_interpretation.clone()),
        );
    }
    if a.samples_per_pixel > 1 {
        meta.series_metadata.insert(
            "PlanarConfiguration".into(),
            MetadataValue::String(a.planar_configuration.to_string()),
        );
    }

    Ok(meta)
}

fn source_pixel_bytes(meta: &ImageMetadata, samples: u16, bits_allocated: u16) -> Result<usize> {
    let pixels = (meta.size_x as usize)
        .checked_mul(meta.size_y as usize)
        .and_then(|v| v.checked_mul(samples as usize))
        .ok_or_else(|| BioFormatsError::Format("DICOM: image dimensions overflow".into()))?;
    let bits = pixels
        .checked_mul(bits_allocated.max(1) as usize)
        .ok_or_else(|| BioFormatsError::Format("DICOM: pixel byte count overflow".into()))?;
    Ok(bits.div_ceil(8))
}

fn validate_pixel_data_length(
    meta: &ImageMetadata,
    pixel_data_length: u64,
    samples: u16,
    bits_allocated: u16,
) -> Result<()> {
    let plane_bytes = source_pixel_bytes(meta, samples, bits_allocated)?;
    let expected = (plane_bytes as u64)
        .checked_mul(meta.image_count as u64)
        .ok_or_else(|| BioFormatsError::Format("DICOM: pixel byte count overflow".into()))?;
    let allowed_padding = u64::from(expected % 2 == 1);
    if pixel_data_length < expected {
        return Err(BioFormatsError::Format(format!(
            "DICOM: pixel data is shorter than expected ({pixel_data_length} < {expected})"
        )));
    }
    if pixel_data_length > expected + allowed_padding {
        return Err(BioFormatsError::Format(format!(
            "DICOM: pixel data length does not match frame stride ({pixel_data_length} > {})",
            expected + allowed_padding
        )));
    }
    Ok(())
}

fn palette_lookup_table(palette: &PaletteLut) -> Option<LookupTable> {
    Some(LookupTable {
        red: palette.red.as_ref()?.data.clone(),
        green: palette.green.as_ref()?.data.clone(),
        blue: palette.blue.as_ref()?.data.clone(),
    })
}

fn lut_value(lut: &LutChannel, index: u16) -> u16 {
    let offset = i32::from(index) - lut.first_mapped;
    if offset <= 0 {
        return lut.data.first().copied().unwrap_or(0);
    }
    let offset = (offset as usize).min(lut.entries.saturating_sub(1));
    lut.data
        .get(offset)
        .copied()
        .or_else(|| lut.data.last().copied())
        .unwrap_or(0)
}

fn lut_output_value(value: u16, bits_per_entry: u16) -> u16 {
    if bits_per_entry <= 8 {
        value & 0x00ff
    } else {
        value
    }
}

fn unpack_bit_samples(src: &[u8], samples: usize, bits: u16) -> Vec<u16> {
    let bits = bits as usize;
    let mut out = Vec::with_capacity(samples);
    let mut bit_offset = 0usize;
    for _ in 0..samples {
        let mut value = 0u16;
        for bit in 0..bits {
            let byte = src.get(bit_offset / 8).copied().unwrap_or(0);
            value |= u16::from((byte >> (bit_offset % 8)) & 1) << bit;
            bit_offset += 1;
        }
        out.push(value);
    }
    out
}

fn normalize_native_pixels(
    src: &[u8],
    meta: &ImageMetadata,
    samples: u16,
    bits_allocated: u16,
    bits_stored: u16,
    pixel_representation: u16,
    palette: &PaletteLut,
) -> Vec<u8> {
    let sample_count = meta.size_x as usize * meta.size_y as usize * samples as usize;
    let stored_bits = bits_stored.max(1).min(bits_allocated.max(1));
    let mask = if stored_bits >= 16 {
        u16::MAX
    } else {
        (1u16 << stored_bits) - 1
    };
    let values: Vec<u16> = if bits_allocated < 8 || bits_allocated % 8 != 0 {
        unpack_bit_samples(src, sample_count, bits_allocated.max(1))
    } else if bits_allocated <= 8 {
        src.iter()
            .take(sample_count)
            .map(|&v| u16::from(v) & mask)
            .collect()
    } else {
        src.chunks_exact(2)
            .take(sample_count)
            .map(|chunk| {
                let raw = if meta.is_little_endian {
                    u16::from_le_bytes([chunk[0], chunk[1]])
                } else {
                    u16::from_be_bytes([chunk[0], chunk[1]])
                } & mask;
                if pixel_representation == 1
                    && stored_bits < 16
                    && (raw & (1u16 << (stored_bits - 1))) != 0
                {
                    raw | !mask
                } else {
                    raw
                }
            })
            .collect()
    };

    if let (Some(red), Some(green), Some(blue)) = (&palette.red, &palette.green, &palette.blue) {
        let bytes_per_sample = meta.pixel_type.bytes_per_sample();
        let mut out = Vec::with_capacity(values.len() * 3 * bytes_per_sample);
        for index in values {
            for (lut, value) in [
                (red, lut_value(red, index)),
                (green, lut_value(green, index)),
                (blue, lut_value(blue, index)),
            ] {
                let value = lut_output_value(value, lut.bits_per_entry);
                if bytes_per_sample == 1 {
                    out.push(value as u8);
                } else if meta.is_little_endian {
                    out.extend_from_slice(&value.to_le_bytes());
                } else {
                    out.extend_from_slice(&value.to_be_bytes());
                }
            }
        }
        return out;
    }

    if meta.pixel_type.bytes_per_sample() == 1 {
        values.into_iter().map(|v| v as u8).collect()
    } else {
        let mut out = Vec::with_capacity(values.len() * 2);
        for value in values {
            if meta.is_little_endian {
                out.extend_from_slice(&value.to_le_bytes());
            } else {
                out.extend_from_slice(&value.to_be_bytes());
            }
        }
        out
    }
}

fn invert_monochrome1(buf: &mut [u8], meta: &ImageMetadata) {
    let max_value = match meta.bits_per_pixel {
        0 => return,
        1..=7 => (1u32 << meta.bits_per_pixel) - 1,
        8 => u8::MAX as u32,
        9..=15 => (1u32 << meta.bits_per_pixel) - 1,
        16 => u16::MAX as u32,
        _ => return,
    };

    match meta.pixel_type.bytes_per_sample() {
        1 => {
            let max = max_value as u8;
            for b in buf {
                *b = max.saturating_sub(*b);
            }
        }
        2 => {
            for px in buf.chunks_exact_mut(2) {
                let value = if meta.is_little_endian {
                    u16::from_le_bytes([px[0], px[1]])
                } else {
                    u16::from_be_bytes([px[0], px[1]])
                };
                let inverted = (max_value as u16).saturating_sub(value);
                let bytes = if meta.is_little_endian {
                    inverted.to_le_bytes()
                } else {
                    inverted.to_be_bytes()
                };
                px.copy_from_slice(&bytes);
            }
        }
        _ => {}
    }
}

fn planar_to_interleaved(buf: &[u8], meta: &ImageMetadata) -> Vec<u8> {
    let samples = meta.size_c as usize;
    let sample_bytes = meta.pixel_type.bytes_per_sample();
    let pixels_per_plane = meta.size_x as usize * meta.size_y as usize;
    let channel_stride = pixels_per_plane * sample_bytes;
    let mut out = vec![0u8; buf.len()];

    for pixel in 0..pixels_per_plane {
        for channel in 0..samples {
            let src = channel * channel_stride + pixel * sample_bytes;
            let dst = (pixel * samples + channel) * sample_bytes;
            out[dst..dst + sample_bytes].copy_from_slice(&buf[src..src + sample_bytes]);
        }
    }
    out
}

fn is_jpeg2000_transfer_syntax(uid: &str) -> bool {
    matches!(
        uid.trim_end_matches('\0').trim(),
        "1.2.840.10008.1.2.4.90" | "1.2.840.10008.1.2.4.91"
    )
}

fn expected_output_bytes(meta: &ImageMetadata) -> Result<usize> {
    (meta.size_x as usize)
        .checked_mul(meta.size_y as usize)
        .and_then(|v| v.checked_mul(meta.size_c as usize))
        .and_then(|v| v.checked_mul(meta.pixel_type.bytes_per_sample()))
        .ok_or_else(|| BioFormatsError::Format("DICOM: pixel byte count overflow".into()))
}

// ── Reader ────────────────────────────────────────────────────────────────────

pub struct DicomReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    pixel_data_offset: u64,
    pixel_data_length: u64,
    encapsulated_frames: Vec<EncapsulatedFrame>,
    is_little_endian: bool,
    encapsulated: bool,
    transfer_syntax: String,
    photometric_interpretation: String,
    planar_configuration: u16,
    source_samples_per_pixel: u16,
    bits_allocated: u16,
    bits_stored: u16,
    pixel_representation: u16,
    palette: PaletteLut,
}

impl DicomReader {
    pub fn new() -> Self {
        DicomReader {
            path: None,
            meta: None,
            pixel_data_offset: 0,
            pixel_data_length: 0,
            encapsulated_frames: Vec::new(),
            is_little_endian: true,
            encapsulated: false,
            transfer_syntax: String::new(),
            photometric_interpretation: String::new(),
            planar_configuration: 0,
            source_samples_per_pixel: 1,
            bits_allocated: 8,
            bits_stored: 8,
            pixel_representation: 0,
            palette: PaletteLut::default(),
        }
    }
}

impl Default for DicomReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for DicomReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("dcm") | Some("dicom") | Some("dic"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.len() >= 132 && &header[128..132] == b"DICM"
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let attrs = parse_dicom(path)?;
        let meta = build_metadata(&attrs)?;
        if !attrs.encapsulated {
            validate_pixel_data_length(
                &meta,
                attrs.pixel_data_length,
                attrs.samples_per_pixel,
                attrs.bits_allocated,
            )?;
        }
        self.meta = Some(meta);
        self.pixel_data_offset = attrs.pixel_data_offset;
        self.pixel_data_length = attrs.pixel_data_length;
        self.encapsulated_frames = attrs.encapsulated_frames;
        self.is_little_endian = attrs.little_endian;
        self.encapsulated = attrs.encapsulated;
        self.transfer_syntax = attrs.transfer_syntax;
        self.photometric_interpretation = attrs.photometric_interpretation;
        self.planar_configuration = attrs.planar_configuration;
        self.source_samples_per_pixel = attrs.samples_per_pixel;
        self.bits_allocated = attrs.bits_allocated;
        self.bits_stored = attrs.bits_stored;
        self.pixel_representation = attrs.pixel_representation;
        self.palette = attrs.palette;
        self.path = Some(path.to_path_buf());
        Ok(())
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

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;

        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        if self.encapsulated {
            if !is_jpeg2000_transfer_syntax(&self.transfer_syntax) {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "DICOM: encapsulated transfer syntax {} is not supported",
                    self.transfer_syntax
                )));
            }
            let frame = self
                .encapsulated_frames
                .get(plane_index as usize)
                .or_else(|| {
                    if plane_index == 0 {
                        self.encapsulated_frames.first()
                    } else {
                        None
                    }
                })
                .ok_or_else(|| BioFormatsError::Format("DICOM: missing pixel fragments".into()))?;
            let mut f = File::open(path).map_err(BioFormatsError::Io)?;
            let mut encoded = Vec::new();
            for fragment in &frame.fragments {
                f.seek(SeekFrom::Start(fragment.offset))
                    .map_err(BioFormatsError::Io)?;
                let start = encoded.len();
                encoded.resize(start + fragment.length as usize, 0);
                f.read_exact(&mut encoded[start..])
                    .map_err(BioFormatsError::Io)?;
            }
            let decoded = crate::common::codec::decompress_jpeg2000(&encoded)?;
            let expected = expected_output_bytes(meta)?;
            if decoded.len() != expected {
                return Err(BioFormatsError::Codec(format!(
                    "DICOM JPEG 2000 decoded {} bytes, expected {expected}",
                    decoded.len()
                )));
            }
            return Ok(decoded);
        }

        let source_plane_bytes =
            source_pixel_bytes(meta, self.source_samples_per_pixel, self.bits_allocated)?;
        let plane_offset = self.pixel_data_offset + plane_index as u64 * source_plane_bytes as u64;

        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(plane_offset))
            .map_err(BioFormatsError::Io)?;
        let mut source = vec![0u8; source_plane_bytes];
        f.read_exact(&mut source).map_err(BioFormatsError::Io)?;
        let mut buf = normalize_native_pixels(
            &source,
            meta,
            self.source_samples_per_pixel,
            self.bits_allocated,
            self.bits_stored,
            self.pixel_representation,
            &self.palette,
        );

        if self.planar_configuration == 1 && meta.size_c > 1 {
            buf = planar_to_interleaved(&buf, meta);
        }
        if self.photometric_interpretation.trim() == "MONOCHROME1" {
            invert_monochrome1(&mut buf, meta);
        }

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
        let full = self.open_bytes(plane_index)?;
        let meta = self.meta.as_ref().unwrap();
        let spp = meta.size_c as usize;
        let bps = meta.pixel_type.bytes_per_sample();
        let row_bytes = meta.size_x as usize * spp * bps;
        let out_row = w as usize * spp * bps;
        let mut out = Vec::with_capacity(h as usize * out_row);
        for row in 0..h as usize {
            let src = &full[(y as usize + row) * row_bytes..];
            let s = x as usize * spp * bps;
            out.extend_from_slice(&src[s..s + out_row]);
        }
        Ok(out)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = ((meta.size_x - tw) / 2, (meta.size_y - th) / 2);
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        use crate::common::metadata::MetadataValue;
        use crate::common::ome_metadata::OmeMetadata;
        let meta = self.meta.as_ref()?;
        let mut ome = OmeMetadata::from_image_metadata(meta);
        let img = &mut ome.images[0];
        // DICOM tag (0028,0030) PixelSpacing: "row_spacing\col_spacing" in mm
        if let Some(MetadataValue::String(s)) = meta.series_metadata.get("(0028,0030)") {
            let parts: Vec<&str> = s.splitn(2, |c| c == '\\' || c == '/').collect();
            if let (Some(row), Some(col)) = (
                parts.first().and_then(|v| v.trim().parse::<f64>().ok()),
                parts.get(1).and_then(|v| v.trim().parse::<f64>().ok()),
            ) {
                // PixelSpacing is in mm → convert to µm
                img.physical_size_x = Some(col * 1000.0);
                img.physical_size_y = Some(row * 1000.0);
            }
        }
        // DICOM tag (0018,0050) SliceThickness in mm
        if let Some(MetadataValue::String(s)) = meta.series_metadata.get("(0018,0050)") {
            img.physical_size_z = s.trim().parse::<f64>().ok().map(|v| v * 1000.0);
        }
        // PatientName / StudyDescription as image name
        if let Some(MetadataValue::String(s)) = meta.series_metadata.get("(0010,0010)") {
            img.name = Some(s.clone());
        }
        Some(ome)
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// DICOM Writer — Secondary Capture
// ═══════════════════════════════════════════════════════════════════════════════

use std::io::{BufWriter, Write};

/// DICOM Secondary Capture writer.
///
/// Produces valid DICOM files with Explicit VR Little Endian transfer syntax.
/// Generates minimal UIDs for patient/study/series/instance.
pub struct DicomWriter {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    planes: Vec<Vec<u8>>,
}

impl DicomWriter {
    pub fn new() -> Self {
        DicomWriter {
            path: None,
            meta: None,
            planes: Vec::new(),
        }
    }
}

impl Default for DicomWriter {
    fn default() -> Self {
        Self::new()
    }
}

/// Write an Explicit VR LE data element.
fn write_elem(
    w: &mut impl Write,
    group: u16,
    elem: u16,
    vr: &[u8; 2],
    data: &[u8],
) -> std::io::Result<()> {
    w.write_all(&group.to_le_bytes())?;
    w.write_all(&elem.to_le_bytes())?;
    w.write_all(vr)?;
    if vr_has_long_length(vr) {
        w.write_all(&[0u8; 2])?; // reserved
        w.write_all(&(data.len() as u32).to_le_bytes())?;
    } else {
        w.write_all(&(data.len() as u16).to_le_bytes())?;
    }
    w.write_all(data)?;
    // Pad odd-length values
    if data.len() % 2 != 0 {
        w.write_all(&[0x20])?; // space padding for strings
    }
    Ok(())
}

fn write_elem_str(
    w: &mut impl Write,
    group: u16,
    elem: u16,
    vr: &[u8; 2],
    s: &str,
) -> std::io::Result<()> {
    let mut data = s.as_bytes().to_vec();
    if data.len() % 2 != 0 {
        data.push(0x20);
    } // pad to even
    write_elem(w, group, elem, vr, &data)
}

fn write_elem_u16(w: &mut impl Write, group: u16, elem: u16, v: u16) -> std::io::Result<()> {
    write_elem(w, group, elem, b"US", &v.to_le_bytes())
}

/// Generate a simple UID based on timestamp + counter.
fn generate_uid(suffix: u32) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    // Root: 1.2.826.0.1 (dummy OID prefix for generated UIDs)
    format!("1.2.826.0.1.{ts}.{suffix}")
}

impl crate::common::writer::FormatWriter for DicomWriter {
    fn is_this_type(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("dcm") | Some("dicom"))
    }

    fn set_metadata(&mut self, meta: &ImageMetadata) -> Result<()> {
        self.meta = Some(meta.clone());
        self.planes.clear();
        Ok(())
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn save_bytes(&mut self, _plane_index: u32, data: &[u8]) -> Result<()> {
        self.planes.push(data.to_vec());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        let meta = self.meta.take().ok_or(BioFormatsError::NotInitialized)?;
        let path = self.path.take().ok_or(BioFormatsError::NotInitialized)?;

        let f = File::create(&path).map_err(BioFormatsError::Io)?;
        let mut w = BufWriter::new(f);

        // 128-byte preamble + DICM magic
        w.write_all(&[0u8; 128]).map_err(BioFormatsError::Io)?;
        w.write_all(b"DICM").map_err(BioFormatsError::Io)?;

        let uid_study = generate_uid(1);
        let uid_series = generate_uid(2);
        let uid_instance = generate_uid(3);
        let uid_sop_class = "1.2.840.10008.5.1.4.1.1.7"; // Secondary Capture

        // File Meta Information (group 0002)
        // First write meta elements to a buffer to compute group length
        let mut meta_buf: Vec<u8> = Vec::new();
        write_elem(&mut meta_buf, 0x0002, 0x0001, b"OB", &[0x00, 0x01]).unwrap(); // FileMetaVersion
        write_elem_str(&mut meta_buf, 0x0002, 0x0002, b"UI", uid_sop_class).unwrap(); // MediaStorageSOPClassUID
        write_elem_str(&mut meta_buf, 0x0002, 0x0003, b"UI", &uid_instance).unwrap(); // MediaStorageSOPInstanceUID
        write_elem_str(&mut meta_buf, 0x0002, 0x0010, b"UI", "1.2.840.10008.1.2.1").unwrap(); // TransferSyntax = Explicit VR LE
        write_elem_str(&mut meta_buf, 0x0002, 0x0012, b"UI", "1.2.826.0.1").unwrap(); // ImplementationClassUID

        // Group length element
        write_elem(
            &mut w,
            0x0002,
            0x0000,
            b"UL",
            &(meta_buf.len() as u32).to_le_bytes(),
        )
        .map_err(BioFormatsError::Io)?;
        w.write_all(&meta_buf).map_err(BioFormatsError::Io)?;

        // Patient module
        write_elem_str(&mut w, 0x0010, 0x0010, b"PN", "Anonymous").map_err(BioFormatsError::Io)?;
        write_elem_str(&mut w, 0x0010, 0x0020, b"LO", "0").map_err(BioFormatsError::Io)?;

        // Study module
        write_elem_str(&mut w, 0x0020, 0x000D, b"UI", &uid_study).map_err(BioFormatsError::Io)?;
        write_elem_str(&mut w, 0x0020, 0x0010, b"SH", "1").map_err(BioFormatsError::Io)?;

        // Series module
        write_elem_str(&mut w, 0x0020, 0x000E, b"UI", &uid_series).map_err(BioFormatsError::Io)?;
        write_elem_str(&mut w, 0x0020, 0x0011, b"IS", "1").map_err(BioFormatsError::Io)?;

        // SOP Common
        write_elem_str(&mut w, 0x0008, 0x0016, b"UI", uid_sop_class)
            .map_err(BioFormatsError::Io)?;
        write_elem_str(&mut w, 0x0008, 0x0018, b"UI", &uid_instance)
            .map_err(BioFormatsError::Io)?;

        // Image module
        let bps = meta.bits_per_pixel as u16;
        let spp = if meta.is_rgb { meta.size_c as u16 } else { 1 };
        let photometric = if meta.is_rgb { "RGB" } else { "MONOCHROME2" };
        let pixel_rep: u16 = match meta.pixel_type {
            PixelType::Int8 | PixelType::Int16 | PixelType::Int32 => 1,
            _ => 0,
        };

        write_elem_u16(&mut w, 0x0028, 0x0002, spp).map_err(BioFormatsError::Io)?; // SamplesPerPixel
        write_elem_str(&mut w, 0x0028, 0x0004, b"CS", photometric).map_err(BioFormatsError::Io)?;
        write_elem_u16(&mut w, 0x0028, 0x0010, meta.size_y as u16).map_err(BioFormatsError::Io)?; // Rows
        write_elem_u16(&mut w, 0x0028, 0x0011, meta.size_x as u16).map_err(BioFormatsError::Io)?; // Columns
        write_elem_u16(&mut w, 0x0028, 0x0100, bps).map_err(BioFormatsError::Io)?; // BitsAllocated
        write_elem_u16(&mut w, 0x0028, 0x0101, bps).map_err(BioFormatsError::Io)?; // BitsStored
        write_elem_u16(&mut w, 0x0028, 0x0102, bps - 1).map_err(BioFormatsError::Io)?; // HighBit
        write_elem_u16(&mut w, 0x0028, 0x0103, pixel_rep).map_err(BioFormatsError::Io)?; // PixelRepresentation

        if self.planes.len() > 1 {
            write_elem_str(
                &mut w,
                0x0028,
                0x0008,
                b"IS",
                &self.planes.len().to_string(),
            )
            .map_err(BioFormatsError::Io)?; // NumberOfFrames
        }

        // Pixel Data (7FE0,0010)
        let total_bytes: usize = self.planes.iter().map(|p| p.len()).sum();
        w.write_all(&0x7FE0u16.to_le_bytes())
            .map_err(BioFormatsError::Io)?;
        w.write_all(&0x0010u16.to_le_bytes())
            .map_err(BioFormatsError::Io)?;
        w.write_all(b"OW").map_err(BioFormatsError::Io)?;
        w.write_all(&[0u8; 2]).map_err(BioFormatsError::Io)?; // reserved
        w.write_all(&(total_bytes as u32).to_le_bytes())
            .map_err(BioFormatsError::Io)?;
        for plane in &self.planes {
            w.write_all(plane).map_err(BioFormatsError::Io)?;
        }

        w.flush().map_err(BioFormatsError::Io)?;
        self.planes.clear();
        Ok(())
    }

    fn can_do_stacks(&self) -> bool {
        true
    }
}
