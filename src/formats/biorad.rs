//! Bio-Rad PIC confocal format reader.
//!
//! 76-byte little-endian header followed by raw pixel data.
//! Magic: int16 at offset 54 == 12345

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;

const HEADER_SIZE: u64 = 76;
const FILE_ID: i16 = 12345;

fn r_i16(b: &[u8], off: usize) -> i16 {
    i16::from_le_bytes([b[off], b[off + 1]])
}
fn r_f32(b: &[u8], off: usize) -> f32 {
    f32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

fn positive_i16_dim(value: i16, label: &str) -> Result<u32> {
    if value <= 0 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "Bio-Rad PIC {label} is non-positive ({value})"
        )));
    }
    Ok(value as u32)
}

/// Java `LUT_LENGTH`: each lookup-table ramp is 256 bytes.
const LUT_LENGTH: usize = 256;

/// One per-channel lookup table: three 256-byte ramps (red, green, blue).
/// Mirrors a single `byte[3][LUT_LENGTH]` entry of Java `lut`.
type ChannelLut = [[u8; LUT_LENGTH]; 3];

pub struct BioRadReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    npic: u32,
    bytes_per_pixel: usize,
    /// All PIC files contributing planes, sorted. When more than one, each
    /// file supplies `npic` planes that together form the channels.
    pic_files: Vec<PathBuf>,
    /// Per-channel lookup tables read from the trailing colour ramps (Java
    /// `byte[][][] lut`); `None` when the file carries no usable LUT.
    lut: Option<Vec<ChannelLut>>,
    /// Channel index of the most recently opened plane (Java `lastChannel`);
    /// selects which entry of `lut` `eight_bit_lookup_table()` returns.
    last_channel: usize,
    /// Set when the note block runs past the end of file or contains an
    /// out-of-range note type (Java `brokenNotes`); aborts LUT reading.
    broken_notes: bool,
    /// Notes parsed from the trailing note block (Java `List<Note> noteStrings`).
    note_strings: Vec<Note>,
    /// Per-detector offsets/gains accumulated while parsing notes (Java
    /// `List<Double> offset` / `gain`); read by `ome_metadata`.
    offset: Vec<Option<f64>>,
    gain: Vec<Option<f64>>,
    /// All files belonging to this series (Java `List<String> used`): the PIC
    /// file(s) plus any companion lse.xml / data.raw. Read by `used_files()`.
    used: Vec<PathBuf>,
}

impl BioRadReader {
    pub fn new() -> Self {
        BioRadReader {
            path: None,
            meta: None,
            npic: 1,
            bytes_per_pixel: 1,
            pic_files: Vec::new(),
            lut: None,
            last_channel: 0,
            broken_notes: false,
            note_strings: Vec::new(),
            offset: Vec::new(),
            gain: Vec::new(),
            used: Vec::new(),
        }
    }

    /// Mirrors Java `get8BitLookupTable()`: the lookup table for the channel of
    /// the most recently opened plane, or `None` when the file carries no LUT.
    pub fn eight_bit_lookup_table(&self) -> Option<&ChannelLut> {
        self.lut.as_ref().and_then(|l| l.get(self.last_channel))
    }

    /// Mirrors Java `getSeriesUsedFiles(false)`: every file backing this series
    /// (the PIC file(s) plus any lse.xml / data.raw companion).
    pub fn used_files(&self) -> &[PathBuf] {
        &self.used
    }
}

fn resolve_biorad_pic_for_companion(path: &Path) -> Result<PathBuf> {
    let dir = path
        .parent()
        .ok_or_else(|| BioFormatsError::Format("Bio-Rad companion has no parent".into()))?;
    let mut found = None;
    for entry in std::fs::read_dir(dir).map_err(BioFormatsError::Io)? {
        let entry = entry.map_err(BioFormatsError::Io)?;
        let candidate = entry.path();
        if candidate
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("pic"))
            .unwrap_or(false)
        {
            found = Some(candidate);
        }
    }
    found.ok_or_else(|| BioFormatsError::Format("No .pic files found - invalid dataset.".into()))
}

/// Mirrors Java `NOTE_NAMES`; index = note type, length 23 (indices 0..=22).
const NOTE_NAMES: [&str; 23] = [
    "0",
    "LIVE",
    "FILE1",
    "NUMBER",
    "USER",
    "LINE",
    "COLLECT",
    "FILE2",
    "SCALEBAR",
    "MERGE",
    "THRUVIEW",
    "ARROW",
    "12",
    "13",
    "14",
    "15",
    "16",
    "17",
    "18",
    "19",
    "VARIABLE",
    "STRUCTURE",
    "4D SERIES",
];

/// The maximum valid note type (NOTE_NAMES has 23 entries, indices 0..=22).
const NOTE_NAMES_LEN: i16 = NOTE_NAMES.len() as i16;

/// Java `STRUCTURE_LABELS_1` (scan-record scalar labels).
const STRUCTURE_LABELS_1: [&str; 15] = [
    "Scan Channel",
    "Both mode",
    "Speed",
    "Filter",
    "Factor",
    "Number of scans",
    "Photon counting mode (channel 1)",
    "Photon counting detector (channel 1)",
    "Photon counting mode (channel 2)",
    "Photon counting detector (channel 2)",
    "Photon mode",
    "Objective magnification",
    "Zoom factor",
    "Motor on",
    "Z Step Size",
];

/// Java `STRUCTURE_LABELS_2` (scan-area scalar labels).
const STRUCTURE_LABELS_2: [&str; 6] = [
    "Z Start",
    "Z Stop",
    "Scan area X coordinate",
    "Scan area Y coordinate",
    "Scan area width",
    "Scan area height",
];

// Java NOTE_TYPE_* constants (loci.formats.in.BioRadReader).
const NOTE_TYPE_USER: i16 = 4;
const NOTE_TYPE_SCALEBAR: i16 = 8;
const NOTE_TYPE_ARROW: i16 = 11;
const NOTE_TYPE_VARIABLE: i16 = 20;
const NOTE_TYPE_STRUCTURE: i16 = 21;

/// A single Bio-Rad note (mirrors the Java `Note` class).
struct Note {
    /// Note type (index into `NOTE_NAMES`); mirrors Java `Note.type`.
    note_type: i16,
    level: i16,
    num: i16,
    status: i16,
    x: i16,
    y: i16,
    /// The note text, trimmed of trailing binary/whitespace.
    p: String,
}

impl Note {
    /// Mirrors Java `Note.toString()` used by `addGlobalMetaList("Note", ...)`.
    fn to_display(&self) -> String {
        let type_name = if self.note_type >= 0 && self.note_type < NOTE_NAMES_LEN {
            NOTE_NAMES[self.note_type as usize]
        } else {
            "?"
        };
        format!(
            "level={}; num={}; status={}; type={}; x={}; y={}; text={}",
            self.level,
            self.num,
            self.status,
            type_name,
            self.x,
            self.y,
            self.p.trim()
        )
    }
}

/// Read all of the note strings from the file, following the pixel data.
///
/// Mirrors Java `readNotes(s, true)`. The seek offset depends on whether a
/// multi-file group has already been established: if `pic_files` is None each
/// note block follows all `image_count` planes; otherwise it follows just the
/// per-file plane count (`image_count / n_files`).
///
/// Returns the collected notes plus any sizeZ/sizeT override implied by an
/// AXIS_4 note whose note-type token is 2 (single Z, time series).
struct ReadNotesResult {
    notes: Vec<Note>,
    size_z: Option<u32>,
    size_t: Option<u32>,
    /// Set when the note block ran past EOF or held an out-of-range note type
    /// (Java `brokenNotes`).
    broken_notes: bool,
}

/// Java `readNotes`: offset at which the trailing note block begins (after the
/// header at 70 and the skipped pixel data + 6 padding bytes).
fn notes_start_offset(
    size_x: u32,
    size_y: u32,
    image_count: u32,
    bpp: usize,
    n_files: Option<u32>,
) -> u64 {
    // Java seeks to 70, then skips bpp * imageLen + 6.
    let mut image_len = size_x as u64 * size_y as u64;
    match n_files {
        None => image_len *= image_count as u64,
        Some(nf) if nf > 0 => image_len *= (image_count / nf) as u64,
        _ => image_len *= image_count as u64,
    }
    70 + bpp as u64 * image_len + 6
}

/// Port of Java `readNotes(s, true)`: read the note block, collecting `Note`s.
/// Leaves the file positioned immediately after the last note read, which is
/// where Java `readLookupTables` resumes to read the colour ramps.
fn read_notes(
    f: &mut File,
    size_x: u32,
    size_y: u32,
    image_count: u32,
    bpp: usize,
    n_files: Option<u32>,
) -> ReadNotesResult {
    let mut result = ReadNotesResult {
        notes: Vec::new(),
        size_z: None,
        size_t: None,
        broken_notes: false,
    };

    let notes_start = notes_start_offset(size_x, size_y, image_count, bpp, n_files);
    if f.seek(SeekFrom::Start(notes_start)).is_err() {
        return result;
    }
    let file_len = f.metadata().map(|m| m.len()).unwrap_or(0);

    // Each note: level(i16), notesFlag(i32), num(i16), status(i16), type(i16),
    // x(i16), y(i16), text(80 bytes) = 16 + 80 bytes.
    let mut more = true;
    let mut guard = 0;
    while more && guard < 1_000_000 {
        guard += 1;
        // Java: if getFilePointer() >= length() -> brokenNotes, break.
        let pos = f.stream_position().unwrap_or(file_len);
        if pos >= file_len {
            result.broken_notes = true;
            break;
        }
        let mut hdr = [0u8; 16];
        if f.read_exact(&mut hdr).is_err() {
            result.broken_notes = true;
            break;
        }
        let level = i16::from_le_bytes([hdr[0], hdr[1]]);
        more = i32::from_le_bytes([hdr[2], hdr[3], hdr[4], hdr[5]]) != 0;
        let num = i16::from_le_bytes([hdr[6], hdr[7]]);
        let status = i16::from_le_bytes([hdr[8], hdr[9]]);
        let note_type = i16::from_le_bytes([hdr[10], hdr[11]]);
        let x = i16::from_le_bytes([hdr[12], hdr[13]]);
        let y = i16::from_le_bytes([hdr[14], hdr[15]]);
        // Java: if type < 0 || type >= NOTE_NAMES.length -> broken notes, stop.
        if note_type < 0 || note_type >= NOTE_NAMES_LEN {
            result.broken_notes = true;
            break;
        }
        let mut text = [0u8; 80];
        if f.read_exact(&mut text).is_err() {
            result.broken_notes = true;
            break;
        }
        // Remove binary data (trim at first NUL), then trim whitespace.
        let end = text.iter().position(|&c| c == 0).unwrap_or(80);
        let p = String::from_utf8_lossy(&text[..end]).trim().to_string();

        // Java readNotes: tokenize value (with '=' removed); if tokens.len > 1
        // and tokens[1] parses to 2 and value contains "AXIS_4" -> sizeZ=1,
        // sizeT=imageCount.
        let value = p.replace('=', "");
        let tokens: Vec<&str> = value.split_whitespace().collect();
        if tokens.len() > 1 {
            if let Ok(nt) = tokens[1].parse::<i32>() {
                if nt == 2 && value.contains("AXIS_4") {
                    result.size_z = Some(1);
                    result.size_t = Some(image_count);
                }
            }
        }

        result.notes.push(Note {
            note_type,
            level,
            num,
            status,
            x,
            y,
            p,
        });
    }
    result
}

/// Port of Java `readLookupTables`: after the note block, the file carries
/// per-channel colour ramps of `LUT_LENGTH` bytes (red, green, blue). Reads up
/// to `n_channels` tables from `f`, which must already be positioned at (or
/// before) the note block. Returns `None` (Java `lut = null`) when no table is
/// present or the notes are broken — matching Java's behaviour where a missing
/// first table or `brokenNotes` clears the whole LUT.
fn read_lookup_tables(
    f: &mut File,
    n_channels: usize,
    size_x: u32,
    size_y: u32,
    image_count: u32,
    bpp: usize,
    n_files: Option<u32>,
) -> Option<Vec<ChannelLut>> {
    // Java readLookupTables calls readNotes(s, false) to skip past the notes,
    // then reads colour tables from the current position.
    let notes = read_notes(f, size_x, size_y, image_count, bpp, n_files);
    if notes.broken_notes {
        return None;
    }
    let file_len = f.metadata().map(|m| m.len()).unwrap_or(0);

    let mut lut: Vec<ChannelLut> = Vec::new();
    let mut channel = 0usize;
    let mut next = 0usize;
    let mut current: ChannelLut = [[0u8; LUT_LENGTH]; 3];
    while channel < n_channels {
        let pos = f.stream_position().unwrap_or(file_len);
        if pos + LUT_LENGTH as u64 > file_len {
            break;
        }
        if f.read_exact(&mut current[next]).is_err() {
            break;
        }
        next += 1;
        if next == 3 {
            next = 0;
            lut.push(current);
            current = [[0u8; LUT_LENGTH]; 3];
            channel += 1;
        }
    }
    // Java: if eof && channel == 0 -> lut = null. We never read a full table.
    if lut.is_empty() {
        return None;
    }
    Some(lut)
}

/// Per-detector offset/gain accumulated from notes (mirrors Java `offset`/`gain`
/// `List<Double>` fields). Index == detector index.
#[derive(Default)]
struct DetectorSettings {
    offset: Vec<Option<f64>>,
    gain: Vec<Option<f64>>,
}

/// Result of porting Java `parseNotes`: dimension overrides plus the note-type
/// taxonomy and instrument/acquisition metadata extracted from the notes.
#[derive(Default)]
struct ParseNotesResult {
    multiple_files: bool,
    size_c: Option<u32>,
    size_z: Option<u32>,
    size_t: Option<u32>,
    /// Free-form key/value metadata (mirrors Java `addGlobalMeta` /
    /// `addGlobalMetaList`); collected in encounter order.
    global_meta: Vec<(String, String)>,
    /// Objective model (from `INFO_OBJECTIVE_NAME`).
    objective_model: Option<String>,
    /// Objective nominal magnification (from `INFO_OBJECTIVE_MAGNIFICATION`,
    /// `LENS_MAGNIFICATION`, or STRUCTURE type-1 record).
    objective_magnification: Option<f64>,
    /// Pixel physical sizes in microns (from AXIS_2/AXIS_3 / STRUCTURE notes).
    physical_size_x: Option<f64>,
    physical_size_y: Option<f64>,
    physical_size_z: Option<f64>,
    /// Per-detector settings (offset/gain).
    detectors: DetectorSettings,
}

#[derive(Default)]
struct CompanionMetadata {
    pics: Vec<PathBuf>,
    used: Vec<PathBuf>,
    size_z: Option<u32>,
    size_c: Option<u32>,
    size_t: Option<u32>,
    global_meta: Vec<(String, String)>,
}

fn push_global_meta_list(
    global_meta: &mut Vec<(String, String)>,
    key: &str,
    value: impl Into<String>,
) {
    let count = global_meta
        .iter()
        .filter(|(k, _)| k == key || k.starts_with(&format!("{key} #")))
        .count();
    let stored_key = if count == 0 {
        key.to_string()
    } else {
        format!("{key} #{}", count + 1)
    };
    global_meta.push((stored_key, value.into()));
}

impl ParseNotesResult {
    fn add_global(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.global_meta.push((key.into(), value.into()));
    }

    /// Mirrors Java `addGlobalMetaList(key, value)`: appends with a numbered
    /// suffix when the bare key already exists.
    fn add_global_list(&mut self, key: &str, value: impl Into<String>) {
        push_global_meta_list(&mut self.global_meta, key, value);
    }
}

fn scan_biorad_companions(
    pic_path: &Path,
    size_z: u32,
    size_c: u32,
    size_t: u32,
    image_count: u32,
) -> CompanionMetadata {
    let mut result = CompanionMetadata::default();
    let Some(parent) = pic_path.parent() else {
        return result;
    };
    let Ok(entries) = std::fs::read_dir(parent) else {
        return result;
    };
    let mut files: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
    files.sort();

    for file in &files {
        let name = file
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if name.eq_ignore_ascii_case("lse.xml") {
            result.used.push(file.clone());
            apply_biorad_lse_xml(file, &mut result, size_z, size_c, size_t, image_count);
            for candidate in &files {
                if candidate
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.eq_ignore_ascii_case("pic"))
                    .unwrap_or(false)
                {
                    result.pics.push(candidate.clone());
                    if !result.used.contains(candidate) {
                        result.used.push(candidate.clone());
                    }
                }
            }
        } else if name.eq_ignore_ascii_case("data.raw") {
            result.used.push(file.clone());
        }
    }

    result.pics.sort();
    result.pics.dedup();
    result.used.dedup();
    result
}

fn apply_biorad_lse_xml(
    path: &Path,
    result: &mut CompanionMetadata,
    size_z: u32,
    size_c: u32,
    size_t: u32,
    image_count: u32,
) {
    let Ok(xml) = std::fs::read_to_string(path) else {
        return;
    };
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(&xml);
    reader.config_mut().trim_text(false);
    let decoder = reader.decoder();

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let name = e.local_name();
                if name.as_ref() == b"Pixels" {
                    let mut z = 1u32;
                    let mut c = 1u32;
                    let mut t = 1u32;
                    for attr in e.attributes().flatten() {
                        let key = attr.key.as_ref().to_vec();
                        let Some(value) = crate::common::xml::decode_xml_attr(attr, decoder) else {
                            continue;
                        };
                        match key.as_slice() {
                            b"SizeZ" => z = value.parse().unwrap_or(1),
                            b"SizeC" => c = value.parse().unwrap_or(1),
                            b"SizeT" => t = value.parse().unwrap_or(1),
                            _ => {}
                        }
                    }
                    let count = size_z.saturating_mul(size_c).saturating_mul(size_t);
                    result.size_z = Some(z);
                    result.size_c = Some(c);
                    result.size_t = Some(t);
                    if count < image_count && count > 0 {
                        result.size_c = Some(image_count / count);
                    }
                } else if matches!(name.as_ref(), b"Z" | b"C" | b"T") {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"TimeCompleted" {
                            if let Some(value) = crate::common::xml::decode_xml_attr(attr, decoder)
                            {
                                push_global_meta_list(&mut result.global_meta, "Timestamp", value);
                            }
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(_) => break,
        }
    }
}

/// Set the detector offset at `index`, growing the list with `None` as needed
/// (mirrors the Java `while (nextDetector > offset.size()) offset.add(null)`).
fn set_detector_value(list: &mut Vec<Option<f64>>, index: usize, value: Option<f64>) {
    if index < list.len() {
        list[index] = value;
    } else {
        while list.len() < index {
            list.push(None);
        }
        list.push(value);
    }
}

/// Port of Java `BioRadReader.parseNotes`. Translates the note-type taxonomy
/// (USER/SCALEBAR/ARROW/VARIABLE/STRUCTURE -> global metadata + instrument
/// fields) and the AXIS dimension parsing. Returns dimension overrides plus the
/// collected metadata for projection into `ImageMetadata`/OME.
fn parse_notes(notes: &[Note], image_count: u32, size_x: u32, size_y: u32) -> ParseNotesResult {
    let mut result = ParseNotesResult::default();
    let mut next_detector: usize = 0;
    let mut n_lasers: usize = 0;

    for n in notes {
        match n.note_type {
            NOTE_TYPE_USER | NOTE_TYPE_SCALEBAR | NOTE_TYPE_ARROW => {
                // TODO (Java): these should become overlays.
                result.add_global_list("Note", n.to_display());
            }
            NOTE_TYPE_VARIABLE => {
                parse_variable_note(&mut result, n, &mut next_detector, size_x, size_y);
            }
            NOTE_TYPE_STRUCTURE => {
                parse_structure_note(&mut result, n, &mut n_lasers, size_x, size_y);
            }
            _ => {
                // Notes for display only.
                result.add_global_list("Note", n.to_display());
            }
        }

        // If the text contains "AXIS", parse it more thoroughly (BioRad spec p.21).
        if n.p.contains("AXIS") {
            parse_axis_note(&mut result, n, image_count);
        }
    }
    result
}

/// Java `parseNotes` case `NOTE_TYPE_VARIABLE`: key=value instrument metadata.
fn parse_variable_note(
    result: &mut ParseNotesResult,
    n: &Note,
    next_detector: &mut usize,
    size_x: u32,
    size_y: u32,
) {
    let _ = (size_x, size_y);
    if let Some(eq) = n.p.find('=') {
        let key = n.p[..eq].trim().to_string();
        let value = n.p[eq + 1..].trim().to_string();
        result.add_global(key.clone(), value.clone());

        if key == "INFO_OBJECTIVE_NAME" {
            result.objective_model = Some(value);
        } else if key == "INFO_OBJECTIVE_MAGNIFICATION" {
            if let Ok(mag) = value.parse::<f64>() {
                result.objective_magnification = Some(mag);
            }
        } else if key == "LENS_MAGNIFICATION" {
            if let Ok(mag) = value.parse::<f64>() {
                result.objective_magnification = Some(mag);
            }
        } else if key.starts_with("SETTING") {
            if let Some(det_pos) = key.find("_DET_") {
                let index = det_pos + 5;
                // Java: if (key.lastIndexOf("_") > index) -> a trailing field.
                if key.rfind('_').map(|p| p > index).unwrap_or(false) {
                    let parsed = value.parse::<f64>().ok();
                    if key.ends_with("OFFSET") {
                        set_detector_value(&mut result.detectors.offset, *next_detector, parsed);
                    } else if key.ends_with("GAIN") {
                        set_detector_value(&mut result.detectors.gain, *next_detector, parsed);
                    }
                    *next_detector += 1;
                }
            }
        } else {
            // Possible "<type> ... <pixelSize>" axis-size record.
            let values: Vec<&str> = value.split(' ').collect();
            if values.len() > 1 {
                if let Ok(t) = values[0].parse::<i32>() {
                    if t == 257 && values.len() >= 3 {
                        if let Ok(pixel_size) = values[2].parse::<f64>() {
                            if key == "AXIS_2" {
                                result.physical_size_x = Some(pixel_size);
                            } else if key == "AXIS_3" {
                                result.physical_size_y = Some(pixel_size);
                            }
                        }
                    }
                }
            }
        }
    } else if n.p.starts_with("AXIS_2") {
        let values: Vec<&str> = n.p.split(' ').collect();
        if let Some(ps) = values.get(3).and_then(|v| v.parse::<f64>().ok()) {
            result.physical_size_x = Some(ps);
        }
    } else if n.p.starts_with("AXIS_3") {
        let values: Vec<&str> = n.p.split(' ').collect();
        if let Some(ps) = values.get(3).and_then(|v| v.parse::<f64>().ok()) {
            result.physical_size_y = Some(ps);
        }
    } else {
        result.add_global_list("Note", n.to_display());
    }
}

/// Java `parseNotes` case `NOTE_TYPE_STRUCTURE`: complex binary records. We
/// translate the instrument-relevant `structureType == 1` cases that the Java
/// reader projects into OME (objective magnification, physical sizes, detector
/// offset/gain) plus the labelled scalar records.
fn parse_structure_note(
    result: &mut ParseNotesResult,
    n: &Note,
    n_lasers: &mut usize,
    size_x: u32,
    size_y: u32,
) {
    let structure_type = (n.x as u16 & 0xff00) >> 8;
    let values: Vec<&str> = n.p.split(' ').collect();
    let get = |i: usize| values.get(i).copied().unwrap_or("");
    let parse = |i: usize| values.get(i).and_then(|v| v.parse::<f64>().ok());

    if structure_type != 1 {
        return;
    }
    match n.y {
        1 => {
            for (i, label) in STRUCTURE_LABELS_1.iter().enumerate() {
                if i < values.len() {
                    result.add_global(*label, get(i));
                }
            }
            if let Some(mag) = parse(11) {
                result.objective_magnification = Some(mag);
            }
            if let Some(sz) = parse(14) {
                result.physical_size_z = Some(sz);
            }
        }
        2 => {
            for (i, label) in STRUCTURE_LABELS_2.iter().enumerate() {
                if i < values.len() {
                    result.add_global(*label, get(i));
                }
            }
            if let (Some(x1), Some(x2), Some(y1), Some(y2)) =
                (parse(2), parse(4), parse(3), parse(5))
            {
                if size_x > 0 {
                    result.physical_size_x = Some((x2 - x1) / size_x as f64);
                }
                if size_y > 0 {
                    result.physical_size_y = Some((y2 - y1) / size_y as f64);
                }
            }
        }
        11 => {
            for i in 0..3 {
                let prefix = format!("Transmission detector {} - ", i + 1);
                result.add_global(format!("{prefix}offset"), get(i * 3));
                result.add_global(format!("{prefix}gain"), get(i * 3 + 1));
                result.add_global(format!("{prefix}black level"), get(i * 3 + 2));
                set_detector_value(&mut result.detectors.offset, i, parse(i * 3));
                set_detector_value(&mut result.detectors.gain, i, parse(i * 3 + 1));
            }
        }
        4 => {
            if let Ok(n) = get(0).parse::<usize>() {
                *n_lasers = n;
            }
            result.add_global("Number of lasers", get(0));
            result.add_global("Number of transmission detectors", get(1));
            result.add_global("Number of PMTs", get(2));
        }
        _ => {}
    }
}

/// Java `parseNotes` AXIS handling. axisType 11 with AXIS_4 marks a
/// single-section multi-channel dataset; with AXIS_9 a multi-file channel split.
fn parse_axis_note(result: &mut ParseNotesResult, n: &Note, image_count: u32) {
    let cleaned = n.p.replace('=', "");
    let values: Vec<&str> = cleaned.split_whitespace().collect();
    if values.len() < 2 {
        return;
    }
    let key = values[0];
    let axis_type: i32 = match values[1].parse() {
        Ok(v) => v,
        Err(_) => return,
    };

    if axis_type == 11 && values.len() > 2 {
        result.add_global(format!("{key} RGB type (X)"), values[2]);
        if let Some(v) = values.get(3) {
            result.add_global(format!("{key} RGB type (Y)"), *v);
        }
        if key == "AXIS_4" {
            // single section multi-channel dataset
            result.size_c = Some(image_count);
            result.size_z = Some(1);
            result.size_t = Some(1);
        } else if key == "AXIS_9" {
            result.multiple_files = true;
            // sizeC = (int) Double.parseDouble(values[3])
            if let Some(v) = values.get(3) {
                if let Ok(c) = v.parse::<f64>() {
                    result.size_c = Some(c as u32);
                }
            }
        }
    }

    // Java emits descriptive global metadata for several axis types.
    if values.len() > 2 {
        match axis_type {
            1 => {
                result.add_global(format!("{key} distance (X) in microns"), values[2]);
                if let Some(v) = values.get(3) {
                    result.add_global(format!("{key} distance (Y) in microns"), *v);
                }
            }
            3 => {
                result.add_global(format!("{key} angle (X) in degrees"), values[2]);
                if let Some(v) = values.get(3) {
                    result.add_global(format!("{key} angle (Y) in degrees"), *v);
                }
            }
            4 => {
                result.add_global(format!("{key} intensity (X)"), values[2]);
                if let Some(v) = values.get(3) {
                    result.add_global(format!("{key} intensity (Y)"), *v);
                }
            }
            6 => {
                result.add_global(format!("{key} ratio (X)"), values[2]);
                if let Some(v) = values.get(3) {
                    result.add_global(format!("{key} ratio (Y)"), *v);
                }
            }
            7 => {
                result.add_global(format!("{key} log ratio (X)"), values[2]);
                if let Some(v) = values.get(3) {
                    result.add_global(format!("{key} log ratio (Y)"), *v);
                }
            }
            _ => {}
        }
    }
}

impl Default for BioRadReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for BioRadReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        if path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("pic"))
            .unwrap_or(false)
        {
            return true;
        }
        path.file_name()
            .and_then(|name| name.to_str())
            .map(|name| {
                let name = name.to_ascii_lowercase();
                name == "lse.xml" || name == "data.raw"
            })
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // Java BioRadReader accepts either a PIC file_id at offset 54 or an
        // "[Input Sources]" companion-style header.
        header.len() >= 56
            && (i16::from_le_bytes([header[54], header[55]]) == FILE_ID
                || header.starts_with(b"[Input Sources]"))
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let path = if self.is_this_type_by_name(path)
            && !path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("pic"))
                .unwrap_or(false)
        {
            resolve_biorad_pic_for_companion(path)?
        } else {
            path.to_path_buf()
        };
        let mut f = File::open(&path).map_err(BioFormatsError::Io)?;
        let mut hdr = [0u8; HEADER_SIZE as usize];
        f.read_exact(&mut hdr).map_err(BioFormatsError::Io)?;

        if r_i16(&hdr, 54) != FILE_ID {
            return Err(BioFormatsError::Format("Not a Bio-Rad PIC file".into()));
        }

        let nx = positive_i16_dim(r_i16(&hdr, 0), "width")?;
        let ny = positive_i16_dim(r_i16(&hdr, 2), "height")?;
        let npic = positive_i16_dim(r_i16(&hdr, 4), "image count")?;
        // Java: pixelType = (byteFormat == 0) ? UINT16 : UINT8. Any nonzero
        // byteFormat means 8-bit data.
        let byte_format = r_i16(&hdr, 14); // 0=uint16 (2 bytes), nonzero=uint8 (1 byte)
        let bpp = if byte_format != 0 { 1usize } else { 2usize };
        let pixel_type = if bpp == 1 {
            PixelType::Uint8
        } else {
            PixelType::Uint16
        };
        let plane_bytes = (nx as u64)
            .checked_mul(ny as u64)
            .and_then(|v| v.checked_mul(bpp as u64))
            .ok_or_else(|| {
                BioFormatsError::Format("Bio-Rad PIC plane byte count overflows".into())
            })?;
        let pixel_bytes = plane_bytes.checked_mul(npic as u64).ok_or_else(|| {
            BioFormatsError::Format("Bio-Rad PIC pixel byte count overflows".into())
        })?;
        let required_len = HEADER_SIZE.checked_add(pixel_bytes).ok_or_else(|| {
            BioFormatsError::Format("Bio-Rad PIC payload offset overflows".into())
        })?;
        let file_len = f.metadata().map_err(BioFormatsError::Io)?.len();
        if file_len < required_len {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "Bio-Rad PIC pixel payload is shorter than declared: need {required_len} bytes, found {file_len}"
            )));
        }
        let name_bytes = &hdr[18..50];
        let name = String::from_utf8_lossy(name_bytes)
            .trim_end_matches('\0')
            .to_string();

        let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
        if !name.is_empty() {
            meta_map.insert("name".into(), MetadataValue::String(name));
        }
        meta_map.insert("lens".into(), MetadataValue::Int(r_i16(&hdr, 64) as i64));
        meta_map.insert(
            "mag_factor".into(),
            MetadataValue::Float(r_f32(&hdr, 66) as f64),
        );

        // Java defaults: sizeZ = imageCount (npic), sizeC = 1, sizeT = 1.
        let mut size_z = npic;
        let mut size_c = 1u32;
        let mut size_t = 1u32;

        // Java initFile: used starts with this PIC file (companion lse.xml /
        // data.raw would be appended during file grouping, which our pure-Rust
        // path does not parse).
        let mut used: Vec<PathBuf> = vec![path.to_path_buf()];

        // Read notes (no group established yet, so notes follow all planes).
        let notes = read_notes(&mut f, nx, ny, npic, bpp, None);
        self.broken_notes = notes.broken_notes;
        // readNotes AXIS_4/noteType==2 override: sizeZ=1, sizeT=imageCount.
        if let (Some(z), Some(t)) = (notes.size_z, notes.size_t) {
            size_z = z;
            size_t = t;
        }

        // Java initFile scans grouped datasets for lse.xml/data.raw. The SAX
        // BioRadHandler applies <Pixels SizeZ/SizeC/SizeT> and records
        // TimeCompleted attributes, and lse.xml forces all sibling PIC files
        // into the grouped series. This happens before Java parseNotes(), so
        // AXIS notes below can still override XML-derived dimensions.
        let companion = scan_biorad_companions(&path, size_z, size_c, size_t, npic);
        if let Some(z) = companion.size_z {
            size_z = z;
        }
        if let Some(c) = companion.size_c {
            size_c = c;
        }
        if let Some(t) = companion.size_t {
            size_t = t;
        }
        for (k, v) in &companion.global_meta {
            meta_map
                .entry(k.clone())
                .or_insert_with(|| MetadataValue::String(v.clone()));
        }
        for p in &companion.used {
            if !used.contains(p) {
                used.push(p.clone());
            }
        }

        // parseNotes: AXIS-driven sizeC/sizeZ/sizeT derivation + multiple-files,
        // plus the note-type taxonomy and instrument/acquisition metadata.
        let parsed = parse_notes(&notes.notes, npic, nx, ny);
        if let Some(c) = parsed.size_c {
            size_c = c;
        }
        if let Some(z) = parsed.size_z {
            size_z = z;
        }
        if let Some(t) = parsed.size_t {
            size_t = t;
        }
        let multiple_files = parsed.multiple_files;

        // Project note-derived metadata into series_metadata. Mirrors Java's
        // addGlobalMeta/addGlobalMetaList: each key/value pair is preserved.
        for (k, v) in &parsed.global_meta {
            meta_map
                .entry(k.clone())
                .or_insert_with(|| MetadataValue::String(v.clone()));
        }
        // Instrument/acquisition fields, keyed so ome_metadata() can project
        // them into the OME Instrument/Objective/Detector elements.
        if let Some(model) = &parsed.objective_model {
            meta_map.insert(
                "objective.model".into(),
                MetadataValue::String(model.clone()),
            );
        }
        if let Some(mag) = parsed.objective_magnification {
            meta_map.insert(
                "objective.nominal_magnification".into(),
                MetadataValue::Float(mag),
            );
        }
        if let Some(px) = parsed.physical_size_x {
            meta_map.insert("physical_size_x".into(), MetadataValue::Float(px));
        }
        if let Some(py) = parsed.physical_size_y {
            meta_map.insert("physical_size_y".into(), MetadataValue::Float(py));
        }
        if let Some(pz) = parsed.physical_size_z {
            meta_map.insert("physical_size_z".into(), MetadataValue::Float(pz));
        }
        // Java fields `offset`/`gain`: per-detector lists accumulated by
        // parseNotes; read by ome_metadata() to fill the Detector elements and
        // per-channel DetectorSettings.
        self.offset = parsed.detectors.offset.clone();
        self.gain = parsed.detectors.gain.clone();

        // File grouping: when notes indicate multiple files, enumerate the
        // sibling PIC files via a FilePattern over the numbered filename and
        // keep those whose length matches this file's length (Java
        // initFile/FilePattern path). Order by name (Arrays.sort(picFiles)).
        let mut pics: Vec<PathBuf> = companion.pics;
        if multiple_files {
            if let Ok(this_len) = std::fs::metadata(&path).map(|m| m.len()) {
                if let Ok(pattern) = crate::stitcher::FilePattern::from_file(&path) {
                    for file in pattern.filenames() {
                        let is_pic = file
                            .extension()
                            .and_then(|e| e.to_str())
                            .map(|e| e.eq_ignore_ascii_case("pic"))
                            .unwrap_or(false);
                        if is_pic
                            && std::fs::metadata(&file)
                                .map(|m| m.len() == this_len)
                                .unwrap_or(false)
                        {
                            pics.push(file);
                        }
                    }
                }
            }
            // Java: if pics.size() == 1, sizeC = 1.
            if pics.len() == 1 {
                size_c = 1;
            }
        }
        pics.sort();
        pics.dedup();

        // Java: if picFiles.length > 0 -> imageCount = npic * picFiles.length,
        // then sizeT or sizeC derived from the remainder. Otherwise picFiles is
        // null and imageCount stays npic.
        let pic_files: Vec<PathBuf>;
        let image_count: u32;
        if !pics.is_empty() {
            if size_c == 0 {
                size_c = 1;
            }
            let n_files = pics.len() as u32;
            image_count = npic * n_files;
            if multiple_files {
                let denom = (size_z * size_c).max(1);
                size_t = (image_count / denom).max(1);
            } else {
                let denom = (size_z * size_t).max(1);
                size_c = (image_count / denom).max(1);
            }
            pic_files = pics;
        } else {
            image_count = npic;
            pic_files = vec![path.to_path_buf()];
        }

        // Java initFile appends every grouped PIC file to `used`. For a single
        // file `used` already holds it; for a multi-file group add the rest.
        for p in &pic_files {
            if !used.contains(p) {
                used.push(p.clone());
            }
        }

        // Java: effectiveSizeC LUTs are read, one colour table per channel,
        // from the first plane of each channel. We read from the lowest-indexed
        // contributing file. n_files passed to the note skip mirrors picFiles.
        let effective_size_c = size_c.max(1) as usize;
        let lut_n_files = if pic_files.len() > 1 {
            Some(pic_files.len() as u32)
        } else {
            None
        };
        let lut = if self.broken_notes {
            None
        } else if pic_files.len() > 1 {
            let mut tables = Vec::with_capacity(effective_size_c);
            for channel in 0..effective_size_c {
                // Java uses getIndex(0, channel, 0) with XYCTZ order; C is
                // the fastest-varying plane coordinate, so the channel's table
                // is read from picFiles[channel % nFiles].
                let file_idx = channel % pic_files.len();
                let table = File::open(&pic_files[file_idx]).ok().and_then(|mut lf| {
                    read_lookup_tables(&mut lf, 1, nx, ny, image_count, bpp, lut_n_files)
                });
                if let Some(mut one) = table {
                    if let Some(first) = one.pop() {
                        tables.push(first);
                        continue;
                    }
                }
                break;
            }
            if !tables.is_empty() {
                Some(tables)
            } else {
                None
            }
        } else {
            // Java reads each channel's table from picFiles[plane % nFiles];
            // for the common single-file case all tables live in this file.
            let lut_path = pic_files.first().cloned();
            lut_path.and_then(|lp| {
                File::open(&lp).ok().and_then(|mut lf| {
                    read_lookup_tables(
                        &mut lf,
                        effective_size_c,
                        nx,
                        ny,
                        image_count,
                        bpp,
                        lut_n_files,
                    )
                })
            })
        };
        // Java: m.indexed = lut != null. Surface channel 0's table as the
        // ImageMetadata LookupTable (the readable channel-0 LUT).
        let is_indexed = lut.is_some();
        let lookup_table =
            lut.as_ref()
                .and_then(|l| l.first())
                .map(|ramp| crate::common::metadata::LookupTable {
                    red: ramp[0].iter().map(|&b| b as u16).collect(),
                    green: ramp[1].iter().map(|&b| b as u16).collect(),
                    blue: ramp[2].iter().map(|&b| b as u16).collect(),
                });

        self.meta = Some(ImageMetadata {
            size_x: nx,
            size_y: ny,
            size_z,
            size_c,
            size_t,
            pixel_type,
            bits_per_pixel: (bpp * 8) as u8,
            image_count,
            dimension_order: DimensionOrder::XYCTZ,
            is_rgb: false,
            is_interleaved: false,
            is_indexed,
            is_little_endian: true,
            resolution_count: 1,
            thumbnail: false,
            series_metadata: meta_map,
            lookup_table,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        });
        self.npic = npic;
        self.bytes_per_pixel = bpp;
        self.pic_files = pic_files;
        self.lut = lut;
        self.last_channel = 0;
        self.note_strings = notes.notes;
        self.used = used;
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        // Mirrors Java close(false): reset every per-file field.
        self.path = None;
        self.meta = None;
        self.pic_files.clear();
        self.lut = None;
        self.last_channel = 0;
        self.broken_notes = false;
        self.note_strings.clear();
        self.offset.clear();
        self.gain.clear();
        self.used.clear();
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
        let (image_count, plane_bytes, size_c) = {
            let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
            (
                meta.image_count,
                (meta.size_x * meta.size_y) as usize * self.bytes_per_pixel,
                meta.size_c.max(1),
            )
        };
        if plane_index >= image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        // Java openBytes: lastChannel = getZCTCoords(no)[1]. Dimension order is
        // XYCTZ, so C is the fastest-varying coordinate after XY.
        self.last_channel = (plane_index % size_c) as usize;

        // Java openBytes: file = no % picFiles.length;
        // offset = (no / picFiles.length) * planeSize; then seek(offset + 76).
        // pic_files always holds >= 1 entry (the single source for one-file PICs).
        let n_files = self.pic_files.len().max(1) as u32;
        let file_idx = (plane_index % n_files) as usize;
        let local_plane = (plane_index / n_files) as u64;
        let path = self
            .pic_files
            .get(file_idx)
            .or_else(|| self.path.as_ref())
            .ok_or(BioFormatsError::NotInitialized)?;
        let offset = HEADER_SIZE + local_plane * plane_bytes as u64;
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(offset))
            .map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; plane_bytes];
        f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
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
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        crop_full_plane("Bio-Rad PIC", &full, meta, 1, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = ((meta.size_x - tw) / 2, (meta.size_y - th) / 2);
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        use crate::common::metadata::MetadataValue;
        use crate::common::ome_metadata::{OmeDetector, OmeInstrument, OmeMetadata, OmeObjective};
        let meta = self.meta.as_ref()?;
        let sm = &meta.series_metadata;
        let get_f = |key: &str| match sm.get(key) {
            Some(MetadataValue::Float(v)) => Some(*v),
            Some(MetadataValue::Int(v)) => Some(*v as f64),
            _ => None,
        };

        let mut ome = OmeMetadata::from_image_metadata(meta);
        let img = &mut ome.images[0];
        if let Some(MetadataValue::String(n)) = sm.get("name") {
            img.name = Some(n.clone());
        }
        // Physical sizes derived from AXIS / STRUCTURE notes.
        if let Some(v) = get_f("physical_size_x") {
            img.physical_size_x = Some(v);
        }
        if let Some(v) = get_f("physical_size_y") {
            img.physical_size_y = Some(v);
        }
        if let Some(v) = get_f("physical_size_z") {
            img.physical_size_z = Some(v);
        }

        // Instrument: Objective (model + nominal magnification) and Detectors
        // (per-channel gain/offset). Mirrors the Java MetadataStore wiring.
        let objective_model = match sm.get("objective.model") {
            Some(MetadataValue::String(s)) => Some(s.clone()),
            _ => None,
        };
        let objective_mag = get_f("objective.nominal_magnification");

        // Java initFile (lines ~512-528): for each effective channel, the
        // accumulated `offset`/`gain` lists drive a Detector element plus the
        // channel's DetectorSettings. Read directly from the struct fields.
        let mut detectors: Vec<OmeDetector> = Vec::new();
        let n_chan = meta.size_c.max(1) as usize;
        for i in 0..n_chan {
            let off = self.offset.get(i).copied().flatten();
            let gain = self.gain.get(i).copied().flatten();
            if off.is_some() || gain.is_some() {
                let detector_id = format!("Detector:0:{i}");
                detectors.push(OmeDetector {
                    id: Some(detector_id.clone()),
                    detector_type: Some("Other".into()),
                    gain,
                    offset: off,
                    ..Default::default()
                });
                // store.setDetectorSettingsGain/Offset(.., 0, i)
                if let Some(ch) = img.channels.get_mut(i) {
                    ch.detector_settings_gain = gain;
                    ch.detector_settings_offset = off;
                    ch.detector_ref = Some(detector_id);
                }
            }
        }

        if objective_model.is_some() || objective_mag.is_some() || !detectors.is_empty() {
            let mut instrument = OmeInstrument {
                id: Some("Instrument:0".into()),
                ..Default::default()
            };
            if objective_model.is_some() || objective_mag.is_some() {
                instrument.objectives.push(OmeObjective {
                    id: Some("Objective:0:0".into()),
                    model: objective_model,
                    nominal_magnification: objective_mag,
                    correction: Some("Other".into()),
                    immersion: Some("Other".into()),
                    ..Default::default()
                });
            }
            instrument.detectors = detectors;
            ome.instruments.push(instrument);
        }

        Some(ome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(p: &str) -> Note {
        note_typed(0, p)
    }

    fn note_typed(note_type: i16, p: &str) -> Note {
        Note {
            note_type,
            level: 0,
            num: 0,
            status: 0,
            x: 0,
            y: 0,
            p: p.to_string(),
        }
    }

    fn global_get<'a>(r: &'a ParseNotesResult, key: &str) -> Option<&'a str> {
        r.global_meta
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn parse_notes_axis_4_multichannel() {
        // AXIS_4 with axisType 11 -> single section multi-channel: sizeC=imageCount.
        let notes = vec![note("AXIS_4 11 0 0")];
        let r = parse_notes(&notes, 3, 0, 0);
        assert!(!r.multiple_files);
        assert_eq!(r.size_c, Some(3));
        assert_eq!(r.size_z, Some(1));
        assert_eq!(r.size_t, Some(1));
    }

    #[test]
    fn parse_notes_axis_9_multifile() {
        // AXIS_9 with axisType 11 -> multiple files, sizeC from values[3].
        let notes = vec![note("AXIS_9 11 1 2")];
        let r = parse_notes(&notes, 4, 0, 0);
        assert!(r.multiple_files);
        assert_eq!(r.size_c, Some(2));
    }

    #[test]
    fn parse_notes_ignores_non_axis_11() {
        // axisType != 11 should not affect dimensions.
        let notes = vec![note("AXIS_2 257 0 1.0")];
        let r = parse_notes(&notes, 5, 0, 0);
        assert!(!r.multiple_files);
        assert_eq!(r.size_c, None);
        assert_eq!(r.size_z, None);
        assert_eq!(r.size_t, None);
    }

    #[test]
    fn parse_notes_variable_objective_and_detector() {
        // NOTE_TYPE_VARIABLE (20) key=value records: objective magnification,
        // objective name, and a per-detector gain/offset SETTING.
        let notes = vec![
            note_typed(NOTE_TYPE_VARIABLE, "INFO_OBJECTIVE_NAME = Plan-Apo 60x"),
            note_typed(NOTE_TYPE_VARIABLE, "INFO_OBJECTIVE_MAGNIFICATION = 60"),
            note_typed(NOTE_TYPE_VARIABLE, "SETTING_DET_0_GAIN = 850"),
            note_typed(NOTE_TYPE_VARIABLE, "SETTING_DET_1_OFFSET = 12.5"),
        ];
        let r = parse_notes(&notes, 1, 512, 512);
        assert_eq!(r.objective_model.as_deref(), Some("Plan-Apo 60x"));
        assert_eq!(r.objective_magnification, Some(60.0));
        // First _DET_ note -> detector 0 gain; second -> detector 1 offset.
        assert_eq!(r.detectors.gain.first().copied().flatten(), Some(850.0));
        assert_eq!(r.detectors.offset.get(1).copied().flatten(), Some(12.5));
        // Free-form keys are preserved in global metadata.
        assert_eq!(global_get(&r, "INFO_OBJECTIVE_MAGNIFICATION"), Some("60"));
    }

    #[test]
    fn parse_notes_user_note_taxonomy() {
        // NOTE_TYPE_USER (4) -> a "Note" entry built from Note.toString().
        let notes = vec![note_typed(NOTE_TYPE_USER, "hello world")];
        let r = parse_notes(&notes, 1, 0, 0);
        let n = global_get(&r, "Note").expect("Note entry present");
        assert!(n.contains("type=USER"));
        assert!(n.contains("text=hello world"));
    }

    #[test]
    fn parse_notes_variable_axis_physical_size() {
        // VARIABLE AXIS_2 with type 257 yields physical size X in microns.
        let notes = vec![note_typed(NOTE_TYPE_VARIABLE, "AXIS_2 = 257 0 0.25")];
        let r = parse_notes(&notes, 1, 512, 512);
        assert_eq!(r.physical_size_x, Some(0.25));
    }

    #[test]
    fn byte_probe_accepts_input_sources_companion_header_like_java() {
        let reader = BioRadReader::new();
        let mut header = [0u8; 56];
        header[..15].copy_from_slice(b"[Input Sources]");
        assert!(reader.is_this_type_by_bytes(&header));
    }

    // -- Synthetic PIC builders for the new member-variable tests --

    /// Encode one 96-byte on-disk note record: 16-byte fixed header followed by
    /// an 80-byte (NUL-padded) text field. `more` sets the i32 "next note" flag.
    fn note_record(more: bool, note_type: i16, text: &str) -> Vec<u8> {
        let mut rec = Vec::new();
        rec.extend_from_slice(&0i16.to_le_bytes()); // level
        rec.extend_from_slice(&(if more { 1i32 } else { 0i32 }).to_le_bytes()); // notesFlag
        rec.extend_from_slice(&0i16.to_le_bytes()); // num
        rec.extend_from_slice(&0i16.to_le_bytes()); // status
        rec.extend_from_slice(&note_type.to_le_bytes()); // type
        rec.extend_from_slice(&0i16.to_le_bytes()); // x
        rec.extend_from_slice(&0i16.to_le_bytes()); // y
        let mut t = [0u8; 80];
        let bytes = text.as_bytes();
        t[..bytes.len().min(80)].copy_from_slice(&bytes[..bytes.len().min(80)]);
        rec.extend_from_slice(&t);
        rec
    }

    /// Build a 1x1, single-plane, 8-bit PIC file: 76-byte header + 1 pixel byte
    /// + the supplied trailing bytes (note block / LUT ramps).
    fn pic_1x1(trailing: &[u8]) -> Vec<u8> {
        pic_1x1_npic(&[0], trailing)
    }

    /// Build a 1x1, N-plane, 8-bit PIC file with explicit pixel bytes.
    fn pic_1x1_npic(pixels: &[u8], trailing: &[u8]) -> Vec<u8> {
        let mut data = vec![0u8; 76];
        data[0..2].copy_from_slice(&1i16.to_le_bytes()); // nx
        data[2..4].copy_from_slice(&1i16.to_le_bytes()); // ny
        data[4..6].copy_from_slice(&(pixels.len() as i16).to_le_bytes()); // npic
        data[14..16].copy_from_slice(&1i16.to_le_bytes()); // byte_format != 0 -> uint8
        data[54..56].copy_from_slice(&FILE_ID.to_le_bytes());
        data.extend_from_slice(pixels);
        data.extend_from_slice(trailing);
        data
    }

    fn tmp_pic(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(name);
        std::fs::write(&p, bytes).unwrap();
        p
    }

    fn tmp_dir(name: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("bioformats_biorad_{name}_{nanos}"));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn read_notes_flags_truncated_block_as_broken() {
        // A note whose notesFlag says "more follow" but no bytes remain: Java
        // sets brokenNotes when the file pointer reaches EOF.
        let trailing = note_record(true, NOTE_TYPE_USER, "hello");
        let bytes = pic_1x1(&trailing);
        let path = tmp_pic("biorad_broken_notes.pic", &bytes);
        let mut f = File::open(&path).unwrap();
        let r = read_notes(&mut f, 1, 1, 1, 1, None);
        assert_eq!(r.notes.len(), 1);
        assert!(r.broken_notes, "EOF after a 'more' note marks broken");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn read_lookup_tables_reads_single_channel_ramp() {
        // One terminating note, then a 3x256 colour ramp -> one channel LUT.
        let mut trailing = note_record(false, NOTE_TYPE_USER, "x");
        for c in 0..3u8 {
            trailing.extend(std::iter::repeat(c * 10).take(LUT_LENGTH));
        }
        let bytes = pic_1x1(&trailing);
        let path = tmp_pic("biorad_lut.pic", &bytes);
        let mut f = File::open(&path).unwrap();
        let lut = read_lookup_tables(&mut f, 1, 1, 1, 1, 1, None).expect("LUT present");
        assert_eq!(lut.len(), 1);
        assert_eq!(lut[0][0][0], 0);
        assert_eq!(lut[0][1][0], 10);
        assert_eq!(lut[0][2][0], 20);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn read_lookup_tables_returns_none_when_no_ramp() {
        // A terminating note with no trailing ramp bytes -> no LUT (lut = null).
        let trailing = note_record(false, NOTE_TYPE_USER, "x");
        let bytes = pic_1x1(&trailing);
        let path = tmp_pic("biorad_no_lut.pic", &bytes);
        let mut f = File::open(&path).unwrap();
        assert!(read_lookup_tables(&mut f, 1, 1, 1, 1, 1, None).is_none());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn set_id_populates_lut_used_and_notes() {
        // End-to-end: a PIC with a terminating note + one colour ramp should
        // populate the lut/last_channel/note_strings/used member variables and
        // mark the image indexed with a channel-0 LookupTable.
        let mut trailing = note_record(false, NOTE_TYPE_USER, "demo note");
        for c in 0..3u8 {
            trailing.extend(std::iter::repeat(c + 1).take(LUT_LENGTH));
        }
        let bytes = pic_1x1(&trailing);
        let path = tmp_pic("biorad_full.pic", &bytes);

        let mut reader = BioRadReader::new();
        reader.set_id(&path).unwrap();

        // noteStrings captured the single USER note.
        assert_eq!(reader.note_strings.len(), 1);
        assert_eq!(reader.note_strings[0].note_type, NOTE_TYPE_USER);
        // used lists the PIC file.
        assert_eq!(reader.used_files(), &[path.clone()]);
        // lut populated; eight_bit_lookup_table() returns channel 0's ramp.
        let lut = reader.eight_bit_lookup_table().expect("LUT present");
        assert_eq!(lut[0][0], 1);
        assert_eq!(lut[1][0], 2);
        assert_eq!(lut[2][0], 3);
        // metadata reflects indexed colour + channel-0 LookupTable.
        let meta = reader.metadata();
        assert!(meta.is_indexed);
        let table = meta.lookup_table.as_ref().expect("metadata LUT");
        assert_eq!(table.red[0], 1);
        assert_eq!(table.green[0], 2);
        assert_eq!(table.blue[0], 3);
        // last_channel updates on open_bytes.
        reader.open_bytes(0).unwrap();
        assert_eq!(reader.last_channel, 0);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn grouped_channel_luts_are_read_from_each_pic_file_like_java() {
        let dir = tmp_dir("channel_luts");
        let pic0 = dir.join("series001.pic");
        let pic1 = dir.join("series002.pic");

        let mut trailing0 = note_record(false, NOTE_TYPE_USER, "AXIS_9 11 0 2");
        for c in 0..3u8 {
            trailing0.extend(std::iter::repeat(c + 1).take(LUT_LENGTH));
        }
        let mut bytes0 = pic_1x1(&trailing0);
        bytes0[76] = 10;
        std::fs::write(&pic0, bytes0).unwrap();

        let mut trailing1 = note_record(false, NOTE_TYPE_USER, "AXIS_9 11 0 2");
        for c in 0..3u8 {
            trailing1.extend(std::iter::repeat(c + 11).take(LUT_LENGTH));
        }
        let mut bytes1 = pic_1x1(&trailing1);
        bytes1[76] = 20;
        std::fs::write(&pic1, bytes1).unwrap();

        let mut reader = BioRadReader::new();
        reader.set_id(&pic0).unwrap();
        assert_eq!(reader.metadata().size_c, 2);
        assert_eq!(reader.metadata().image_count, 2);
        assert_eq!(reader.used_files().len(), 2);

        assert_eq!(reader.open_bytes(0).unwrap(), vec![10]);
        let lut0 = reader.eight_bit_lookup_table().expect("channel 0 LUT");
        assert_eq!(lut0[0][0], 1);
        assert_eq!(lut0[1][0], 2);
        assert_eq!(lut0[2][0], 3);

        assert_eq!(reader.open_bytes(1).unwrap(), vec![20]);
        let lut1 = reader.eight_bit_lookup_table().expect("channel 1 LUT");
        assert_eq!(lut1[0][0], 11);
        assert_eq!(lut1[1][0], 12);
        assert_eq!(lut1[2][0], 13);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn lse_xml_companion_groups_sibling_pics_and_sets_dimensions_like_java() {
        let dir = tmp_dir("lse_group");
        let pic0 = dir.join("series001.pic");
        let pic1 = dir.join("series002.pic");
        let xml = dir.join("lse.xml");
        let raw = dir.join("data.raw");

        let mut bytes0 = pic_1x1(&note_record(false, NOTE_TYPE_USER, "x"));
        bytes0[76] = 31;
        std::fs::write(&pic0, bytes0).unwrap();
        let mut bytes1 = pic_1x1(&note_record(false, NOTE_TYPE_USER, "x"));
        bytes1[76] = 47;
        std::fs::write(&pic1, bytes1).unwrap();
        std::fs::write(
            &xml,
            r#"<Root><Pixels SizeZ="1" SizeC="2" SizeT="1"/><C TimeCompleted="12.5"/><T TimeCompleted="13.5"/></Root>"#,
        )
        .unwrap();
        std::fs::write(&raw, b"raw companion").unwrap();

        let mut reader = BioRadReader::new();
        reader.set_id(&xml).unwrap();
        let meta = reader.metadata();
        assert_eq!(meta.size_z, 1);
        assert_eq!(meta.size_c, 2);
        assert_eq!(meta.size_t, 1);
        assert_eq!(meta.image_count, 2);
        assert!(matches!(
            meta.series_metadata.get("Timestamp"),
            Some(MetadataValue::String(v)) if v == "12.5"
        ));
        assert!(matches!(
            meta.series_metadata.get("Timestamp #2"),
            Some(MetadataValue::String(v)) if v == "13.5"
        ));

        assert_eq!(reader.open_bytes(0).unwrap(), vec![31]);
        assert_eq!(reader.open_bytes(1).unwrap(), vec![47]);
        assert!(reader.used_files().contains(&xml));
        assert!(reader.used_files().contains(&raw));
        assert!(reader.used_files().contains(&pic0));
        assert!(reader.used_files().contains(&pic1));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn axis_notes_override_lse_xml_dimensions_like_java_parse_order() {
        let dir = tmp_dir("lse_axis_order");
        let pic = dir.join("series001.pic");
        let xml = dir.join("lse.xml");

        let trailing = note_record(false, NOTE_TYPE_USER, "AXIS_4 11 0 0");
        std::fs::write(&pic, pic_1x1_npic(&[3, 4], &trailing)).unwrap();
        std::fs::write(
            &xml,
            r#"<Root><Pixels SizeZ="1" SizeC="1" SizeT="2"/></Root>"#,
        )
        .unwrap();

        let mut reader = BioRadReader::new();
        reader.set_id(&xml).unwrap();
        let meta = reader.metadata();
        assert_eq!(meta.size_z, 1);
        assert_eq!(meta.size_c, 2);
        assert_eq!(meta.size_t, 1);
        assert_eq!(meta.image_count, 2);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![3]);
        assert_eq!(reader.open_bytes(1).unwrap(), vec![4]);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn ome_metadata_surfaces_detector_settings_from_offset_gain() {
        use crate::common::reader::FormatReader;
        // STRUCTURE case 11 fills offset/gain for detectors 0..2 in lockstep;
        // ome_metadata projects them onto each channel's DetectorSettings.
        // (Mirrors Java parseNotes STRUCTURE-11 + initFile DetectorSettings.)
        let mut detectors = DetectorSettings::default();
        set_detector_value(&mut detectors.offset, 0, Some(5.0));
        set_detector_value(&mut detectors.gain, 0, Some(712.0));
        // Drive the reader's member lists directly (the parse path that fills
        // them is covered by parse_notes_variable_objective_and_detector).
        let bytes = pic_1x1(&note_record(false, NOTE_TYPE_USER, "x"));
        let path = tmp_pic("biorad_detset.pic", &bytes);
        let mut reader = BioRadReader::new();
        reader.set_id(&path).unwrap();
        reader.offset = detectors.offset.clone();
        reader.gain = detectors.gain.clone();

        // Member lists populated.
        assert_eq!(reader.gain.first().copied().flatten(), Some(712.0));
        assert_eq!(reader.offset.first().copied().flatten(), Some(5.0));

        let ome = reader.ome_metadata().expect("OME metadata");
        let ch = &ome.images[0].channels[0];
        assert_eq!(ch.detector_settings_gain, Some(712.0));
        assert_eq!(ch.detector_settings_offset, Some(5.0));
        assert_eq!(ch.detector_ref.as_deref(), Some("Detector:0:0"));
        let inst = &ome.instruments[0];
        assert_eq!(inst.detectors[0].gain, Some(712.0));
        assert_eq!(inst.detectors[0].offset, Some(5.0));

        let _ = std::fs::remove_file(path);
    }
}
