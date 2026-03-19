//! DICOM format reader (medical imaging).
//!
//! Supports:
//! - Explicit VR Little Endian (most common, default)
//! - Implicit VR Little Endian (legacy)
//! - Unencapsulated (raw) pixel data
//!
//! Does NOT support compressed transfer syntaxes (JPEG, JPEG 2000, etc.).

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use bioformats_common::error::{BioFormatsError, Result};
use bioformats_common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use bioformats_common::pixel_type::PixelType;
use bioformats_common::reader::FormatReader;

// ── VR codes that use 4-byte length (reserved 2 bytes + uint32) ──────────────
fn vr_has_long_length(vr: &[u8; 2]) -> bool {
    matches!(
        vr,
        b"OB" | b"OD" | b"OF" | b"OL" | b"OW" | b"SQ" | b"UC" | b"UN" | b"UR" | b"UT"
    )
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
    transfer_syntax: String,
    pixel_data_offset: u64,
    pixel_data_length: u64,
    little_endian: bool,
    explicit_vr: bool,
    encapsulated: bool,
    extra: HashMap<String, String>,
}

fn ascii_trim(v: &[u8]) -> String {
    std::str::from_utf8(v)
        .unwrap_or("")
        .trim_end_matches(['\0', ' '])
        .to_string()
}

fn parse_dicom(path: &Path) -> Result<DicomAttrs> {
    let f = File::open(path).map_err(BioFormatsError::Io)?;
    let mut r = BufReader::new(f);

    // Skip 128-byte preamble, verify "DICM"
    let mut preamble = [0u8; 132];
    r.read_exact(&mut preamble).map_err(BioFormatsError::Io)?;
    if &preamble[128..132] != b"DICM" {
        return Err(BioFormatsError::Format("Not a DICOM file: missing DICM magic".into()));
    }

    let mut attrs = DicomAttrs {
        little_endian: true,
        explicit_vr: true,
        ..Default::default()
    };

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

    // ── Phase 2: Parse remaining data elements ──────────────────────────────
    loop {
        let pos = r.stream_position().map_err(BioFormatsError::Io)?;
        let group = if attrs.little_endian {
            match read_u16_le(&mut r) {
                Ok(g) => g,
                Err(_) => break,
            }
        } else {
            match read_u16_be(&mut r) {
                Ok(g) => g,
                Err(_) => break,
            }
        };
        let element = if attrs.little_endian {
            read_u16_le(&mut r).map_err(BioFormatsError::Io)?
        } else {
            read_u16_be(&mut r).map_err(BioFormatsError::Io)?
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
                // Encapsulated pixel data — record position but can't easily read
                attrs.pixel_data_offset = r.stream_position().map_err(BioFormatsError::Io)?;
                attrs.pixel_data_length = 0;
                attrs.encapsulated = true;
                break;
            } else {
                // Skip undefined-length SQ/other: try to find item delimiter
                // For simplicity, stop parsing if we hit unknown undefined-length data
                break;
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

        // Decode key imaging tags
        let read_u16 = |v: &[u8]| -> u16 {
            if v.len() >= 2 {
                if attrs.little_endian { u16::from_le_bytes([v[0], v[1]]) }
                else { u16::from_be_bytes([v[0], v[1]]) }
            } else { 0 }
        };
        let _read_u32_val = |v: &[u8]| -> u32 {
            if v.len() >= 4 {
                if attrs.little_endian { u32::from_le_bytes([v[0], v[1], v[2], v[3]]) }
                else { u32::from_be_bytes([v[0], v[1], v[2], v[3]]) }
            } else { 0 }
        };

        match (group, element) {
            (0x0028, 0x0008) => {
                // Number of Frames (IS string)
                let s = ascii_trim(&value);
                attrs.number_of_frames = s.trim().parse().unwrap_or(1);
            }
            (0x0028, 0x0010) => attrs.rows = read_u16(&value),
            (0x0028, 0x0011) => attrs.columns = read_u16(&value),
            (0x0028, 0x0002) => attrs.samples_per_pixel = read_u16(&value),
            (0x0028, 0x0100) => attrs.bits_allocated = read_u16(&value),
            (0x0028, 0x0101) => attrs.bits_stored = read_u16(&value),
            (0x0028, 0x0103) => attrs.pixel_representation = read_u16(&value),
            _ => {
                // Store human-readable metadata for common tags
                let key = format!("({:04X},{:04X})", group, element);
                if &vr == b"LO" || &vr == b"LT" || &vr == b"PN" || &vr == b"SH"
                    || &vr == b"ST" || &vr == b"UI" || &vr == b"CS" || &vr == b"DA"
                    || &vr == b"TM" || &vr == b"DT"
                {
                    attrs.extra.insert(key, ascii_trim(&value));
                }
            }
        }
        let _ = (pos, value_start);
    }

    if attrs.number_of_frames == 0 {
        attrs.number_of_frames = 1;
    }
    if attrs.samples_per_pixel == 0 {
        attrs.samples_per_pixel = 1;
    }

    Ok(attrs)
}

fn build_metadata(a: &DicomAttrs) -> Result<ImageMetadata> {
    if a.rows == 0 || a.columns == 0 {
        return Err(BioFormatsError::Format("DICOM: missing image dimensions".into()));
    }
    let pixel_type = match (a.bits_allocated, a.pixel_representation) {
        (8, _) => PixelType::Uint8,
        (16, 0) => PixelType::Uint16,
        (16, 1) => PixelType::Int16,
        (32, 0) => PixelType::Uint32,
        (32, 1) => PixelType::Int32,
        _ => PixelType::Uint16,
    };

    let is_rgb = a.samples_per_pixel == 3;
    let image_count = a.number_of_frames;

    let mut meta = ImageMetadata {
        size_x: a.columns as u32,
        size_y: a.rows as u32,
        size_z: image_count,
        size_c: a.samples_per_pixel as u32,
        size_t: 1,
        pixel_type,
        bits_per_pixel: a.bits_stored.max(a.bits_allocated) as u8,
        image_count,
        dimension_order: DimensionOrder::XYCZT,
        is_rgb,
        is_interleaved: true,
        is_indexed: false,
        is_little_endian: a.little_endian,
        resolution_count: 1,
        series_metadata: a.extra.iter()
            .map(|(k, v)| (k.clone(), MetadataValue::String(v.clone())))
            .collect(),
        lookup_table: None,
    };

    if !a.transfer_syntax.is_empty() {
        meta.series_metadata.insert(
            "TransferSyntaxUID".into(),
            MetadataValue::String(a.transfer_syntax.clone()),
        );
    }

    Ok(meta)
}

// ── Reader ────────────────────────────────────────────────────────────────────

pub struct DicomReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    pixel_data_offset: u64,
    pixel_data_length: u64,
    is_little_endian: bool,
    encapsulated: bool,
}

impl DicomReader {
    pub fn new() -> Self {
        DicomReader {
            path: None,
            meta: None,
            pixel_data_offset: 0,
            pixel_data_length: 0,
            is_little_endian: true,
            encapsulated: false,
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
        let ext = path.extension().and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("dcm") | Some("dicom") | Some("dic"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.len() >= 132 && &header[128..132] == b"DICM"
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let attrs = parse_dicom(path)?;
        self.meta = Some(build_metadata(&attrs)?);
        self.pixel_data_offset = attrs.pixel_data_offset;
        self.pixel_data_length = attrs.pixel_data_length;
        self.is_little_endian = attrs.little_endian;
        self.encapsulated = attrs.encapsulated;
        self.path = Some(path.to_path_buf());
        Ok(())
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

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;

        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        if self.encapsulated {
            return Err(BioFormatsError::UnsupportedFormat(
                "DICOM: encapsulated (compressed) pixel data is not supported".into(),
            ));
        }

        let bps = meta.pixel_type.bytes_per_sample();
        let plane_bytes = (meta.size_x * meta.size_y * meta.size_c) as usize * bps;
        let plane_offset = self.pixel_data_offset + plane_index as u64 * plane_bytes as u64;

        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(plane_offset)).map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; plane_bytes];
        f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
        Ok(buf)
    }

    fn open_bytes_region(&mut self, plane_index: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
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
}
