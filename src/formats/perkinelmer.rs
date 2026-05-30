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
#[derive(Default)]
struct PeMeta {
    size_x: u32,
    size_y: u32,
    size_z: u32,
    size_c: u32,
    size_t: u32,
    details: Option<String>,
    metadata: HashMap<String, MetadataValue>,
}

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

    // Parse metadata.
    let mut m = PeMeta::default();
    if let Some(tf) = &tim_file {
        if let Ok(content) = std::fs::read_to_string(tf) {
            pe_parse_tim(&mut m, &content);
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
    let _ = (csv_file, zpo_file);

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
        path.with_extension("hdr")
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
    } else if let Some(color) = color {
        meta.is_indexed = color > 0;
    }
    meta.series_metadata = metadata;

    let pixels_path = photon_dynamics_pixels_path(&header_path);
    let record_width = record_width.unwrap_or(size_x as usize).max(size_x as usize);
    let row_pixels = record_width;
    let required_len = (row_pixels as u64)
        .checked_mul(size_y as u64)
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
    let mut out = vec![0u8; w as usize * h as usize * 2];
    let read_x = if reverse_x { meta.size_x - w - x } else { x } as usize;
    let read_y = if reverse_y { meta.size_y - h - y } else { y } as usize;
    let row_stride = record_width.max(meta.size_x as usize) * 2;

    for row in 0..h as usize {
        let src = ((read_y + row) * row_stride + read_x * 2) as u64;
        file.seek(SeekFrom::Start(src))
            .map_err(BioFormatsError::Io)?;
        let dst = row * w as usize * 2;
        file.read_exact(&mut out[dst..dst + w as usize * 2])
            .map_err(BioFormatsError::Io)?;
    }

    if reverse_x {
        for row in out.chunks_exact_mut(w as usize * 2) {
            for col in 0..w as usize / 2 {
                let left = col * 2;
                let right = (w as usize - col - 1) * 2;
                row.swap(left, right);
                row.swap(left + 1, right + 1);
            }
        }
    }

    if reverse_y {
        let row_bytes = w as usize * 2;
        for row in 0..h as usize / 2 {
            let top = row * row_bytes;
            let bottom = (h as usize - row - 1) * row_bytes;
            for col in 0..row_bytes {
                out.swap(top + col, bottom + col);
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
        matches!(ext.as_deref(), Some("hdr") | Some("img") | Some("pds"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.starts_with(b" IDENTIFICATION")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
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
}
