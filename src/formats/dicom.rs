//! DICOM format reader (medical imaging).
//!
//! Supports:
//! - Explicit VR Little Endian (most common, default)
//! - Implicit VR Little Endian (legacy)
//! - Unencapsulated (raw) pixel data
//! - JPEG 2000 encapsulated pixel data
//! - JPEG baseline / lossless encapsulated pixel data (via the shared JPEG
//!   decoder)
//! - RLE (run-length encoding, PS3.5 Annex G) encapsulated pixel data
//!
//! Does NOT support Deflated Explicit VR Little Endian transfer syntax.

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
    /// Window Width (0028,1051); -1 when absent/empty (DicomReader.maxPixelRange).
    max_pixel_range: i32,
    /// Window Center (0028,1050); -1 when absent/empty (DicomReader.centerPixelValue).
    center_pixel_value: i32,
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
        (0x0028, 0x1050) => ("WindowCenter", "DS"),
        (0x0028, 0x1051) => ("WindowWidth", "DS"),
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
    // File length is used to bound element-value allocations: a corrupt or
    // malicious `length` field must never trigger a multi-GB allocation. The
    // Java reader streams/skips through the file, so an element value can never
    // exceed the bytes physically present.
    let file_len = f.metadata().map_err(BioFormatsError::Io)?.len();
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

        // Guard against an implausible length (corrupt header): the value
        // cannot extend past the end of the file.
        let cur = r.stream_position().map_err(BioFormatsError::Io)?;
        if length > file_len.saturating_sub(cur) {
            return Err(BioFormatsError::InvalidData(
                "DICOM meta element length exceeds file size".into(),
            ));
        }
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
        // Guard against an implausible length (corrupt element): the value
        // cannot extend past the end of the file. This prevents a huge
        // `vec![0u8; length]` allocation on malformed input.
        if length > file_len.saturating_sub(value_start) {
            return Err(BioFormatsError::InvalidData(
                "DICOM element length exceeds file size".into(),
            ));
        }
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
            (0x0028, 0x1050) => {
                // Window Center (DS). DicomReader.centerPixelValue: -1 when empty.
                let s = ascii_trim(&value);
                let first = s.split('\\').next().unwrap_or("").trim();
                attrs.center_pixel_value = if first.is_empty() {
                    -1
                } else {
                    first.parse::<f64>().map(|f| f as i32).unwrap_or(-1)
                };
            }
            (0x0028, 0x1051) => {
                // Window Width (DS). DicomReader.maxPixelRange: -1 when empty.
                let s = ascii_trim(&value);
                let first = s.split('\\').next().unwrap_or("").trim();
                attrs.max_pixel_range = if first.is_empty() {
                    -1
                } else {
                    first.parse::<f64>().map(|f| f as i32).unwrap_or(-1)
                };
            }
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

// ── Multi-series companion-file grouping (DicomReader.makeFileList / ──────────
//    scanDirectory / addFileToList) ────────────────────────────────────────────
//
// The Java DicomReader, when file grouping is enabled (the default), scans the
// directory of the selected file and groups together every DICOM file that
// belongs to the same study/series. Each resulting group is exposed as a
// separate series. Files are grouped by Series Number (0020,0011); a candidate
// is admitted to the group only when it shares the original file's
// Acquisition Date (0008,0022), its Series Number matches, the leading
// components of the SOP Instance UID (0008,0018) match (all but the trailing
// two), the specimen matches, and the Acquisition Time (0008,0032) is within
// 150 s. Files whose pixel dimensions differ from the original land in a
// separate (incremented) series, mirroring Java's resolution split.

/// Grouping key extracted from a parsed DICOM file, mirroring the fields read
/// by `DicomReader.addFileToList`.
#[derive(Clone, Default)]
struct DicomGroupKey {
    date: Option<String>,
    time: Option<String>,
    instance: Option<i64>,
    series: i32,
    instance_uid: Option<String>,
    rows: u16,
    columns: u16,
}

fn first_value(s: &str) -> String {
    s.split('\\').next().unwrap_or("").trim().to_string()
}

fn group_key_from_attrs(a: &DicomAttrs) -> DicomGroupKey {
    let get = |tag: &str| a.extra.get(tag).map(|v| first_value(v));
    let date = get("AcquisitionDate").filter(|s| !s.is_empty());
    let time = get("AcquisitionTime").filter(|s| !s.is_empty());
    let instance = get("InstanceNumber")
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse::<f64>().ok())
        .map(|f| f as i64);
    let series = get("SeriesNumber")
        .and_then(|s| s.parse::<f64>().ok())
        .map(|f| f as i32)
        .unwrap_or(0);
    let instance_uid = get("SOPInstanceUID").filter(|s| !s.is_empty());
    DicomGroupKey {
        date,
        time,
        instance,
        series,
        instance_uid,
        rows: a.rows,
        columns: a.columns,
    }
}

/// Convert a DICOM TM value (HHMMSS.FFFFFF, optionally with ':' separators or a
/// +/- timezone) to microseconds. Mirrors DicomReader.getTimestampMicroseconds.
fn timestamp_microseconds(v: Option<&str>) -> i128 {
    let Some(v) = v else { return 0 };
    let mut v = v.trim().replace(':', "");
    if let Some(p) = v.find('+') {
        v.truncate(p);
    }
    if let Some(p) = v.find('-') {
        v.truncate(p);
    }
    if v.is_empty() {
        return 0;
    }
    let digits: String = v.chars().take_while(|c| c.is_ascii_digit()).collect();
    let hours = digits.get(0..2).and_then(|s| s.parse::<i128>().ok()).unwrap_or(0);
    let mut total = hours * 60 * 60;
    if let Some(m) = digits.get(2..4).and_then(|s| s.parse::<i128>().ok()) {
        total += m * 60;
    }
    if let Some(s) = digits.get(4..6).and_then(|s| s.parse::<i128>().ok()) {
        total += s;
    }
    total *= 1_000_000;
    if let Some(dot) = v.find('.') {
        if let Ok(frac) = v[dot + 1..].parse::<i128>() {
            total += frac;
        }
    }
    total
}

/// UID prefix match: all but the trailing two dot-separated components must be
/// equal (DicomReader.addFileToList).
fn instance_uid_prefix_matches(original: &Option<String>, candidate: &Option<String>) -> bool {
    match (original, candidate) {
        (Some(o), Some(c)) => {
            let ou: Vec<&str> = o.split('.').collect();
            let cu: Vec<&str> = c.split('.').collect();
            let count = ou.len().min(cu.len()).saturating_sub(2);
            (0..count).all(|i| ou[i] == cu[i])
        }
        (None, None) => true,
        // Exactly one UID present → not a match.
        _ => false,
    }
}

/// Decide whether `candidate` belongs in the same dataset as `original`, and if
/// so which series index it lands in. Returns `None` when the file should not be
/// grouped. Mirrors the admission test in DicomReader.addFileToList.
fn grouped_series(original: &DicomGroupKey, candidate: &DicomGroupKey) -> Option<i32> {
    // Must have date/time/instance, same series number, and matching UID prefix.
    if candidate.date.is_none() || candidate.time.is_none() || candidate.instance.is_none() {
        return None;
    }
    if candidate.series != original.series {
        return None;
    }
    if !instance_uid_prefix_matches(&original.instance_uid, &candidate.instance_uid) {
        return None;
    }

    let mut file_series = candidate.series;
    // Differing dimensions → separate (resolution) series.
    if candidate.columns != original.columns || candidate.rows != original.rows {
        file_series += 1;
    }

    let stamp = timestamp_microseconds(candidate.time.as_deref());
    let timestamp = timestamp_microseconds(original.time.as_deref());
    let time_difference = (stamp - timestamp).abs();

    if candidate.date == original.date && time_difference < 150_000_000 {
        Some(file_series)
    } else {
        None
    }
}

/// Scan the directory of `path` and build the series→files grouping, following
/// DicomReader.makeFileList. The selected file always anchors its own series
/// (keyed by its Series Number). Returns a map series-number → ordered file
/// list, where each file is placed at its (InstanceNumber - 1) position.
fn build_dicom_file_list(
    path: &Path,
    original: &DicomGroupKey,
) -> std::collections::BTreeMap<i32, Vec<PathBuf>> {
    use std::collections::BTreeMap;

    let mut file_list: BTreeMap<i32, Vec<Option<PathBuf>>> = BTreeMap::new();

    let abs = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());

    // Seed the original file at its instance position within its own series.
    let instance_number = original.instance.unwrap_or(1).max(1) - 1;
    let series_files = file_list.entry(original.series).or_default();
    if instance_number == 0 {
        series_files.push(Some(abs.clone()));
    } else {
        while (instance_number as usize) > series_files.len() {
            series_files.push(None);
        }
        series_files.push(Some(abs.clone()));
    }

    if let Some(dir) = path.parent() {
        if let Ok(entries) = std::fs::read_dir(dir) {
            let mut files: Vec<PathBuf> = entries
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.is_file())
                .collect();
            files.sort();

            let reader = DicomReader::new();
            for file in files {
                let file_abs =
                    std::fs::canonicalize(&file).unwrap_or_else(|_| file.clone());
                if file_abs == abs {
                    continue;
                }
                // Must look like DICOM.
                let header = match read_dicom_probe_header(&file) {
                    Some(h) => h,
                    None => continue,
                };
                if !reader.is_this_type_by_bytes(&header) {
                    continue;
                }
                let attrs = match parse_dicom(&file) {
                    Ok(a) => a,
                    Err(_) => continue,
                };
                let candidate = group_key_from_attrs(&attrs);
                let Some(series) = grouped_series(original, &candidate) else {
                    continue;
                };

                let position = (candidate.instance.unwrap_or(1).max(1) - 1).max(0) as usize;
                let bucket = file_list.entry(series).or_default();
                if position < bucket.len() {
                    let mut pos = position;
                    while pos < bucket.len() && bucket[pos].is_some() {
                        pos += 1;
                    }
                    if pos < bucket.len() {
                        bucket[pos] = Some(file_abs.clone());
                    } else if !bucket.iter().any(|f| f.as_deref() == Some(file_abs.as_path())) {
                        bucket.push(Some(file_abs.clone()));
                    }
                } else if !bucket.iter().any(|f| f.as_deref() == Some(file_abs.as_path())) {
                    while position > bucket.len() {
                        bucket.push(None);
                    }
                    bucket.push(Some(file_abs.clone()));
                }
            }
        }
    }

    // Drop the null placeholders (DicomReader.makeFileList removes them).
    file_list
        .into_iter()
        .map(|(series, files)| (series, files.into_iter().flatten().collect::<Vec<_>>()))
        .filter(|(_, files)| !files.is_empty())
        .collect()
}

/// Read the first 132 bytes of a candidate file for the DICM magic probe.
fn read_dicom_probe_header(path: &Path) -> Option<Vec<u8>> {
    let mut f = File::open(path).ok()?;
    let mut buf = vec![0u8; 132];
    let n = f.read(&mut buf).ok()?;
    buf.truncate(n);
    Some(buf)
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

/// Maximum sample value for a pixel type, matching FormatTools.defaultMinMax()[1].
fn default_max_value(meta: &ImageMetadata) -> i64 {
    match meta.pixel_type {
        PixelType::Int8 => i8::MAX as i64,
        PixelType::Uint8 | PixelType::Bit => u8::MAX as i64,
        PixelType::Int16 => i16::MAX as i64,
        PixelType::Uint16 => u16::MAX as i64,
        PixelType::Int32 => i32::MAX as i64,
        PixelType::Uint32 => u32::MAX as i64,
        // Floating types: Bio-Formats uses the corresponding int range.
        PixelType::Float32 => i32::MAX as i64,
        PixelType::Float64 => i64::MAX,
    }
}

/// Invert MONOCHROME1 pixels (white→0 stored, so subtract from the observed/
/// windowed maximum), following DicomReader.openBytes.
fn invert_monochrome1(
    buf: &mut [u8],
    meta: &ImageMetadata,
    max_pixel_range: i32,
    center_pixel_value: i32,
) {
    match meta.pixel_type.bytes_per_sample() {
        1 => {
            // Java: buf[i] = (byte) (255 - buf[i]).
            for b in buf {
                *b = 255u8.wrapping_sub(*b);
            }
        }
        2 => {
            // Java:
            //   maxPixelValue = maxPixelRange + (centerPixelValue / 2);
            //   if (maxPixelRange == -1 || centerPixelValue < (maxPixelRange/2))
            //     maxPixelValue = defaultMinMax(pixelType)[1];
            let mut max_pixel_value =
                max_pixel_range as i64 + (center_pixel_value as i64) / 2;
            if max_pixel_range == -1 || (center_pixel_value as i64) < (max_pixel_range as i64) / 2
            {
                max_pixel_value = default_max_value(meta);
            }
            for px in buf.chunks_exact_mut(2) {
                let value = if meta.is_little_endian {
                    u16::from_le_bytes([px[0], px[1]]) as i64
                } else {
                    u16::from_be_bytes([px[0], px[1]]) as i64
                };
                let inverted = (max_pixel_value - value) as u16;
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

/// Transfer-syntax classification mirroring DicomReader.java:
///   isJP2K   = uid.startsWith("1.2.840.10008.1.2.4.9")
///   isJPEG   = !isJP2K && uid.startsWith("1.2.840.10008.1.2.4")
///   isRLE    = uid.startsWith("1.2.840.10008.1.2.5")
///   isDeflate= uid.startsWith("1.2.8.10008.1.2.1.99")
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum EncapsulatedSyntax {
    Jpeg2000,
    Jpeg,
    Rle,
    Deflate,
    Unknown,
}

fn classify_transfer_syntax(uid: &str) -> EncapsulatedSyntax {
    let uid = uid.trim_end_matches('\0').trim();
    if uid.starts_with("1.2.840.10008.1.2.4.9") {
        EncapsulatedSyntax::Jpeg2000
    } else if uid.starts_with("1.2.840.10008.1.2.4") {
        EncapsulatedSyntax::Jpeg
    } else if uid.starts_with("1.2.840.10008.1.2.5") {
        EncapsulatedSyntax::Rle
    } else if uid.starts_with("1.2.8.10008.1.2.1.99") {
        EncapsulatedSyntax::Deflate
    } else {
        EncapsulatedSyntax::Unknown
    }
}

/// Decodes DICOM RLE (PS3.5 Annex G) into a native interleaved pixel buffer.
///
/// Mirrors `DicomReader.readTile` for the RLE branch. The fragment begins with a
/// 64-byte header of 16 little-endian uint32s: `[0]` = segment count, `[1..]` =
/// byte offsets (from the fragment start) of each segment. Each segment is
/// PackBits-encoded. Segments are ordered by sample, then by byte plane from
/// most-significant to least-significant. We decode each segment, then
/// reassemble samples in little-endian order (matching Java's
/// `byteIndex = bpp - j - 1` for little-endian output) and interleave channels
/// (RGBRGB...) to match the layout the native pixel pipeline expects.
fn decode_dicom_rle(
    data: &[u8],
    width: usize,
    height: usize,
    ec: usize,
    bpp: usize,
) -> Result<Vec<u8>> {
    if data.len() < 64 {
        return Err(BioFormatsError::Format(
            "DICOM RLE: fragment shorter than 64-byte header".into(),
        ));
    }
    let plane = width
        .checked_mul(height)
        .ok_or_else(|| BioFormatsError::Format("DICOM RLE: dimensions overflow".into()))?;

    let num_segments = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
    if num_segments == 0 || num_segments > 15 {
        return Err(BioFormatsError::Format(format!(
            "DICOM RLE: invalid segment count {num_segments}"
        )));
    }

    // Read segment offsets and derive per-segment lengths.
    let mut offsets = Vec::with_capacity(num_segments + 1);
    for s in 0..num_segments {
        let o = 4 + s * 4;
        let off = u32::from_le_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]]) as usize;
        offsets.push(off);
    }
    offsets.push(data.len());

    // Decode each segment via PackBits.
    let mut segments: Vec<Vec<u8>> = Vec::with_capacity(num_segments);
    for s in 0..num_segments {
        let start = offsets[s];
        let end = offsets[s + 1].max(start).min(data.len());
        if start > data.len() {
            return Err(BioFormatsError::Format(
                "DICOM RLE: segment offset past end of fragment".into(),
            ));
        }
        let mut seg = crate::common::codec::decompress_packbits(&data[start..end])?;
        // Each segment should yield exactly `plane` bytes; pad/truncate to be safe.
        seg.resize(plane, 0);
        segments.push(seg);
    }

    // Reassemble into interleaved native output. For sample (channel) c and
    // byte index j of a pixel, the source segment is c*bpp + (bpp-1-j) so that
    // the most-significant byte plane (segment 0 of the sample) lands in the
    // high byte (little-endian native output: low byte first).
    let mut out = vec![0u8; plane * ec * bpp];
    for c in 0..ec {
        for j in 0..bpp {
            // Most-significant plane first in segment order.
            let seg_index = c * bpp + (bpp - 1 - j);
            if seg_index >= segments.len() {
                continue;
            }
            let seg = &segments[seg_index];
            for p in 0..plane {
                out[(p * ec + c) * bpp + j] = seg[p];
            }
        }
    }
    Ok(out)
}

/// Normalises an encapsulated JPEG fragment into a self-contained JPEG stream,
/// mirroring the marker fix-ups in `DicomReader.readTile`:
///   * if byte 2 is not 0xFF, insert a 0xFF there (some encoders drop it);
///   * locate the last EOI (0xFF 0xD9) marker; if absent, append one; if it
///     appears before the end of the buffer, truncate just after it.
fn trim_dicom_jpeg(mut b: Vec<u8>) -> Vec<u8> {
    if b.len() < 8 {
        return b;
    }
    if b[2] != 0xff {
        let mut tmp = Vec::with_capacity(b.len() + 1);
        tmp.push(b[0]);
        tmp.push(b[1]);
        tmp.push(0xff);
        tmp.extend_from_slice(&b[2..]);
        b = tmp;
    }

    // Find the last 0xFF 0xD9 (EOI) marker.
    let mut pt: isize = b.len() as isize - 2;
    while pt >= 0 && !(b[pt as usize] == 0xff && b[pt as usize + 1] == 0xd9) {
        pt -= 1;
    }
    if pt < 0 {
        b.push(0xff);
        b.push(0xd9);
    } else if (pt as usize) < b.len() - 2 {
        b.truncate(pt as usize + 2);
    }
    b
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
    max_pixel_range: i32,
    center_pixel_value: i32,
    palette: PaletteLut,
    /// Per-series ordered file lists, in series order. When grouping finds only
    /// the selected file (or grouping is disabled) this holds a single entry.
    series_files: Vec<Vec<PathBuf>>,
    /// Currently selected series index into `series_files`.
    current_series: usize,
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
            max_pixel_range: 0,
            center_pixel_value: 0,
            palette: PaletteLut::default(),
            series_files: Vec::new(),
            current_series: 0,
        }
    }
}

impl DicomReader {
    /// Load the representative (first) file of `series_index` and populate the
    /// per-series reader state. Mirrors how Java re-derives core metadata per
    /// series from `fileList.get(keys[i]).get(0)`.
    fn load_series(&mut self, series_index: usize) -> Result<()> {
        let files = self
            .series_files
            .get(series_index)
            .ok_or(BioFormatsError::SeriesOutOfRange(series_index))?;
        let rep = files
            .first()
            .ok_or(BioFormatsError::SeriesOutOfRange(series_index))?
            .clone();

        let attrs = parse_dicom(&rep)?;
        let mut meta = build_metadata(&attrs)?;

        if !attrs.encapsulated {
            validate_pixel_data_length(
                &meta,
                attrs.pixel_data_length,
                attrs.samples_per_pixel,
                attrs.bits_allocated,
            )?;
        }

        // When a series spans multiple files, each file contributes its planes
        // along Z (DicomReader multiplies sizeZ by the file count).
        let file_count = files.len() as u32;
        if file_count > 1 {
            meta.size_z = meta.size_z.saturating_mul(file_count).max(1);
            meta.image_count = meta.image_count.saturating_mul(file_count).max(1);
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
        self.max_pixel_range = attrs.max_pixel_range;
        self.center_pixel_value = attrs.center_pixel_value;
        self.palette = attrs.palette;
        self.path = Some(rep);
        self.current_series = series_index;
        Ok(())
    }

    /// Read a single plane from an arbitrary companion file in the current
    /// series. Builds a throwaway single-file reader for `file` (so the full
    /// native/encapsulated pixel pipeline is reused unchanged) and reads the
    /// local plane index within it.
    fn open_plane_from_file(&self, file: &Path, local_plane: u32) -> Result<Vec<u8>> {
        let attrs = parse_dicom(file)?;
        let meta = build_metadata(&attrs)?;
        if !attrs.encapsulated {
            validate_pixel_data_length(
                &meta,
                attrs.pixel_data_length,
                attrs.samples_per_pixel,
                attrs.bits_allocated,
            )?;
        }
        let mut sub = DicomReader::new();
        sub.meta = Some(meta);
        sub.pixel_data_offset = attrs.pixel_data_offset;
        sub.pixel_data_length = attrs.pixel_data_length;
        sub.encapsulated_frames = attrs.encapsulated_frames;
        sub.is_little_endian = attrs.little_endian;
        sub.encapsulated = attrs.encapsulated;
        sub.transfer_syntax = attrs.transfer_syntax;
        sub.photometric_interpretation = attrs.photometric_interpretation;
        sub.planar_configuration = attrs.planar_configuration;
        sub.source_samples_per_pixel = attrs.samples_per_pixel;
        sub.bits_allocated = attrs.bits_allocated;
        sub.bits_stored = attrs.bits_stored;
        sub.pixel_representation = attrs.pixel_representation;
        sub.max_pixel_range = attrs.max_pixel_range;
        sub.center_pixel_value = attrs.center_pixel_value;
        sub.palette = attrs.palette;
        sub.path = Some(file.to_path_buf());
        sub.series_files = vec![vec![file.to_path_buf()]];
        sub.current_series = 0;
        sub.open_bytes(local_plane)
    }

    /// Map a plane index within the current series to the file holding it and
    /// the plane index within that file. For multi-file series each file
    /// contributes `planes_per_file` consecutive planes along Z.
    fn locate_plane(&self, plane_index: u32) -> Result<(PathBuf, u32)> {
        let files = self
            .series_files
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        if files.len() <= 1 {
            let path = files
                .first()
                .or(self.path.as_ref())
                .cloned()
                .ok_or(BioFormatsError::NotInitialized)?;
            return Ok((path, plane_index));
        }
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let planes_per_file = (meta.image_count / files.len() as u32).max(1);
        let file_idx = (plane_index / planes_per_file).min(files.len() as u32 - 1) as usize;
        let local = plane_index % planes_per_file;
        Ok((files[file_idx].clone(), local))
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
        // Parse the selected file first to derive its grouping key.
        let attrs = parse_dicom(path)?;
        let key = group_key_from_attrs(&attrs);

        // Scan the directory and group companion files into series (DicomReader
        // makeFileList / scanDirectory). When grouping yields nothing extra we
        // fall back to a single series containing just the selected file.
        let abs = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let file_list = build_dicom_file_list(path, &key);

        let mut series_files: Vec<Vec<PathBuf>> = file_list.into_values().collect();
        if series_files.is_empty() {
            series_files = vec![vec![abs.clone()]];
        }

        // Select the series that contains the originally requested file so that
        // `series()` reflects it after set_id (Java keeps series 0 selected but
        // the requested file is always present in the list).
        let selected = series_files
            .iter()
            .position(|files| files.iter().any(|f| f == &abs))
            .unwrap_or(0);

        self.series_files = series_files;
        self.load_series(selected)?;
        // Match Java: series 0 is selected after initialisation.
        self.set_series(0)?;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.series_files.clear();
        self.current_series = 0;
        Ok(())
    }

    fn series_count(&self) -> usize {
        self.series_files.len().max(1)
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if s >= self.series_files.len().max(1) {
            return Err(BioFormatsError::SeriesOutOfRange(s));
        }
        if self.series_files.is_empty() {
            // Single-file fallback (no grouping performed).
            if s != 0 {
                return Err(BioFormatsError::SeriesOutOfRange(s));
            }
            return Ok(());
        }
        if s != self.current_series {
            self.load_series(s)?;
        }
        Ok(())
    }

    fn series(&self) -> usize {
        self.current_series
    }

    fn metadata(&self) -> &ImageMetadata {
        self.meta.as_ref().expect("set_id not called")
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        {
            let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
            if plane_index >= meta.image_count {
                return Err(BioFormatsError::PlaneOutOfRange(plane_index));
            }
        }

        // For a series spanning several files, route the plane to the file that
        // holds it and read it with that file's own pixel layout. The
        // representative file's state already covers a single-file series.
        let multi_file = self
            .series_files
            .get(self.current_series)
            .map(|f| f.len() > 1)
            .unwrap_or(false);
        if multi_file {
            let (file, local_plane) = self.locate_plane(plane_index)?;
            return self.open_plane_from_file(&file, local_plane);
        }

        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;

        if self.encapsulated {
            let syntax = classify_transfer_syntax(&self.transfer_syntax);
            if matches!(
                syntax,
                EncapsulatedSyntax::Deflate | EncapsulatedSyntax::Unknown
            ) {
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
            let expected = expected_output_bytes(meta)?;

            match syntax {
                EncapsulatedSyntax::Jpeg2000 => {
                    let decoded = crate::common::codec::decompress_jpeg2000(&encoded)?;
                    if decoded.len() != expected {
                        return Err(BioFormatsError::Codec(format!(
                            "DICOM JPEG 2000 decoded {} bytes, expected {expected}",
                            decoded.len()
                        )));
                    }
                    return Ok(decoded);
                }
                EncapsulatedSyntax::Jpeg => {
                    // Trim the fragment to a clean JPEG stream (Java readTile):
                    // ensure an 0xFF prefix before SOI and a trailing EOI marker.
                    let trimmed = trim_dicom_jpeg(encoded);
                    // Both baseline (process 1) and lossless (process 14) JPEG
                    // are handled by the shared JPEG decoder, which supports the
                    // lossless SOF3 path.
                    let decoded = crate::common::codec::decompress_jpeg(&trimmed)?;
                    if decoded.len() != expected {
                        return Err(BioFormatsError::Codec(format!(
                            "DICOM JPEG decoded {} bytes, expected {expected}",
                            decoded.len()
                        )));
                    }
                    return Ok(decoded);
                }
                EncapsulatedSyntax::Rle => {
                    let ec = self.source_samples_per_pixel.max(1) as usize;
                    let bpp = (self.bits_allocated.max(8) as usize).div_ceil(8);
                    let native = decode_dicom_rle(
                        &encoded,
                        meta.size_x as usize,
                        meta.size_y as usize,
                        ec,
                        bpp,
                    )?;
                    // RLE output is already interleaved (planar config 0);
                    // run it through the native normalisation pipeline.
                    let mut buf = normalize_native_pixels(
                        &native,
                        meta,
                        self.source_samples_per_pixel,
                        self.bits_allocated,
                        self.bits_stored,
                        self.pixel_representation,
                        &self.palette,
                    );
                    if self.photometric_interpretation.trim() == "MONOCHROME1" {
                        invert_monochrome1(
                            &mut buf,
                            meta,
                            self.max_pixel_range,
                            self.center_pixel_value,
                        );
                    }
                    return Ok(buf);
                }
                EncapsulatedSyntax::Deflate | EncapsulatedSyntax::Unknown => unreachable!(),
            }
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
            invert_monochrome1(
                &mut buf,
                meta,
                self.max_pixel_range,
                self.center_pixel_value,
            );
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

#[cfg(test)]
mod tests {
    use super::*;

    fn rle_header(offsets: &[u32]) -> Vec<u8> {
        // 64-byte RLE header: segment count + 15 offsets, all little-endian u32.
        let mut h = vec![0u8; 64];
        h[0..4].copy_from_slice(&(offsets.len() as u32).to_le_bytes());
        for (i, &off) in offsets.iter().enumerate() {
            let o = 4 + i * 4;
            h[o..o + 4].copy_from_slice(&off.to_le_bytes());
        }
        h
    }

    #[test]
    fn dicom_rle_decodes_single_segment_8bit() {
        // 2x2 grayscale, one PackBits segment with literal run [10,20,30,40].
        let mut data = rle_header(&[64]);
        data.extend_from_slice(&[3, 10, 20, 30, 40]); // PackBits: literal of 4 bytes
        let out = decode_dicom_rle(&data, 2, 2, 1, 1).expect("RLE decode");
        assert_eq!(out, vec![10, 20, 30, 40]);
    }

    #[test]
    fn dicom_rle_decodes_16bit_two_segments() {
        // 1x2 (2 pixels) 16-bit, two segments: MSB plane then LSB plane.
        // Pixel values: 0x0102, 0x0304 (little-endian native output).
        let seg_start_0 = 64u32;
        // MSB segment: literal [0x01, 0x03]
        let msb = [1u8, 0x01, 0x03];
        let seg_start_1 = seg_start_0 + msb.len() as u32;
        // LSB segment: literal [0x02, 0x04]
        let lsb = [1u8, 0x02, 0x04];
        let mut data = rle_header(&[seg_start_0, seg_start_1]);
        data.extend_from_slice(&msb);
        data.extend_from_slice(&lsb);
        let out = decode_dicom_rle(&data, 1, 2, 1, 2).expect("RLE 16-bit decode");
        // Little-endian native: low byte first, then high byte.
        assert_eq!(out, vec![0x02, 0x01, 0x04, 0x03]);
    }

    #[test]
    fn dicom_rle_decodes_rgb_three_segments() {
        // 1x1 RGB 8-bit: 3 segments (R, G, B), interleaved on output.
        let s0 = 64u32;
        let r = [0u8, 255]; // PackBits literal of 1 byte: 255
        let s1 = s0 + r.len() as u32;
        let g = [0u8, 128];
        let s2 = s1 + g.len() as u32;
        let b = [0u8, 64];
        let mut data = rle_header(&[s0, s1, s2]);
        data.extend_from_slice(&r);
        data.extend_from_slice(&g);
        data.extend_from_slice(&b);
        let out = decode_dicom_rle(&data, 1, 1, 3, 1).expect("RLE RGB decode");
        assert_eq!(out, vec![255, 128, 64]);
    }

    #[test]
    fn dicom_jpeg_trim_appends_eoi() {
        // No EOI marker present; one should be appended. Needs 0xFF at index 2.
        let input = vec![0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10, 0x00, 0x00];
        let out = trim_dicom_jpeg(input);
        assert_eq!(&out[out.len() - 2..], &[0xff, 0xd9]);
    }

    #[test]
    fn dicom_jpeg_trim_truncates_after_eoi() {
        let input = vec![0xff, 0xd8, 0xff, 0xd9, 0x11, 0x22, 0x33, 0x44];
        let out = trim_dicom_jpeg(input);
        assert_eq!(out, vec![0xff, 0xd8, 0xff, 0xd9]);
    }

    #[test]
    fn classify_transfer_syntax_matches_java() {
        assert_eq!(
            classify_transfer_syntax("1.2.840.10008.1.2.4.90"),
            EncapsulatedSyntax::Jpeg2000
        );
        assert_eq!(
            classify_transfer_syntax("1.2.840.10008.1.2.4.50"),
            EncapsulatedSyntax::Jpeg
        );
        assert_eq!(
            classify_transfer_syntax("1.2.840.10008.1.2.4.70"),
            EncapsulatedSyntax::Jpeg
        );
        assert_eq!(
            classify_transfer_syntax("1.2.840.10008.1.2.5"),
            EncapsulatedSyntax::Rle
        );
        assert_eq!(
            classify_transfer_syntax("1.2.840.10008.1.2.1"),
            EncapsulatedSyntax::Unknown
        );
    }
}
