//! PerkinElmer format readers.
//!
//! - PerkinElmerReader: UltraVIEW spinning disk (.cfg + .rec)
//! - OpenlabRawReader: Openlab Raw (.raw) with "LBLB" magic
//! - PhotonDynamicsReader: Photon Dynamics (.pds) extension-only

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;

// ── Helpers ──────────────────────────────────────────────────────────────────

fn default_meta(w: u32, h: u32, pt: PixelType) -> ImageMetadata {
    let bps = pt.bytes_per_sample();
    ImageMetadata {
        size_x: w,
        size_y: h,
        size_z: 1,
        size_c: 1,
        size_t: 1,
        pixel_type: pt,
        bits_per_pixel: (bps * 8) as u8,
        image_count: 1,
        dimension_order: DimensionOrder::XYZCT,
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
    }
}

fn open_bytes_impl(
    path: &Path,
    offset: u64,
    meta: &ImageMetadata,
    plane_index: u32,
) -> Result<Vec<u8>> {
    if plane_index != 0 {
        return Err(BioFormatsError::PlaneOutOfRange(plane_index));
    }
    let bps = meta.pixel_type.bytes_per_sample();
    let plane_bytes = meta.size_x as usize * meta.size_y as usize * bps;
    let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
    f.seek(SeekFrom::Start(offset))
        .map_err(BioFormatsError::Io)?;
    let mut buf = vec![0u8; plane_bytes];
    f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
    Ok(buf)
}

fn region_from_full(
    full: &[u8],
    meta: &ImageMetadata,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
) -> Result<Vec<u8>> {
    crop_full_plane("PerkinElmer/OpenLab", full, meta, 1, x, y, w, h)
}

// ── PerkinElmerReader ─────────────────────────────────────────────────────────
//
// Ported from the upstream Java PerkinElmerReader. A PerkinElmer dataset is a
// directory containing one `.htm` file, several metadata companions (.tim,
// .csv, .zpo, .cfg, .ano, .rec) and a set of pixel files which are either
// TIFFs or raw binaries numbered by extension (.2, .3, .4, …) with a 6-byte
// header. Wavelengths/Frames/Slices map to C/T/Z.

/// A single pixel file (TIFF or raw numbered binary).
#[derive(Clone)]
struct PixelsFile {
    path: PathBuf,
    /// Sequence index parsed from a `_NNN` suffix, or -1 when absent.
    first_index: i32,
    /// File extension index (the numeric extension for raw, 0 for TIFF).
    ext_index: i32,
}

pub struct PerkinElmerReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    files: Vec<PixelsFile>,
    ext_count: usize,
    is_tiff: bool,
    tiff_reader: crate::tiff::TiffReader,
    tiff_loaded: bool,

    // -- Data fields mirroring the Java PerkinElmerReader --
    /// "Experiment details:" string (Wavelengths/Frames/Slices). Java `details`.
    details: Option<String>,
    /// "Z slice space" string. Java `sliceSpace`.
    slice_space: Option<String>,
    /// "Pixel Size X"/"Pixel Size Y" in microns. Java `pixelSizeX`/`pixelSizeY`.
    pixel_size_x: f64,
    pixel_size_y: f64,
    /// "Start Time:"/"Finish Time:" raw strings. Java `startTime`/`finishTime`.
    start_time: Option<String>,
    finish_time: Option<String>,
    /// "Origin X/Y/Z" stage positions. Java `originX`/`originY`/`originZ`.
    origin_x: f64,
    origin_y: f64,
    origin_z: f64,
}

impl PerkinElmerReader {
    pub fn new() -> Self {
        PerkinElmerReader {
            path: None,
            meta: None,
            files: Vec::new(),
            ext_count: 1,
            is_tiff: true,
            tiff_reader: crate::tiff::TiffReader::new(),
            tiff_loaded: false,
            details: None,
            slice_space: None,
            pixel_size_x: 1.0,
            pixel_size_y: 1.0,
            start_time: None,
            finish_time: None,
            origin_x: 0.0,
            origin_y: 0.0,
            origin_z: 0.0,
        }
    }
}

impl Default for PerkinElmerReader {
    fn default() -> Self {
        Self::new()
    }
}

fn has_ext(name: &str, ext: &str) -> bool {
    name.rsplit('.')
        .next()
        .map(|e| e.eq_ignore_ascii_case(ext))
        .unwrap_or(false)
}

fn is_tiff_name(name: &str) -> bool {
    has_ext(name, "tif") || has_ext(name, "tiff")
}

/// Result of parsing the metadata companion files.
struct PeMeta {
    size_x: u32,
    size_y: u32,
    size_z: u32,
    size_c: u32,
    size_t: u32,
    details: Option<String>,
    slice_space: Option<String>,
    pixel_size_x: f64,
    pixel_size_y: f64,
    start_time: Option<String>,
    finish_time: Option<String>,
    origin_x: f64,
    origin_y: f64,
    origin_z: f64,
    metadata: HashMap<String, MetadataValue>,
}

impl Default for PeMeta {
    fn default() -> Self {
        // pixelSize defaults to 1, origins to 0 — matches the Java field inits.
        PeMeta {
            size_x: 0,
            size_y: 0,
            size_z: 0,
            size_c: 0,
            size_t: 0,
            details: None,
            slice_space: None,
            pixel_size_x: 1.0,
            pixel_size_y: 1.0,
            start_time: None,
            finish_time: None,
            origin_x: 0.0,
            origin_y: 0.0,
            origin_z: 0.0,
            metadata: HashMap::new(),
        }
    }
}

/// One Java function: `parseKeyValue`. Records every key as global metadata and
/// fills the typed dimension/physical/timing fields.
fn pe_parse_key_value(m: &mut PeMeta, key: &str, value: &str) {
    m.metadata
        .insert(key.to_string(), MetadataValue::String(value.to_string()));
    match key {
        "Image Width" => {
            if let Ok(v) = value.trim().parse() {
                m.size_x = v;
            }
        }
        "Image Length" => {
            if let Ok(v) = value.trim().parse() {
                m.size_y = v;
            }
        }
        "Number of slices" => {
            if let Ok(v) = value.trim().parse() {
                m.size_z = v;
            }
        }
        "Experiment details:" => m.details = Some(value.to_string()),
        "Z slice space" => m.slice_space = Some(value.to_string()),
        "Pixel Size X" => {
            if let Ok(v) = value.trim().parse() {
                m.pixel_size_x = v;
            }
        }
        "Pixel Size Y" => {
            if let Ok(v) = value.trim().parse() {
                m.pixel_size_y = v;
            }
        }
        "Finish Time:" => m.finish_time = Some(value.to_string()),
        "Start Time:" => m.start_time = Some(value.to_string()),
        "Origin X" => {
            if let Ok(v) = value.trim().parse() {
                m.origin_x = v;
            }
        }
        "Origin Y" => {
            if let Ok(v) = value.trim().parse() {
                m.origin_y = v;
            }
        }
        "Origin Z" => {
            if let Ok(v) = value.trim().parse() {
                m.origin_z = v;
            }
        }
        _ => {}
    }
}

/// Parse a `.tim` file: whitespace-separated tokens mapped to known keys
/// (mirrors Java parseTimFile).
fn pe_parse_tim(m: &mut PeMeta, content: &str) {
    let hash_keys = [
        "Number of Wavelengths/Timepoints",
        "Zero 1",
        "Zero 2",
        "Number of slices",
        "Extra int",
        "Calibration Unit",
        "Pixel Size Y",
        "Pixel Size X",
        "Image Width",
        "Image Length",
        "Origin X",
        "SubfileType X",
        "Dimension Label X",
        "Origin Y",
        "SubfileType Y",
        "Dimension Label Y",
        "Origin Z",
        "SubfileType Z",
        "Dimension Label Z",
    ];
    let mut t_num = 0usize;
    for token in content.split_whitespace() {
        if token.trim().is_empty() {
            continue;
        }
        if t_num >= hash_keys.len() {
            break;
        }
        if token == "um" {
            t_num = 5;
        }
        while (t_num == 1 || t_num == 2) && token.trim() != "0" {
            t_num += 1;
        }
        if t_num == 4 && token.parse::<i64>().is_err() {
            t_num += 1;
        }
        if t_num < hash_keys.len() {
            pe_parse_key_value(m, hash_keys[t_num], token);
            t_num += 1;
        }
    }
}

/// Parse the `.htm` header, which defines the Experiment details (Wavelengths,
/// Frames, Slices). Tokens are split on tags/whitespace.
fn pe_parse_htm(m: &mut PeMeta, content: &str) {
    // Split on HTML tags and surrounding whitespace, similar to Java's
    // HTML_REGEX. Tokens containing '<' are blanked.
    let mut tokens: Vec<String> = Vec::new();
    for part in content.split(|c| c == '<' || c == '>') {
        let trimmed = part.trim();
        tokens.push(trimmed.to_string());
    }
    let mut j = 0;
    while j + 1 < tokens.len() {
        let key = tokens[j].trim().to_string();
        let value = tokens[j + 1].trim().to_string();
        if !key.is_empty() {
            pe_parse_key_value(m, &key, &value);
        }
        j += 2;
    }
}

/// One Java function: `parseCSVFile`. Whitespace-split tokens with a positional
/// state machine that picks out Calibration Unit / Pixel Size X / Pixel Size Y /
/// Z slice space, then `key+key`/`value` triplets.
fn pe_parse_csv(m: &mut PeMeta, content: &str) {
    let tokens: Vec<&str> = content
        .split_whitespace()
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .collect();

    let hash_keys = [
        "Calibration Unit",
        "Pixel Size X",
        "Pixel Size Y",
        "Z slice space",
    ];
    let mut t_num = 0usize;
    let mut pt = 0usize;
    let mut j = 0usize;
    while j < tokens.len() {
        let key: Option<String>;
        let value: Option<String>;
        if t_num < 7 {
            j += 1;
            key = None;
            value = None;
        } else if (t_num > 7 && t_num < 12) || (t_num > 12 && t_num < 18) || (t_num > 18 && t_num < 22)
        {
            j += 1;
            key = None;
            value = None;
        } else if pt < hash_keys.len() {
            key = Some(hash_keys[pt].to_string());
            pt += 1;
            value = Some(tokens[j].to_string());
            j += 1;
        } else {
            // key = tokens[j++] + tokens[j++]; value = tokens[j++];
            if j + 2 >= tokens.len() {
                break;
            }
            let k = format!("{}{}", tokens[j], tokens[j + 1]);
            j += 2;
            let v = tokens[j].to_string();
            j += 1;
            key = Some(k);
            value = Some(v);
        }

        if let (Some(k), Some(v)) = (key, value) {
            pe_parse_key_value(m, &k, &v);
        }
        t_num += 1;
    }
}

/// One Java function: `parseZpoFile`. Each whitespace token becomes an indexed
/// "Z slice position" global metadata entry (mirrors `addGlobalMetaList`).
fn pe_parse_zpo(m: &mut PeMeta, content: &str) {
    let mut n = 1usize;
    for token in content.split_whitespace() {
        m.metadata.insert(
            format!("Z slice position #{n}"),
            MetadataValue::String(token.to_string()),
        );
        n += 1;
    }
}

/// Convert a PerkinElmer `Finish Time:` string formatted as DATE_FORMAT
/// (`HH:mm:ss (MM/dd/yyyy)`) into OME ISO8601 (`yyyy-MM-ddTHH:mm:ss`), mirroring
/// Java's `Timestamp.valueOf(DateTools.formatDate(finishTime, DATE_FORMAT))`.
/// Returns `None` when the string does not match, matching Java's null result.
fn pe_finish_time_to_iso8601(s: &str) -> Option<String> {
    let s = s.trim();
    // Expect "HH:mm:ss (MM/dd/yyyy)".
    let (time_part, rest) = s.split_once(' ')?;
    let date_part = rest.trim().trim_start_matches('(').trim_end_matches(')');
    let mut tparts = time_part.split(':');
    let hh = tparts.next()?;
    let mm = tparts.next()?;
    let ss = tparts.next()?;
    if tparts.next().is_some() {
        return None;
    }
    let mut dparts = date_part.split('/');
    let month = dparts.next()?;
    let day = dparts.next()?;
    let year = dparts.next()?;
    if dparts.next().is_some() {
        return None;
    }
    // Validate numerics so a malformed header yields None (Java returns null).
    for p in [hh, mm, ss, month, day, year] {
        if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
    }
    Some(format!(
        "{:0>4}-{:0>2}-{:0>2}T{:0>2}:{:0>2}:{:0>2}",
        year, month, day, hh, mm, ss
    ))
}

/// Convert an ISO8601 `yyyy-MM-ddTHH:mm:ss` string into Unix epoch
/// milliseconds (UTC). Mirrors Java's `DateTools.getTime`, used only to compute
/// the relative per-plane DeltaT spacing, so absolute timezone is irrelevant.
fn pe_iso8601_to_unix_millis(iso: &str) -> Option<i64> {
    let (date, time) = iso.split_once('T')?;
    let mut d = date.split('-');
    let year: i64 = d.next()?.parse().ok()?;
    let month: i64 = d.next()?.parse().ok()?;
    let day: i64 = d.next()?.parse().ok()?;
    let mut t = time.split(':');
    let hh: i64 = t.next()?.parse().ok()?;
    let mm: i64 = t.next()?.parse().ok()?;
    let ss: i64 = t.next()?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // Days since 1970-01-01 via a civil-date algorithm (Howard Hinnant).
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(((days * 86400 + hh * 3600 + mm * 60 + ss) * 1000) as i64)
}

fn parse_pe_dataset(id: &Path) -> Result<(PeMeta, Vec<PixelsFile>, usize, bool)> {
    // Always initialise from the .htm file; locate it if id is something else.
    let dir = id.parent().unwrap_or(Path::new(".")).to_path_buf();
    let mut htm_id = id.to_path_buf();
    if !id
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("htm") || e.eq_ignore_ascii_case("html"))
        .unwrap_or(false)
    {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for ent in entries.flatten() {
                let name = ent.file_name().to_string_lossy().to_string();
                if (has_ext(&name, "htm") || has_ext(&name, "html")) && !name.starts_with('.') {
                    htm_id = dir.join(&name);
                    break;
                }
            }
        }
    }

    // Prefix used for matching companion files.
    let check = htm_id
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();

    // List + sort directory entries.
    let mut entries: Vec<String> = std::fs::read_dir(&dir)
        .map_err(BioFormatsError::Io)?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    entries.sort();

    let mut tim_file: Option<PathBuf> = None;
    let mut csv_file: Option<PathBuf> = None;
    let mut zpo_file: Option<PathBuf> = None;
    let mut htm_file: Option<PathBuf> = None;
    let mut temp_files: Vec<PixelsFile> = Vec::new();
    let mut is_tiff = true;
    let mut prefix: Option<String> = None;

    for (dir_index, name) in entries.iter().enumerate() {
        let dot = name.rfind('.');
        let stem = match dot {
            Some(d) => &name[..d],
            None => name.as_str(),
        };
        let matches = stem.starts_with(&check)
            || check.starts_with(stem)
            || prefix
                .as_deref()
                .map(|p| stem.starts_with(p))
                .unwrap_or(false);
        if !matches {
            continue;
        }
        if let Some(d) = dot {
            prefix = Some(name[..d].to_string());
        }
        if tim_file.is_none() && has_ext(name, "tim") {
            tim_file = Some(dir.join(name));
        }
        if csv_file.is_none() && has_ext(name, "csv") {
            csv_file = Some(dir.join(name));
        }
        if zpo_file.is_none() && has_ext(name, "zpo") {
            zpo_file = Some(dir.join(name));
        }
        if htm_file.is_none() && (has_ext(name, "htm") || has_ext(name, "html")) {
            htm_file = Some(dir.join(name));
        }

        let dot_pos = match dot {
            Some(d) => d,
            None => continue,
        };
        let path = dir.join(name);
        let bytes = name.as_bytes();
        if is_tiff_name(name) {
            // _NNN before the extension -> firstIndex; _NNNN_NNN -> extIndex
            let first_index = if dot_pos >= 4 && bytes[dot_pos - 4] == b'_' {
                name[dot_pos - 3..dot_pos].parse::<i32>().unwrap_or(-1)
            } else {
                -1
            };
            let (first_index, ext_index) = if dot_pos >= 9 && bytes[dot_pos - 9] == b'_' {
                (
                    first_index,
                    name[dot_pos - 8..dot_pos - 4].parse::<i32>().unwrap_or(0),
                )
            } else {
                // Java PerkinElmerReader.java:386 uses `i`, the index into the
                // full sorted directory listing (companions included), not the
                // count of pixel files collected so far.
                (dir_index as i32, 0)
            };
            temp_files.push(PixelsFile {
                path,
                first_index,
                ext_index,
            });
        } else {
            // raw numbered binary: extension is a hex number
            let ext = if dot_pos + 1 < name.len() {
                &name[dot_pos + 1..]
            } else {
                ""
            };
            if let Ok(ext_index) = i32::from_str_radix(ext, 16) {
                let first_index = if dot_pos >= 4 && bytes[dot_pos - 4] == b'_' {
                    name[dot_pos - 3..dot_pos].parse::<i32>().unwrap_or(-1)
                } else {
                    -1
                };
                is_tiff = false;
                temp_files.push(PixelsFile {
                    path,
                    first_index,
                    ext_index,
                });
            }
        }
    }

    // Count distinct extension indices.
    let mut found_exts: Vec<i32> = Vec::new();
    for f in &temp_files {
        if !found_exts.contains(&f.ext_index) {
            found_exts.push(f.ext_index);
        }
    }
    let ext_count = found_exts.len().max(1);

    // Parse metadata. Java order: .tim, then .csv (or .zpo if no .csv), then
    // the aggressive .htm pass that defines wavelength/timepoint counts.
    let mut m = PeMeta::default();
    if let Some(tf) = &tim_file {
        if let Ok(content) = std::fs::read_to_string(tf) {
            pe_parse_tim(&mut m, &content);
        }
    }
    if let Some(cf) = &csv_file {
        if let Ok(content) = std::fs::read_to_string(cf) {
            pe_parse_csv(&mut m, &content);
        }
    } else if let Some(zf) = &zpo_file {
        if let Ok(content) = std::fs::read_to_string(zf) {
            pe_parse_zpo(&mut m, &content);
        }
    }
    let htm = htm_file.clone().unwrap_or(htm_id);
    if let Ok(content) = std::fs::read_to_string(&htm) {
        pe_parse_htm(&mut m, &content);
    } else {
        return Err(BioFormatsError::UnsupportedFormat(
            "PerkinElmer: valid .htm header file not found".into(),
        ));
    }

    // Parse experiment details for Wavelengths/Frames/Slices.
    if let Some(details) = m.details.clone() {
        let mut n = 0u32;
        for token in details.split_whitespace() {
            match token {
                "Wavelengths" => m.size_c = n,
                "Frames" => m.size_t = n,
                "Slices" => m.size_z = n,
                _ => {}
            }
            n = token.parse::<u32>().unwrap_or(0);
        }
    }

    if temp_files.is_empty() {
        return Err(BioFormatsError::UnsupportedFormat(
            "PerkinElmer: no pixel files found".into(),
        ));
    }

    Ok((m, temp_files, ext_count, is_tiff))
}

impl PerkinElmerReader {
    /// Locate the PixelsFile for the given plane, mirroring Java lookupFile.
    fn lookup_file(&self, no: u32) -> Option<&PixelsFile> {
        let no = no as i32;
        let mut min_ext = i32::MAX;
        let mut min_first = i32::MAX;
        for f in &self.files {
            if f.ext_index < min_ext {
                min_ext = f.ext_index;
            }
            if f.first_index >= 0 && f.first_index < min_first {
                min_first = f.first_index;
            }
        }
        let ext_count = self.ext_count as i32;
        for ext in min_ext..=ext_count + min_ext {
            for f in &self.files {
                if f.ext_index == ext {
                    if f.first_index < 0 {
                        if no % ext_count == ext - min_ext {
                            return Some(f);
                        }
                    } else if no == (f.first_index - min_first) * ext_count + ext - min_ext {
                        return Some(f);
                    }
                }
            }
        }
        None
    }

    fn file_index(&self, no: u32) -> u32 {
        match self.lookup_file(no) {
            Some(f) if f.first_index >= 0 => 0,
            _ => no / self.ext_count as u32,
        }
    }

    /// "Experiment details:" string (Java `details`). Also available as the
    /// `Experiment details:` global metadata key.
    pub fn details(&self) -> Option<&str> {
        self.details.as_deref()
    }

    /// "Z slice space" string (Java `sliceSpace`). Also available as the
    /// `Z slice space` global metadata key.
    pub fn slice_space(&self) -> Option<&str> {
        self.slice_space.as_deref()
    }
}

impl FormatReader for PerkinElmerReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        match ext.as_deref() {
            Some("htm") | Some("html") => true,
            // A companion file is acceptable if a sibling .htm exists.
            Some("tim") | Some("csv") | Some("zpo") | Some("cfg") | Some("ano") | Some("rec") => {
                let dir = path.parent().unwrap_or(Path::new("."));
                std::fs::read_dir(dir)
                    .map(|entries| {
                        entries.flatten().any(|e| {
                            let n = e.file_name().to_string_lossy().to_string();
                            has_ext(&n, "htm") || has_ext(&n, "html")
                        })
                    })
                    .unwrap_or(false)
            }
            _ => false,
        }
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let (m, files, mut ext_count, is_tiff) = parse_pe_dataset(path)?;

        // Determine pixel type and (for raw files) sizeX/sizeY from the data.
        let mut size_z = if m.size_z == 0 { 1 } else { m.size_z };
        let mut size_c = if m.size_c == 0 { 1 } else { m.size_c };
        let mut size_x = m.size_x.max(1);
        let mut size_y = m.size_y.max(1);
        let pixel_type;
        let mut little_endian = true;
        let mut is_rgb = false;

        let first_path = files[0].path.clone();
        if is_tiff {
            self.tiff_reader.set_id(&first_path)?;
            let tm = self.tiff_reader.metadata();
            size_x = tm.size_x;
            size_y = tm.size_y;
            pixel_type = tm.pixel_type;
            little_endian = tm.is_little_endian;
            is_rgb = tm.is_rgb;
            let _ = self.tiff_reader.close();
        } else {
            let flen = std::fs::metadata(&first_path)
                .map_err(BioFormatsError::Io)?
                .len();
            let area = (size_x as u64 * size_y as u64).max(1);
            let mut bpp = ((flen.saturating_sub(6)) / area) as u32;
            if bpp % 3 == 0 && bpp > 0 {
                bpp /= 3;
            }
            pixel_type = match bpp {
                1 => PixelType::Uint8,
                2 => PixelType::Uint16,
                4 => PixelType::Uint32,
                _ => PixelType::Uint16,
            };
        }

        // imageCount: one per pixel file, plus expansion for raw files that hold
        // multiple concatenated planes (Java PerkinElmerReader.java:435-442).
        let mut image_count = 0u32;
        for f in &files {
            image_count += 1;
            if f.first_index < 0 && ext_count > 1 && files.len() > ext_count {
                image_count += (((files.len() - 1) / (ext_count - 1)) - 1) as u32;
            }
        }

        // sizeT derivation (Java logic).
        let zc = (size_z * size_c).max(1);
        let mut size_t = if m.size_t == 0 || image_count % zc == 0 {
            (image_count / zc).max(1)
        } else {
            image_count = (size_z * size_c * m.size_t).min(files.len() as u32);
            (image_count / zc).max(1)
        };
        if size_t == 0 {
            size_t = 1;
        }
        if image_count != size_z * size_c * size_t {
            image_count = size_z * size_c * size_t;
        }
        let _ = (&mut size_z, &mut size_c);

        // For raw (non-TIFF) multi-wavelength data, correct extCount so the
        // plane->file/offset mapping in lookup_file/file_index is right
        // (Java PerkinElmerReader.java:595-597).
        if !is_tiff && ext_count > size_t as usize {
            ext_count = (size_t * size_c) as usize;
        }

        // Mirror the Java data fields on the reader. They are surfaced into the
        // OME projection by `ome_metadata()` below, exactly where Java's
        // initFile populates the MetadataStore.
        self.details = m.details.clone();
        self.slice_space = m.slice_space.clone();
        self.pixel_size_x = m.pixel_size_x;
        self.pixel_size_y = m.pixel_size_y;
        self.start_time = m.start_time.clone();
        self.finish_time = m.finish_time.clone();
        self.origin_x = m.origin_x;
        self.origin_y = m.origin_y;
        self.origin_z = m.origin_z;

        let meta = ImageMetadata {
            size_x,
            size_y,
            size_z,
            size_c,
            size_t,
            pixel_type,
            bits_per_pixel: (pixel_type.bytes_per_sample() * 8) as u8,
            image_count,
            dimension_order: DimensionOrder::XYCTZ,
            is_rgb,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: little_endian,
            resolution_count: 1,
            thumbnail: false,
            series_metadata: m.metadata,
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        };

        self.path = Some(path.to_path_buf());
        self.meta = Some(meta);
        self.files = files;
        self.ext_count = ext_count;
        self.is_tiff = is_tiff;
        self.tiff_loaded = false;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.files.clear();
        self.ext_count = 1;
        // Reset data fields to their constructor defaults (Java close()).
        self.details = None;
        self.slice_space = None;
        self.pixel_size_x = 1.0;
        self.pixel_size_y = 1.0;
        self.start_time = None;
        self.finish_time = None;
        self.origin_x = 0.0;
        self.origin_y = 0.0;
        self.origin_z = 0.0;
        if self.tiff_loaded {
            let _ = self.tiff_reader.close();
            self.tiff_loaded = false;
        }
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
        self.meta
            .as_ref()
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let bps = meta.pixel_type.bytes_per_sample();
        let plane_bytes = meta.size_x as usize
            * meta.size_y as usize
            * bps
            * if meta.is_rgb { meta.size_c as usize } else { 1 };

        let file = self
            .lookup_file(plane_index)
            .ok_or(BioFormatsError::PlaneOutOfRange(plane_index))?
            .clone();
        let index = self.file_index(plane_index);

        if self.is_tiff {
            if self.tiff_loaded {
                let _ = self.tiff_reader.close();
            }
            self.tiff_reader.set_id(&file.path)?;
            self.tiff_loaded = true;
            return self.tiff_reader.open_bytes(index);
        }

        // raw binary with a 6-byte header per file, planes are concatenated.
        let mut buf = vec![0u8; plane_bytes];
        let offset = 6u64 + index as u64 * plane_bytes as u64;
        let mut f = std::fs::File::open(&file.path).map_err(BioFormatsError::Io)?;
        let len = f.metadata().map_err(BioFormatsError::Io)?.len();
        let end = offset.checked_add(plane_bytes as u64).ok_or_else(|| {
            BioFormatsError::InvalidData("PerkinElmer plane offset overflows".into())
        })?;
        if end > len {
            return Err(BioFormatsError::InvalidData(format!(
                "PerkinElmer raw plane {plane_index} exceeds file length: need bytes {offset}..{end}, file length {len}"
            )));
        }
        f.seek(SeekFrom::Start(offset))
            .map_err(BioFormatsError::Io)?;
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
        let meta = self.meta.as_ref().unwrap();
        region_from_full(&full, meta, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        use crate::common::ome_metadata::{OmeMetadata, OmePlane};
        let meta = self.meta.as_ref()?;
        let mut ome = OmeMetadata::from_image_metadata(meta);
        let img = ome.images.get_mut(0)?;

        // finishTime -> Image AcquisitionDate (Java initFile, line 606-610).
        if let Some(date) = self
            .finish_time
            .as_deref()
            .and_then(pe_finish_time_to_iso8601)
        {
            img.acquisition_date = Some(date);
        }

        // pixelSizeX/Y -> Pixels PhysicalSizeX/Y (Java initFile, line 614-621).
        if self.pixel_size_x.is_finite() && self.pixel_size_x > 0.0 {
            img.physical_size_x = Some(self.pixel_size_x);
        }
        if self.pixel_size_y.is_finite() && self.pixel_size_y > 0.0 {
            img.physical_size_y = Some(self.pixel_size_y);
        }

        // start/finish time -> per-plane DeltaT (Java initFile, line 647-659):
        // secondsPerPlane = (end - start) / imageCount; planeDeltaT = i * spp.
        let start = self
            .start_time
            .as_deref()
            .and_then(pe_finish_time_to_iso8601)
            .and_then(|iso| pe_iso8601_to_unix_millis(&iso));
        let end = self
            .finish_time
            .as_deref()
            .and_then(pe_finish_time_to_iso8601)
            .and_then(|iso| pe_iso8601_to_unix_millis(&iso));
        let seconds_per_plane = match (start, end) {
            (Some(s), Some(e)) if meta.image_count > 0 => {
                Some((e - s) as f64 / meta.image_count as f64 / 1000.0)
            }
            _ => None,
        };

        // originX/Y/Z -> per-plane StagePosition (Java initFile, line 665-677).
        // Java writes the same origin onto every plane.
        let has_origin = self.origin_x != 0.0 || self.origin_y != 0.0 || self.origin_z != 0.0;
        if has_origin || seconds_per_plane.is_some() {
            let c_size = meta.size_c.max(1);
            let z_size = meta.size_z.max(1);
            if img.planes.is_empty() {
                for i in 0..meta.image_count {
                    img.planes.push(OmePlane {
                        the_z: (i / c_size) % z_size,
                        the_c: i % c_size,
                        the_t: i / (c_size * z_size),
                        ..Default::default()
                    });
                }
            }
            for (i, plane) in img.planes.iter_mut().enumerate() {
                if has_origin {
                    plane.position_x = Some(self.origin_x);
                    plane.position_y = Some(self.origin_y);
                    plane.position_z = Some(self.origin_z);
                }
                if let Some(spp) = seconds_per_plane {
                    plane.delta_t = Some(i as f64 * spp);
                }
            }
        }

        Some(ome)
    }
}

// ── OpenlabRawReader ──────────────────────────────────────────────────────────

const OPENLAB_MAGIC: &[u8] = b"LBLB";
const OPENLAB_HEADER_SIZE: u64 = 288;

pub struct OpenlabRawReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl OpenlabRawReader {
    pub fn new() -> Self {
        OpenlabRawReader {
            path: None,
            meta: None,
        }
    }
}

impl Default for OpenlabRawReader {
    fn default() -> Self {
        Self::new()
    }
}

fn parse_openlab(path: &Path) -> Result<ImageMetadata> {
    let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
    if data.len() < OPENLAB_HEADER_SIZE as usize {
        return Err(BioFormatsError::Format("Openlab header too short".into()));
    }
    if data[..4] != *OPENLAB_MAGIC {
        return Err(BioFormatsError::UnsupportedFormat(
            "Openlab raw header is missing LBLB magic".into(),
        ));
    }

    // Width at offset 8, Height at offset 12, bit_depth at offset 16 (i32 BE)
    let width = i32::from_be_bytes([data[8], data[9], data[10], data[11]]);
    let height = i32::from_be_bytes([data[12], data[13], data[14], data[15]]);
    let bit_depth = i32::from_be_bytes([data[16], data[17], data[18], data[19]]);
    if width <= 0 || height <= 0 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "Openlab raw header has invalid dimensions {width}x{height}"
        )));
    }

    let pixel_type = match bit_depth {
        8 => PixelType::Uint8,
        16 => PixelType::Uint16,
        32 => PixelType::Float32,
        _ => {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "Openlab raw bit depth {bit_depth} is not supported"
            )));
        }
    };

    let meta = default_meta(width as u32, height as u32, pixel_type);
    let required_len = OPENLAB_HEADER_SIZE
        .checked_add(
            (meta.size_x as u64)
                .checked_mul(meta.size_y as u64)
                .and_then(|n| n.checked_mul(meta.pixel_type.bytes_per_sample() as u64))
                .ok_or_else(|| {
                    BioFormatsError::Format("Openlab raw plane size overflows".into())
                })?,
        )
        .ok_or_else(|| BioFormatsError::Format("Openlab raw file size overflows".into()))?;
    if (data.len() as u64) < required_len {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "Openlab raw pixel payload is shorter than declared image: got {} bytes, expected at least {required_len}",
            data.len()
        )));
    }

    Ok(meta)
}

impl FormatReader for OpenlabRawReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("raw"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.len() >= 4 && header[0..4] == *OPENLAB_MAGIC
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let meta = parse_openlab(path)?;
        self.path = Some(path.to_path_buf());
        self.meta = Some(meta);
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
        self.meta
            .as_ref()
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let path = self
            .path
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?
            .clone();
        open_bytes_impl(&path, OPENLAB_HEADER_SIZE, meta, plane_index)
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
        region_from_full(&full, meta, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}

// ── PhotonDynamicsReader ──────────────────────────────────────────────────────

pub struct PhotonDynamicsReader {
    path: Option<PathBuf>,
    pixels_path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    record_width: usize,
    reverse_x: bool,
    reverse_y: bool,
}

impl PhotonDynamicsReader {
    pub fn new() -> Self {
        PhotonDynamicsReader {
            path: None,
            pixels_path: None,
            meta: None,
            record_width: 0,
            reverse_x: false,
            reverse_y: false,
        }
    }
}

impl Default for PhotonDynamicsReader {
    fn default() -> Self {
        Self::new()
    }
}

fn photon_dynamics_header_path(path: &Path) -> PathBuf {
    if path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("img"))
        .unwrap_or(false)
    {
        let lower = path.with_extension("hdr");
        if lower.exists() {
            lower
        } else {
            path.with_extension("HDR")
        }
    } else {
        path.to_path_buf()
    }
}

fn photon_dynamics_pixels_path(header_path: &Path) -> PathBuf {
    let upper = header_path.with_extension("IMG");
    if upper.exists() {
        upper
    } else {
        header_path.with_extension("img")
    }
}

fn parse_photon_dynamics_header(
    path: &Path,
) -> Result<(ImageMetadata, PathBuf, usize, bool, bool)> {
    let header_path = photon_dynamics_header_path(path);
    let content = std::fs::read_to_string(&header_path).map_err(BioFormatsError::Io)?;
    if !content.starts_with(" IDENTIFICATION") {
        return Err(BioFormatsError::UnsupportedFormat(
            "Photon Dynamics PDS header missing IDENTIFICATION magic".into(),
        ));
    }

    let mut size_x = None;
    let mut size_y = None;
    let mut record_width = None;
    let mut reverse_x = false;
    let mut reverse_y = false;
    let mut color = None;
    let mut metadata = HashMap::new();

    for raw_line in content.lines() {
        let Some(eq) = raw_line.find('=') else {
            continue;
        };
        let end = raw_line.find('/').unwrap_or(raw_line.len());
        let key = raw_line[..eq].trim();
        let value = raw_line[eq + 1..end].trim().trim_matches('\'').trim();
        metadata.insert(key.to_string(), MetadataValue::String(value.to_string()));

        match key {
            "NXP" => size_x = value.parse::<u32>().ok(),
            "NYP" => size_y = value.parse::<u32>().ok(),
            "SIGNX" => reverse_x = value == "-",
            "SIGNY" => reverse_y = value == "-",
            "COLOR" => color = value.parse::<u32>().ok(),
            "FILE REC LEN" => {
                record_width = value.parse::<usize>().ok().map(|bytes| bytes / 2);
            }
            _ => {}
        }
    }

    let size_x = size_x.ok_or_else(|| {
        BioFormatsError::UnsupportedFormat("Photon Dynamics PDS header missing NXP".into())
    })?;
    let size_y = size_y.ok_or_else(|| {
        BioFormatsError::UnsupportedFormat("Photon Dynamics PDS header missing NYP".into())
    })?;
    if size_x == 0 || size_y == 0 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "Photon Dynamics PDS has invalid dimensions {size_x}x{size_y}"
        )));
    }

    let mut meta = default_meta(size_x, size_y, PixelType::Uint16);
    meta.dimension_order = DimensionOrder::XYCZT;
    if color == Some(4) {
        meta.size_c = 3;
        meta.is_rgb = true;
        meta.is_interleaved = false;
    } else if let Some(color) = color {
        meta.is_indexed = color > 0;
    }
    meta.series_metadata = metadata;

    let pixels_path = photon_dynamics_pixels_path(&header_path);
    let record_width = record_width.ok_or_else(|| {
        BioFormatsError::UnsupportedFormat("Photon Dynamics PDS header missing FILE REC LEN".into())
    })?;
    let record_width = record_width.max(size_x as usize);
    let row_pixels = record_width;
    let required_len = (row_pixels as u64)
        .checked_mul(size_y as u64)
        .and_then(|n| n.checked_mul(meta.size_c as u64))
        .and_then(|n| n.checked_mul(2))
        .ok_or_else(|| BioFormatsError::Format("Photon Dynamics IMG size overflows".into()))?;
    let actual_len = std::fs::metadata(&pixels_path)
        .map_err(BioFormatsError::Io)?
        .len();
    if actual_len < required_len {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "Photon Dynamics IMG payload is shorter than declared image: got {actual_len} bytes, expected at least {required_len}"
        )));
    }

    Ok((meta, pixels_path, record_width, reverse_x, reverse_y))
}

fn read_photon_dynamics_plane(
    path: &Path,
    meta: &ImageMetadata,
    record_width: usize,
    reverse_x: bool,
    reverse_y: bool,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
) -> Result<Vec<u8>> {
    if x.checked_add(w).is_none_or(|end| end > meta.size_x)
        || y.checked_add(h).is_none_or(|end| end > meta.size_y)
    {
        return Err(BioFormatsError::InvalidData(
            "Photon Dynamics region exceeds image bounds".into(),
        ));
    }

    let mut file = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
    let channel_bytes = w as usize * h as usize * 2;
    let mut out = vec![0u8; channel_bytes * meta.size_c as usize];
    let read_x = if reverse_x { meta.size_x - w - x } else { x } as usize;
    let read_y = if reverse_y { meta.size_y - h - y } else { y } as usize;
    let row_stride = record_width.max(meta.size_x as usize) * 2;
    let channel_stride = row_stride * meta.size_y as usize;

    for channel in 0..meta.size_c as usize {
        for row in 0..h as usize {
            let src = (channel * channel_stride + (read_y + row) * row_stride + read_x * 2) as u64;
            file.seek(SeekFrom::Start(src))
                .map_err(BioFormatsError::Io)?;
            let dst = channel * channel_bytes + row * w as usize * 2;
            file.read_exact(&mut out[dst..dst + w as usize * 2])
                .map_err(BioFormatsError::Io)?;
        }
    }

    if reverse_x {
        for channel in 0..meta.size_c as usize {
            let start = channel * channel_bytes;
            let end = start + channel_bytes;
            for row in out[start..end].chunks_exact_mut(w as usize * 2) {
                for col in 0..w as usize / 2 {
                    let left = col * 2;
                    let right = (w as usize - col - 1) * 2;
                    row.swap(left, right);
                    row.swap(left + 1, right + 1);
                }
            }
        }
    }

    if reverse_y {
        let row_bytes = w as usize * 2;
        for channel in 0..meta.size_c as usize {
            let base = channel * channel_bytes;
            for row in 0..h as usize / 2 {
                let top = base + row * row_bytes;
                let bottom = base + (h as usize - row - 1) * row_bytes;
                for col in 0..row_bytes {
                    out.swap(top + col, bottom + col);
                }
            }
        }
    }

    Ok(out)
}

impl FormatReader for PhotonDynamicsReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        match ext.as_deref() {
            Some("hdr") | Some("pds") => std::fs::read(path)
                .map(|header| self.is_this_type_by_bytes(&header))
                .unwrap_or(false),
            Some("img") => {
                let header_path = photon_dynamics_header_path(path);
                std::fs::read(header_path)
                    .map(|header| self.is_this_type_by_bytes(&header))
                    .unwrap_or(false)
            }
            _ => false,
        }
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.starts_with(b" IDENTIFICATION")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let (meta, pixels_path, record_width, reverse_x, reverse_y) =
            parse_photon_dynamics_header(path)?;
        self.path = Some(photon_dynamics_header_path(path));
        self.pixels_path = Some(pixels_path);
        self.meta = Some(meta);
        self.record_width = record_width;
        self.reverse_x = reverse_x;
        self.reverse_y = reverse_y;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.pixels_path = None;
        self.meta = None;
        self.record_width = 0;
        self.reverse_x = false;
        self.reverse_y = false;
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
        if plane_index != 0 {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let pixels = self
            .pixels_path
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?;
        read_photon_dynamics_plane(
            pixels,
            meta,
            self.record_width,
            self.reverse_x,
            self.reverse_y,
            0,
            0,
            meta.size_x,
            meta.size_y,
        )
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
        if plane_index != 0 {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let pixels = self
            .pixels_path
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?;
        read_photon_dynamics_plane(
            pixels,
            meta,
            self.record_width,
            self.reverse_x,
            self.reverse_y,
            x,
            y,
            w,
            h,
        )
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}

#[cfg(test)]
mod photon_dynamics_tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tmp_pair(name: &str) -> (PathBuf, PathBuf) {
        let id = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let hdr = std::env::temp_dir().join(format!("{name}_{id}.hdr"));
        let img = hdr.with_extension("IMG");
        (hdr, img)
    }

    fn tmp_base(name: &str) -> PathBuf {
        let id = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("{name}_{id}"))
    }

    fn write_header(path: &Path, sign_x: &str, sign_y: &str, rec_len: usize) {
        std::fs::write(
            path,
            format!(
                " IDENTIFICATION\nNXP = 3\nNYP = 2\nSIGNX = '{sign_x}'\nSIGNY = '{sign_y}'\nCOLOR = 1\nFILE REC LEN = {}\n",
                rec_len * 2
            ),
        )
        .unwrap();
    }

    #[test]
    fn photon_dynamics_reads_companion_img_with_record_padding() {
        let (hdr, img) = tmp_pair("photon_padded");
        write_header(&hdr, "+", "+", 4);
        let samples = [1u16, 2, 3, 99, 4, 5, 6, 88];
        let bytes: Vec<u8> = samples.into_iter().flat_map(u16::to_le_bytes).collect();
        std::fs::write(&img, bytes).unwrap();

        let mut reader = PhotonDynamicsReader::new();
        reader.set_id(&hdr).unwrap();

        let expected: Vec<u8> = [1u16, 2, 3, 4, 5, 6]
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(reader.open_bytes(0).unwrap(), expected);

        let crop: Vec<u8> = [2u16, 3, 5, 6]
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(reader.open_bytes_region(0, 1, 0, 2, 2).unwrap(), crop);

        let _ = std::fs::remove_file(hdr);
        let _ = std::fs::remove_file(img);
    }

    #[test]
    fn photon_dynamics_detects_only_identification_headers() {
        let (hdr, img) = tmp_pair("photon_detect");
        std::fs::write(&hdr, b"not a photon dynamics header").unwrap();
        std::fs::write(&img, []).unwrap();

        let mut reader = PhotonDynamicsReader::new();
        assert!(!reader.is_this_type_by_name(&hdr));
        assert!(!reader.is_this_type_by_name(&img));
        assert!(matches!(
            reader.set_series(0),
            Err(BioFormatsError::NotInitialized)
        ));
        assert_eq!(reader.series_count(), 0);

        write_header(&hdr, "+", "+", 4);
        assert!(reader.is_this_type_by_name(&hdr));
        assert!(reader.is_this_type_by_name(&img));

        let _ = std::fs::remove_file(hdr);
        let _ = std::fs::remove_file(img);
    }

    #[test]
    fn photon_dynamics_opens_img_with_uppercase_hdr_sibling() {
        let base = tmp_base("photon_upper_hdr");
        let hdr = base.with_extension("HDR");
        let img = base.with_extension("IMG");
        write_header(&hdr, "+", "+", 4);
        let samples = [1u16, 2, 3, 99, 4, 5, 6, 88];
        let bytes: Vec<u8> = samples.into_iter().flat_map(u16::to_le_bytes).collect();
        std::fs::write(&img, bytes).unwrap();

        let mut reader = PhotonDynamicsReader::new();
        assert!(reader.is_this_type_by_name(&img));
        reader.set_id(&img).unwrap();
        assert_eq!(reader.path.as_deref(), Some(hdr.as_path()));
        assert_eq!(
            reader.open_bytes_region(0, 0, 1, 3, 1).unwrap(),
            [4u16, 5, 6]
                .into_iter()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>()
        );

        let _ = std::fs::remove_file(hdr);
        let _ = std::fs::remove_file(img);
    }

    #[test]
    fn photon_dynamics_reads_planar_rgb_payload() {
        let (hdr, img) = tmp_pair("photon_rgb");
        std::fs::write(
            &hdr,
            " IDENTIFICATION\nNXP = 2\nNYP = 1\nSIGNX = '+'\nSIGNY = '+'\nCOLOR = 4\nFILE REC LEN = 6\n",
        )
        .unwrap();
        let samples = [
            10u16, 20, 0xeeee, // red row plus padding
            30, 40, 0xeeee, // green row plus padding
            50, 60, 0xeeee, // blue row plus padding
        ];
        let bytes: Vec<u8> = samples.into_iter().flat_map(u16::to_le_bytes).collect();
        std::fs::write(&img, bytes).unwrap();

        let mut reader = PhotonDynamicsReader::new();
        reader.set_id(&hdr).unwrap();
        assert_eq!(reader.metadata().size_c, 3);
        assert!(reader.metadata().is_rgb);
        assert!(!reader.metadata().is_interleaved);
        assert_eq!(
            reader.open_bytes(0).unwrap(),
            [10u16, 20, 30, 40, 50, 60]
                .into_iter()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>()
        );

        let _ = std::fs::remove_file(hdr);
        let _ = std::fs::remove_file(img);
    }

    #[test]
    fn photon_dynamics_applies_reverse_axes_after_reading_region() {
        let (hdr, img) = tmp_pair("photon_reversed");
        write_header(&hdr, "-", "-", 3);
        let samples = [1u16, 2, 3, 4, 5, 6];
        let bytes: Vec<u8> = samples.into_iter().flat_map(u16::to_le_bytes).collect();
        std::fs::write(&img, bytes).unwrap();

        let mut reader = PhotonDynamicsReader::new();
        reader.set_id(&hdr).unwrap();

        let expected: Vec<u8> = [6u16, 5, 4, 3, 2, 1]
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(reader.open_bytes(0).unwrap(), expected);

        let crop: Vec<u8> = [6u16, 5, 3, 2]
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(reader.open_bytes_region(0, 0, 0, 2, 2).unwrap(), crop);

        let _ = std::fs::remove_file(hdr);
        let _ = std::fs::remove_file(img);
    }

    #[test]
    fn photon_dynamics_rejects_missing_magic_and_short_img() {
        let (hdr, img) = tmp_pair("photon_invalid");
        std::fs::write(&hdr, b"NXP = 3\nNYP = 2\n").unwrap();
        std::fs::write(&img, []).unwrap();
        let err = PhotonDynamicsReader::new().set_id(&hdr).unwrap_err();
        assert!(matches!(
            err,
            BioFormatsError::UnsupportedFormat(message) if message.contains("IDENTIFICATION")
        ));

        write_header(&hdr, "+", "+", 3);
        let err = PhotonDynamicsReader::new().set_id(&hdr).unwrap_err();
        assert!(matches!(
            err,
            BioFormatsError::UnsupportedFormat(message) if message.contains("shorter")
        ));

        let _ = std::fs::remove_file(hdr);
        let _ = std::fs::remove_file(img);
    }

    #[test]
    fn photon_dynamics_failed_reopen_clears_state() {
        let (valid_hdr, valid_img) = tmp_pair("photon_valid_reopen");
        write_header(&valid_hdr, "+", "+", 3);
        let samples = [1u16, 2, 3, 4, 5, 6];
        let bytes: Vec<u8> = samples.into_iter().flat_map(u16::to_le_bytes).collect();
        std::fs::write(&valid_img, bytes).unwrap();

        let (bad_hdr, bad_img) = tmp_pair("photon_bad_reopen");
        std::fs::write(&bad_hdr, b"NXP = 3\nNYP = 2\n").unwrap();
        std::fs::write(&bad_img, []).unwrap();

        let mut reader = PhotonDynamicsReader::new();
        reader.set_id(&valid_hdr).unwrap();
        assert_eq!(reader.series_count(), 1);
        let _ = reader.set_id(&bad_hdr).unwrap_err();
        assert_eq!(reader.series_count(), 0);
        assert!(matches!(
            reader.open_bytes(0),
            Err(BioFormatsError::NotInitialized)
        ));

        let _ = std::fs::remove_file(valid_hdr);
        let _ = std::fs::remove_file(valid_img);
        let _ = std::fs::remove_file(bad_hdr);
        let _ = std::fs::remove_file(bad_img);
    }
}

#[cfg(test)]
mod perkinelmer_metadata_tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_dir(name: &str) -> PathBuf {
        let id = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("{name}_{id}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn parse_key_value_fills_origin_pixel_size_and_times() {
        let mut m = PeMeta::default();
        pe_parse_key_value(&mut m, "Pixel Size X", "0.25");
        pe_parse_key_value(&mut m, "Pixel Size Y", "0.30");
        pe_parse_key_value(&mut m, "Origin X", "10.0");
        pe_parse_key_value(&mut m, "Origin Y", "20.0");
        pe_parse_key_value(&mut m, "Origin Z", "5.5");
        pe_parse_key_value(&mut m, "Start Time:", "09:30:00 (01/02/2020)");
        pe_parse_key_value(&mut m, "Finish Time:", "09:31:00 (01/02/2020)");
        pe_parse_key_value(&mut m, "Z slice space", "1.25");
        pe_parse_key_value(&mut m, "Experiment details:", "3 Wavelengths 4 Frames 2 Slices");

        assert_eq!(m.pixel_size_x, 0.25);
        assert_eq!(m.pixel_size_y, 0.30);
        assert_eq!(m.origin_x, 10.0);
        assert_eq!(m.origin_y, 20.0);
        assert_eq!(m.origin_z, 5.5);
        assert_eq!(m.start_time.as_deref(), Some("09:30:00 (01/02/2020)"));
        assert_eq!(m.finish_time.as_deref(), Some("09:31:00 (01/02/2020)"));
        assert_eq!(m.slice_space.as_deref(), Some("1.25"));
        assert_eq!(m.details.as_deref(), Some("3 Wavelengths 4 Frames 2 Slices"));
        // Every key is also kept as global metadata (Java addGlobalMeta).
        assert!(matches!(
            m.metadata.get("Origin Z"),
            Some(MetadataValue::String(v)) if v == "5.5"
        ));
    }

    #[test]
    fn parse_csv_extracts_pixel_sizes_and_slice_space() {
        // 23 tokens: significant values at indices 7, 12, 18, 22.
        let mut tokens = vec!["x"; 23];
        tokens[7] = "microns"; // Calibration Unit
        tokens[12] = "0.42"; // Pixel Size X
        tokens[18] = "0.43"; // Pixel Size Y
        tokens[22] = "2.5"; // Z slice space
        let content = tokens.join(" ");

        let mut m = PeMeta::default();
        pe_parse_csv(&mut m, &content);

        assert_eq!(m.pixel_size_x, 0.42);
        assert_eq!(m.pixel_size_y, 0.43);
        assert_eq!(m.slice_space.as_deref(), Some("2.5"));
        assert!(matches!(
            m.metadata.get("Calibration Unit"),
            Some(MetadataValue::String(v)) if v == "microns"
        ));
    }

    #[test]
    fn parse_zpo_records_indexed_slice_positions() {
        let mut m = PeMeta::default();
        pe_parse_zpo(&mut m, "0.0 1.5 3.0");
        assert!(matches!(
            m.metadata.get("Z slice position #1"),
            Some(MetadataValue::String(v)) if v == "0.0"
        ));
        assert!(matches!(
            m.metadata.get("Z slice position #3"),
            Some(MetadataValue::String(v)) if v == "3.0"
        ));
    }

    #[test]
    fn finish_time_converts_to_iso8601() {
        assert_eq!(
            pe_finish_time_to_iso8601("09:31:05 (01/02/2020)").as_deref(),
            Some("2020-01-02T09:31:05")
        );
        // Malformed strings yield None (Java returns null).
        assert_eq!(pe_finish_time_to_iso8601("not a date"), None);
        assert_eq!(pe_finish_time_to_iso8601("9:31 (1/2/2020)"), None);
    }

    #[test]
    fn iso8601_to_unix_millis_spacing_is_one_minute() {
        let start = pe_iso8601_to_unix_millis("2020-01-02T09:30:00").unwrap();
        let end = pe_iso8601_to_unix_millis("2020-01-02T09:31:00").unwrap();
        assert_eq!(end - start, 60_000);
        // Known epoch anchor: 1970-01-01T00:00:00 == 0 ms.
        assert_eq!(pe_iso8601_to_unix_millis("1970-01-01T00:00:00"), Some(0));
    }

    #[test]
    fn set_id_surfaces_pixel_size_origin_and_time_in_ome() {
        let dir = unique_dir("perkin_ome");
        let htm = dir.join("scan.htm");
        std::fs::write(&htm, b"<html><body></body></html>").unwrap();

        // A .csv companion supplies the pixel sizes deterministically.
        let mut tokens = vec!["x"; 23];
        tokens[7] = "microns";
        tokens[12] = "0.25";
        tokens[18] = "0.30";
        tokens[22] = "1.0";
        std::fs::write(dir.join("scan.csv"), tokens.join(" ")).unwrap();

        // A small TIFF pixel file (single plane).
        let mut meta = ImageMetadata {
            size_x: 3,
            size_y: 2,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type: PixelType::Uint8,
            bits_per_pixel: 8,
            image_count: 1,
            is_little_endian: true,
            ..default_meta(3, 2, PixelType::Uint8)
        };
        meta.dimension_order = DimensionOrder::XYZCT;
        crate::writer_registry::ImageWriter::save(
            &dir.join("scan.tif"),
            &meta,
            &[vec![1u8, 2, 3, 4, 5, 6]],
        )
        .unwrap();

        let mut pe = PerkinElmerReader::new();
        pe.set_id(&htm).unwrap();

        // Typed fields populated from the .csv.
        assert_eq!(pe.pixel_size_x, 0.25);
        assert_eq!(pe.pixel_size_y, 0.30);
        assert_eq!(pe.slice_space(), Some("1.0"));

        // Manually set origin/time on the reader to exercise the OME projection
        // (the .htm token format is brittle to construct in a unit test).
        pe.origin_x = 11.0;
        pe.origin_y = 22.0;
        pe.origin_z = 3.0;
        pe.start_time = Some("09:30:00 (01/02/2020)".into());
        pe.finish_time = Some("09:31:00 (01/02/2020)".into());

        let ome = pe.ome_metadata().unwrap();
        let img = &ome.images[0];
        assert_eq!(img.physical_size_x, Some(0.25));
        assert_eq!(img.physical_size_y, Some(0.30));
        assert_eq!(img.acquisition_date.as_deref(), Some("2020-01-02T09:31:00"));
        assert_eq!(img.planes.len(), 1);
        assert_eq!(img.planes[0].position_x, Some(11.0));
        assert_eq!(img.planes[0].position_y, Some(22.0));
        assert_eq!(img.planes[0].position_z, Some(3.0));
        assert_eq!(img.planes[0].delta_t, Some(0.0));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn close_resets_data_fields_to_defaults() {
        let mut pe = PerkinElmerReader::new();
        pe.pixel_size_x = 9.0;
        pe.origin_z = 4.0;
        pe.finish_time = Some("x".into());
        pe.details = Some("d".into());
        pe.slice_space = Some("s".into());
        pe.close().unwrap();
        assert_eq!(pe.pixel_size_x, 1.0);
        assert_eq!(pe.pixel_size_y, 1.0);
        assert_eq!(pe.origin_z, 0.0);
        assert!(pe.finish_time.is_none());
        assert!(pe.details().is_none());
        assert!(pe.slice_space().is_none());
    }
}
