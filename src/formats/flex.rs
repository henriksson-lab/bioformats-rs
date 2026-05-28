//! PerkinElmer/Evotec FLEX HCS format reader.
//!
//! FLEX is a TIFF-based format used for high-content screening (HCS) by
//! PerkinElmer/Evotec. Each FLEX image plane stores an XML metadata block in a
//! custom TIFF tag (65200). That XML contains one `<Array>` element per image
//! plane carrying `Name` and `Factor` attributes. `Factor` is a per-plane
//! scaling multiplier that must be applied to pixel values on read; when any
//! factor is greater than 1 the effective pixel type widens (UINT16, or UINT32
//! when the largest factor exceeds 256). This mirrors `FlexReader.java`.
//!
//! A FLEX dataset is normally a *directory* of `.flex` files, one per
//! (well, field), optionally accompanied by `.mea`/`.res` measurement
//! companions that enumerate the wells/fields. Following `FlexReader.java`:
//!   - `.flex` filenames of the form `nnnnnnnnn.flex` (14 chars) encode the
//!     well row (chars 0..3), well column (chars 3..6) and field (chars 6..9),
//!     all 1-based.
//!   - Files are grouped by well; each file within a well is a field.
//!   - One OME series is produced per (plate, well, field).
//!   - The `.mea` file lists `<Picture path=...>` entries pointing at the
//!     `.flex` files; the `.res` file carries the plate acquisition date.
//!
//! When no companion files are present and the single `.flex` cannot be grouped
//! (filename is not the 14-char well pattern), the reader falls back to the
//! original single-file behavior: the TIFF IFDs map directly to planes of a
//! single series.
//!
//! This reader wraps `TiffReader` for the raw TIFF pixel I/O and layers the
//! Flex XML factor parsing + pixel scaling + well/field series assembly on top.

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::ImageMetadata;
use crate::common::ome_metadata::{create_lsid, OmeMetadata, OmePlate, OmeWell, OmeWellSample};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::tiff::ifd::IfdValue;
use std::path::{Path, PathBuf};

/// Custom Flex IFD entry holding the per-image XML (FlexReader.FLEX = 65200).
const FLEX_TAG: u16 = 65200;

/// One `.flex` file in a grouped dataset: a (well row, well column, field).
struct FlexFile {
    row: u32,
    column: u32,
    field: u32,
    path: PathBuf,
    /// Per-plane scaling factors for this file (`None` when every factor is 1).
    factors: Option<Vec<f64>>,
}

pub struct FlexReader {
    /// TiffReader bound to the *currently selected* series' `.flex` file.
    inner: crate::tiff::TiffReader,
    /// Path currently loaded into `inner` (so we avoid re-opening on set_series).
    inner_path: Option<PathBuf>,
    /// Grouped files (multi-file HCS mode). Empty in single-file fallback.
    flex_files: Vec<FlexFile>,
    /// Current series index.
    series: usize,
    /// Effective pixel type after factor widening (applies to series 0 like Java).
    scaled_pixel_type: Option<PixelType>,
    /// Per-series metadata (cloned from the representative file's TIFF series 0).
    series_meta: Vec<ImageMetadata>,
    /// planes-per-image-count for the dataset (Z*C*T of series 0).
    image_count: u32,
    /// HCS layout.
    plate_count: u32,
    well_count: u32,
    field_count: u32,
    well_rows: u32,
    well_columns: u32,
    /// (row, col) for each well index (parallel to the column-major well list).
    well_number: Vec<(u32, u32)>,
    /// Companion measurement files found (.mea/.res), absolute paths.
    measurement_files: Vec<PathBuf>,
    /// Plate acquisition start time from the .res file.
    plate_acq_start_time: Option<String>,
    /// Plate name / barcode parsed from XML.
    plate_name: Option<String>,
    plate_barcode: Option<String>,
    /// True when running in single-file fallback mode.
    single_file: bool,
}

impl FlexReader {
    pub fn new() -> Self {
        FlexReader {
            inner: crate::tiff::TiffReader::new(),
            inner_path: None,
            flex_files: Vec::new(),
            series: 0,
            scaled_pixel_type: None,
            series_meta: Vec::new(),
            image_count: 0,
            plate_count: 0,
            well_count: 0,
            field_count: 0,
            well_rows: 0,
            well_columns: 0,
            well_number: Vec::new(),
            measurement_files: Vec::new(),
            plate_acq_start_time: None,
            plate_name: None,
            plate_barcode: None,
            single_file: true,
        }
    }

    /// Extract the Flex XML block (tag 65200) from the first IFD as a string.
    fn flex_xml(&self) -> Option<String> {
        let ifd = self.inner.ifd(0)?;
        match ifd.get(FLEX_TAG) {
            Some(IfdValue::Ascii(s)) => Some(s.clone()),
            Some(IfdValue::Byte(b)) | Some(IfdValue::Undefined(b)) => {
                Some(String::from_utf8_lossy(b).into_owned())
            }
            _ => None,
        }
    }

    /// Ensure `inner` is bound to the `.flex` file for series `s`.
    fn bind_series(&mut self, s: usize) -> Result<()> {
        if self.single_file {
            self.series = s;
            return Ok(());
        }
        let file = self
            .flex_files
            .get(self.file_index_for_series(s))
            .ok_or(BioFormatsError::SeriesOutOfRange(s))?;
        let path = file.path.clone();
        if self.inner_path.as_deref() != Some(path.as_path()) {
            self.inner.set_id(&path)?;
            self.inner_path = Some(path);
        }
        self.series = s;
        Ok(())
    }

    /// Map an OME series index to an index into `flex_files`.
    ///
    /// Mirrors Java `lookupFile(int fileSeries)`: lengths =
    /// {fieldCount, wellCount, plateCount}, raster-to-position, then look up by
    /// (row, col, field).
    fn file_index_for_series(&self, series: usize) -> usize {
        if self.flex_files.len() == 1 {
            return 0;
        }
        let field_count = self.field_count.max(1);
        let well_count = self.well_count.max(1);
        let plate_count = self.plate_count.max(1);

        // effectiveFieldCount: 1 when wellCount*plateCount == files.
        let effective_field_count = if (well_count * plate_count) as usize == self.flex_files.len()
        {
            1
        } else {
            field_count
        };

        let lengths = [
            field_count as usize,
            well_count as usize,
            plate_count as usize,
        ];
        let pos = raster_to_position(&lengths, series);

        let zero_well = well_count == 1 && effective_field_count == 1;
        let (row, col) = if zero_well {
            (0, 0)
        } else {
            self.well_number.get(pos[1]).copied().unwrap_or((0, 0))
        };
        let field = if effective_field_count == 1 {
            0
        } else {
            pos[0] as u32
        };

        self.flex_files
            .iter()
            .position(|f| f.row == row && f.column == col && f.field == field)
            .unwrap_or(0)
    }

    /// Parse `<Array Name=.. Factor=..>` arrays and derive factors / widening.
    /// Returns the factor vector (`None` when all factors are 1) and updates
    /// `scaled_pixel_type` from the largest factor (Java semantics).
    fn derive_factors(&mut self, total_planes: usize) -> Option<Vec<f64>> {
        let mut xml = self.flex_xml()?;
        let trimmed = xml.trim();
        if trimmed.ends_with(">>") || trimmed.ends_with('%') {
            xml = trimmed[..trimmed.len() - 1].to_string();
        } else {
            xml = trimmed.to_string();
        }
        let (_names, factors) = parse_flex_arrays(&xml);

        let total_planes = total_planes.max(factors.len());
        let mut factor_values = vec![1.0f64; total_planes];
        let mut max_idx = 0usize;
        let mut one_factors = true;
        for (i, f) in factors.iter().enumerate() {
            let q = f.parse::<f64>().unwrap_or(1.0);
            if i < factor_values.len() {
                factor_values[i] = q;
                if q > factor_values[max_idx] {
                    max_idx = i;
                }
                if q != 1.0 {
                    one_factors = false;
                }
            }
        }

        let max_factor = factor_values.get(max_idx).copied().unwrap_or(1.0);
        if max_factor > 256.0 {
            self.scaled_pixel_type = Some(PixelType::Uint32);
        } else if max_factor > 1.0 {
            self.scaled_pixel_type = Some(PixelType::Uint16);
        }

        if one_factors {
            None
        } else {
            Some(factor_values)
        }
    }

    /// Apply the Flex pixel scaling factor to a freshly read plane, widening to
    /// `scaled_pixel_type` if needed. Mirrors `FlexReader.openBytes` scaling.
    fn apply_factor(&self, raw: Vec<u8>, plane: u32, little_endian: bool) -> Vec<u8> {
        let src_pt = self
            .inner
            .series_list()
            .get(self.inner.series())
            .map(|s| s.metadata.pixel_type)
            .unwrap_or(PixelType::Uint8);
        let n_bytes = src_pt.bytes_per_sample();
        let dst_pt = self.scaled_pixel_type.unwrap_or(src_pt);
        let bpp = dst_pt.bytes_per_sample();

        let factor = if self.single_file {
            // factors stored on inner-file order via derive on series 0.
            self.series_factor(0, plane)
        } else {
            let fi = self.file_index_for_series(self.series);
            self.series_factor(fi, plane)
        };

        if factor == 1.0 && n_bytes == bpp {
            return raw;
        }
        if n_bytes == 0 || bpp == 0 {
            return raw;
        }

        let num = raw.len() / n_bytes;
        let mut out = vec![0u8; num * bpp];
        for i in 0..num {
            let q = read_uint(&raw, i * n_bytes, n_bytes, little_endian);
            let scaled = (q as f64 * factor) as u64;
            write_uint(&mut out, i * bpp, bpp, scaled, little_endian);
        }
        out
    }

    fn series_factor(&self, file_index: usize, plane: u32) -> f64 {
        self.flex_files
            .get(file_index)
            .and_then(|f| f.factors.as_ref())
            .and_then(|v| v.get(plane as usize).copied())
            .unwrap_or(1.0)
    }
}

impl Default for FlexReader {
    fn default() -> Self {
        Self::new()
    }
}

/// FormatTools.rasterToPosition: convert a raster index to per-axis positions
/// (axis 0 fastest-varying), mirroring the Java helper.
fn raster_to_position(lengths: &[usize], mut raster: usize) -> Vec<usize> {
    let mut pos = vec![0usize; lengths.len()];
    for (i, &len) in lengths.iter().enumerate() {
        let len = len.max(1);
        pos[i] = raster % len;
        raster /= len;
    }
    pos
}

/// Read an unsigned integer of `n` bytes from `buf` at `off`.
fn read_uint(buf: &[u8], off: usize, n: usize, little_endian: bool) -> u64 {
    let mut v = 0u64;
    for i in 0..n {
        let byte = buf.get(off + i).copied().unwrap_or(0) as u64;
        if little_endian {
            v |= byte << (8 * i);
        } else {
            v = (v << 8) | byte;
        }
    }
    v
}

/// Write an unsigned integer of `n` bytes into `buf` at `off`.
fn write_uint(buf: &mut [u8], off: usize, n: usize, value: u64, little_endian: bool) {
    for i in 0..n {
        let shift = if little_endian {
            8 * i
        } else {
            8 * (n - 1 - i)
        };
        if let Some(slot) = buf.get_mut(off + i) {
            *slot = ((value >> shift) & 0xff) as u8;
        }
    }
}

/// Parse all `<Array ... Name=... Factor=...>` elements, returning the lists of
/// names and factor strings in document order (mirrors FlexHandler).
fn parse_flex_arrays(xml: &str) -> (Vec<String>, Vec<String>) {
    let mut names = Vec::new();
    let mut factors = Vec::new();
    let bytes = xml.as_bytes();
    let mut i = 0;
    while let Some(rel) = xml[i..].find("<Array") {
        let start = i + rel;
        let end = match xml[start..].find('>') {
            Some(e) => start + e,
            None => break,
        };
        let tag = &xml[start..end];
        if let Some(name) = xml_attr(tag, "Name") {
            names.push(name);
        }
        if let Some(factor) = xml_attr(tag, "Factor") {
            factors.push(factor);
        }
        i = end + 1;
        if i >= bytes.len() {
            break;
        }
    }
    (names, factors)
}

/// Extract an XML attribute value (`attr="value"`) from a single start tag.
fn xml_attr(tag: &str, attr: &str) -> Option<String> {
    let mut search_from = 0;
    while let Some(rel) = tag[search_from..].find(attr) {
        let pos = search_from + rel;
        let prev_ok = pos == 0 || tag.as_bytes()[pos - 1].is_ascii_whitespace();
        let after = pos + attr.len();
        let rest = tag[after..].trim_start();
        if prev_ok && rest.starts_with('=') {
            let rest = rest[1..].trim_start();
            let quote = rest.chars().next()?;
            if quote == '"' || quote == '\'' {
                let val_start = 1;
                if let Some(end) = rest[val_start..].find(quote) {
                    return Some(rest[val_start..val_start + end].to_string());
                }
            }
        }
        search_from = after;
    }
    None
}

/// Extract the text content of the first `<tag>..</tag>` (or `Barcode` style)
/// occurrence. Used for plate Barcode/PlateName from the Flex XML.
fn xml_element_text(xml: &str, name: &str) -> Option<String> {
    let needle = name;
    let idx = xml.find(needle)?;
    let start = xml[idx..].find('>').map(|e| idx + e + 1)?;
    let end = xml[idx..].find('<').map(|e| idx + e)?;
    if end > start {
        Some(xml[start..end].to_string())
    } else {
        None
    }
}

/// Parse the well row/column (0-based) from a 14-char `nnnnnnnnn.flex` name.
/// Returns None if the name does not match the pattern.
fn parse_well(name: &str) -> Option<(u32, u32)> {
    if name.len() == 14 && name.to_ascii_lowercase().ends_with(".flex") {
        let row = name.get(0..3)?.parse::<u32>().ok()?;
        let col = name.get(3..6)?.parse::<u32>().ok()?;
        return Some((row.saturating_sub(1), col.saturating_sub(1)));
    }
    None
}

/// Parse the field index (0-based) from a 14-char `nnnnnnnnn.flex` name.
fn parse_field(name: &str) -> u32 {
    if name.len() == 14 && name.to_ascii_lowercase().ends_with(".flex") {
        if let Some(s) = name.get(6..9) {
            if let Ok(v) = s.parse::<u32>() {
                return v.saturating_sub(1);
            }
        }
    }
    0
}

/// Parse a `.mea` file's `<Picture path=...>` entries into a list of `.flex`
/// file names (relative). Mirrors MeaHandler (minus server-name remapping,
/// which is not applicable without a configured server map).
fn parse_mea_flex_names(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(rel) = text[i..].find("<Picture") {
        let start = i + rel;
        let end = match text[start..].find('>') {
            Some(e) => start + e,
            None => break,
        };
        let tag = &text[start..end];
        if let Some(mut path) = xml_attr(tag, "path") {
            if !path.to_ascii_lowercase().ends_with(".flex") {
                path.push_str(".flex");
            }
            // Normalise separators to native; we only use the file name below.
            let path = path.replace('\\', "/");
            out.push(path);
        }
        i = end + 1;
    }
    out
}

/// Parse the plate acquisition date attribute from a `.res` file
/// (`<AnalysisResults date="...">`). Mirrors ResHandler.
fn parse_res_date(text: &str) -> Option<String> {
    let idx = text.find("<AnalysisResults")?;
    let end = text[idx..].find('>').map(|e| idx + e)?;
    let tag = &text[idx..end];
    xml_attr(tag, "date")
}

/// Find companion `.mea`/`.res` files in the same directory as the `.flex`.
fn find_measurement_files(flex_path: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Some(dir) = flex_path.parent() else {
        return out;
    };
    if let Ok(rd) = std::fs::read_dir(dir) {
        for entry in rd.flatten() {
            let p = entry.path();
            let ext = p
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_ascii_lowercase());
            if matches!(ext.as_deref(), Some("mea") | Some("res")) {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// Collect grouped `.flex` files for a dataset. Returns the sorted list of
/// 14-char-pattern `.flex` files in the same directory, or just the input file
/// when no grouping is possible.
fn collect_flex_files(flex_path: &Path) -> Vec<PathBuf> {
    let name = flex_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    // Only group when the file follows the nnnnnnnnn.flex naming convention.
    if parse_well(name).is_none() {
        return vec![flex_path.to_path_buf()];
    }
    let Some(dir) = flex_path.parent() else {
        return vec![flex_path.to_path_buf()];
    };
    let mut files: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for entry in rd.flatten() {
            let p = entry.path();
            let n = p.file_name().and_then(|x| x.to_str()).unwrap_or_default();
            if n.len() == 14 && n.to_ascii_lowercase().ends_with(".flex") {
                files.push(p);
            }
        }
    }
    if files.is_empty() {
        files.push(flex_path.to_path_buf());
    }
    files.sort();
    files
}

impl FormatReader for FlexReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| {
                e.eq_ignore_ascii_case("flex")
                    || e.eq_ignore_ascii_case("mea")
                    || e.eq_ignore_ascii_case("res")
            })
            .unwrap_or(false)
    }
    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        if header.len() < 4 {
            return false;
        }
        (header[0] == 0x49 && header[1] == 0x49 && header[2] == 0x2A && header[3] == 0x00)
            || (header[0] == 0x4D && header[1] == 0x4D && header[2] == 0x00 && header[3] == 0x2A)
            || (header[0] == 0x49 && header[1] == 0x49 && header[2] == 0x2B && header[3] == 0x00)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        // Resolve the .flex entry point. If handed a .mea/.res, find a .flex in
        // the same directory (Java initMeaFile/initResFile fall back to this).
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        let flex_entry: PathBuf = if matches!(ext.as_deref(), Some("flex")) {
            path.to_path_buf()
        } else {
            // .mea / .res: locate a .flex in the same directory.
            let dir = path.parent().unwrap_or_else(|| Path::new("."));
            let mut found = None;
            if let Ok(rd) = std::fs::read_dir(dir) {
                let mut candidates: Vec<PathBuf> = rd
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| {
                        p.extension()
                            .and_then(|e| e.to_str())
                            .map(|e| e.eq_ignore_ascii_case("flex"))
                            .unwrap_or(false)
                    })
                    .collect();
                candidates.sort();
                found = candidates.into_iter().next();
            }
            found.ok_or_else(|| {
                BioFormatsError::UnsupportedFormat(
                    "Flex .mea/.res companion has no .flex files in its directory".into(),
                )
            })?
        };

        let measurement_files = find_measurement_files(&flex_entry);
        // Parse .res for the plate acquisition start time.
        for m in &measurement_files {
            if m.extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("res"))
                .unwrap_or(false)
            {
                if let Ok(text) = std::fs::read_to_string(m) {
                    if let Some(d) = parse_res_date(&text) {
                        self.plate_acq_start_time = Some(d);
                    }
                }
            }
        }

        // Determine the grouped file list. Prefer the .mea list when present.
        let mut grouped: Vec<PathBuf> = Vec::new();
        for m in &measurement_files {
            if m.extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("mea"))
                .unwrap_or(false)
            {
                if let Ok(text) = std::fs::read_to_string(m) {
                    let dir = flex_entry.parent().unwrap_or_else(|| Path::new("."));
                    for rel in parse_mea_flex_names(&text) {
                        // Match by file name within the .flex directory.
                        let fname = rel.rsplit('/').next().unwrap_or(&rel);
                        let candidate = dir.join(fname);
                        if candidate.exists() {
                            grouped.push(candidate);
                        }
                    }
                }
            }
        }
        if grouped.is_empty() {
            grouped = collect_flex_files(&flex_entry);
        } else {
            grouped.sort();
            grouped.dedup();
        }
        self.measurement_files = measurement_files;

        // Single-file fallback: cannot group by well pattern.
        let entry_name = flex_entry
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        let groupable = grouped.len() > 1 || parse_well(entry_name).is_some();

        if !groupable {
            // ---- single-file mode (original behavior) ----
            self.single_file = true;
            self.inner.set_id(&flex_entry)?;
            self.inner_path = Some(flex_entry.clone());

            let total_planes: usize = (0..self.inner.series_count())
                .map(|s| self.inner.series_list()[s].metadata.image_count as usize)
                .sum();
            let factors = self.derive_factors(total_planes);

            if let Some(pt) = self.scaled_pixel_type {
                let series = self.inner.series_list_mut();
                if let Some(s0) = series.first_mut() {
                    s0.metadata.pixel_type = pt;
                    s0.metadata.bits_per_pixel = (pt.bytes_per_sample() * 8) as u8;
                }
            }
            // store the single file's factors as flex_files[0] for apply_factor.
            self.flex_files = vec![FlexFile {
                row: 0,
                column: 0,
                field: 0,
                path: flex_entry,
                factors,
            }];
            self.series = 0;
            self.image_count = self
                .inner
                .series_list()
                .first()
                .map(|s| s.metadata.image_count)
                .unwrap_or(0);
            return Ok(());
        }

        // ---- multi-file HCS mode ----
        self.single_file = false;

        // Group files by well (row, col), each file within a well is a field.
        // Build well list in (row, col) order; record well_number layout.
        use std::collections::BTreeMap;
        let mut wells: BTreeMap<(u32, u32), Vec<PathBuf>> = BTreeMap::new();
        let mut max_row = 0u32;
        let mut max_col = 0u32;
        for f in &grouped {
            let n = f.file_name().and_then(|x| x.to_str()).unwrap_or_default();
            let (row, col) = if grouped.len() == 1 {
                (0, 0)
            } else {
                parse_well(n).unwrap_or((0, 0))
            };
            max_row = max_row.max(row);
            max_col = max_col.max(col);
            wells.entry((row, col)).or_default().push(f.clone());
        }
        self.well_rows = max_row + 1;
        self.well_columns = max_col + 1;
        if grouped.len() == 1 {
            self.well_rows = 1;
            self.well_columns = 1;
        }
        self.well_count = wells.len() as u32;

        // Build the flex_files list in well order, fields sorted within a well.
        let mut flex_files: Vec<FlexFile> = Vec::new();
        let mut well_number: Vec<(u32, u32)> = Vec::new();
        let mut n_files_per_well = 1usize;
        for (&(row, col), files) in &wells {
            well_number.push((row, col));
            let mut sorted = files.clone();
            sorted.sort();
            n_files_per_well = sorted.len();
            // Java assigns the field index by sorted position within the well
            // (FlexFile.field = field loop variable), but the filename's field
            // digits (chars 6..9) are the authoritative field number. Use the
            // filename field when the 14-char pattern is present, falling back
            // to sorted position otherwise.
            for (pos, p) in sorted.into_iter().enumerate() {
                let n = p.file_name().and_then(|x| x.to_str()).unwrap_or_default();
                let field = if n.len() == 14 {
                    parse_field(n)
                } else {
                    pos as u32
                };
                flex_files.push(FlexFile {
                    row,
                    column: col,
                    field,
                    path: p,
                    factors: None,
                });
            }
        }
        self.well_number = well_number;

        // Parse the first file to obtain core dimensions + factors.
        let first_path = flex_files[0].path.clone();
        self.inner.set_id(&first_path)?;
        self.inner_path = Some(first_path);
        let n_planes = self
            .inner
            .series_list()
            .first()
            .map(|s| s.metadata.image_count)
            .unwrap_or(0);

        // Derive factors & widening from series-0 XML.
        let factors = self.derive_factors(n_planes as usize);
        flex_files[0].factors = factors;

        // populateCoreMetadata field logic (faithful to the common case):
        // each file holds exactly imageCount planes => one field per file, so
        // fieldCount = number of files per well (Java: `fieldCount *= nFiles`
        // when fieldCount == 1). Fields-stored-within-a-file is rare and not
        // reconstructed here.
        let field_count = (n_files_per_well as u32).max(1);
        self.field_count = field_count;
        self.image_count = n_planes;

        self.plate_count = 1;

        // seriesCount = plateCount * wellCount * fieldCount.
        let series_count = (self.plate_count * self.well_count * self.field_count).max(1) as usize;

        // Apply factor widening to the (single) base metadata.
        let mut base_meta = self
            .inner
            .series_list()
            .first()
            .map(|s| s.metadata.clone())
            .ok_or_else(|| BioFormatsError::Format("Flex: no IFDs in first file".into()))?;
        if let Some(pt) = self.scaled_pixel_type {
            base_meta.pixel_type = pt;
            base_meta.bits_per_pixel = (pt.bytes_per_sample() * 8) as u8;
        }

        // Parse factors for the remaining files (each file may have its own XML).
        for i in 1..flex_files.len() {
            let p = flex_files[i].path.clone();
            self.inner.set_id(&p)?;
            self.inner_path = Some(p);
            let np = self
                .inner
                .series_list()
                .first()
                .map(|s| s.metadata.image_count)
                .unwrap_or(n_planes);
            let f = self.derive_factors(np as usize);
            flex_files[i].factors = f;
        }

        // Plate name/barcode from the first file's XML.
        if let Some(xml) = {
            // rebind to first file to read its XML
            let p = flex_files[0].path.clone();
            if self.inner_path.as_deref() != Some(p.as_path()) {
                self.inner.set_id(&p)?;
                self.inner_path = Some(p);
            }
            self.flex_xml()
        } {
            self.plate_barcode = xml_element_text(&xml, "Barcode");
            self.plate_name = xml_element_text(&xml, "PlateName");
        }

        self.flex_files = flex_files;
        self.series_meta = vec![base_meta; series_count];
        self.series = 0;
        // Bind inner to series 0's file.
        self.bind_series(0)?;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.flex_files.clear();
        self.inner_path = None;
        self.scaled_pixel_type = None;
        self.series_meta.clear();
        self.well_number.clear();
        self.measurement_files.clear();
        self.plate_acq_start_time = None;
        self.plate_name = None;
        self.plate_barcode = None;
        self.series = 0;
        self.single_file = true;
        self.inner.close()
    }

    fn series_count(&self) -> usize {
        if self.single_file {
            self.inner.series_count()
        } else {
            self.series_meta.len()
        }
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.single_file {
            self.inner.set_series(s)?;
            self.series = s;
            Ok(())
        } else {
            if s >= self.series_meta.len() {
                return Err(BioFormatsError::SeriesOutOfRange(s));
            }
            self.bind_series(s)
        }
    }

    fn series(&self) -> usize {
        self.series
    }

    fn metadata(&self) -> &ImageMetadata {
        if self.single_file {
            self.inner.metadata()
        } else {
            self.series_meta
                .get(self.series)
                .unwrap_or(crate::common::reader::uninitialized_metadata())
        }
    }

    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        if !self.single_file {
            self.bind_series(self.series)?;
        }
        let le = self.inner.is_little_endian();
        let raw = self.inner.open_bytes(p)?;
        Ok(self.apply_factor(raw, p, le))
    }

    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        if !self.single_file {
            self.bind_series(self.series)?;
        }
        let le = self.inner.is_little_endian();
        let raw = self.inner.open_bytes_region(p, x, y, w, h)?;
        Ok(self.apply_factor(raw, p, le))
    }

    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        if !self.single_file {
            self.bind_series(self.series)?;
        }
        self.inner.open_thumb_bytes(p)
    }

    fn resolution_count(&self) -> usize {
        self.inner.resolution_count()
    }
    fn set_resolution(&mut self, l: usize) -> Result<()> {
        self.inner.set_resolution(l)
    }
    fn resolution(&self) -> usize {
        self.inner.resolution()
    }

    fn ome_metadata(&self) -> Option<OmeMetadata> {
        if std::ptr::eq(
            self.metadata(),
            crate::common::reader::uninitialized_metadata(),
        ) {
            return None;
        }
        let mut ome = OmeMetadata::from_image_metadata(self.metadata());
        if self.single_file {
            return Some(ome);
        }

        // Build one Image per series; one Plate with Wells/WellSamples.
        let series_count = self.series_meta.len();
        ome.images = (0..series_count)
            .map(|_| crate::common::ome_metadata::OmeImage::default())
            .collect();

        let field_count = self.field_count.max(1) as usize;
        let well_count = self.well_count.max(1) as usize;
        let plate_count = self.plate_count.max(1) as usize;
        let lengths = [field_count, well_count, plate_count];

        let mut plate = OmePlate {
            id: Some(create_lsid("Plate", &[0])),
            name: self
                .plate_name
                .clone()
                .or_else(|| Some("Plate".to_string())),
            rows: self.well_rows,
            columns: self.well_columns,
            wells: Vec::new(),
        };
        if let Some(barcode) = &self.plate_barcode {
            plate.name = Some(match &plate.name {
                Some(n) => format!("{barcode} {n}"),
                None => barcode.clone(),
            });
        }

        // Index wells by (row,col) -> Vec<WellSample>.
        use std::collections::BTreeMap;
        let mut well_map: BTreeMap<(u32, u32), Vec<OmeWellSample>> = BTreeMap::new();

        for i in 0..series_count {
            let pos = raster_to_position(&lengths, i);
            let (row, col) = self.well_number.get(pos[1]).copied().unwrap_or((0, 0));
            let field = pos[0] as u32;
            well_map.entry((row, col)).or_default().push(OmeWellSample {
                id: Some(create_lsid(
                    "WellSample",
                    &[
                        pos[2],
                        (row * self.well_columns + col) as usize,
                        field as usize,
                    ],
                )),
                index: i as u32,
                image_ref: Some(i),
                position_x: None,
                position_y: None,
            });
        }

        for ((row, col), samples) in well_map {
            plate.wells.push(OmeWell {
                id: Some(create_lsid(
                    "Well",
                    &[0, (row * self.well_columns + col) as usize],
                )),
                row,
                column: col,
                well_samples: samples,
            });
        }

        ome.plates = vec![plate];
        Some(ome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_well_and_field_from_14char_name() {
        // 002003001.flex -> row 1, col 2, field 0 (all 1-based in file)
        assert_eq!(parse_well("002003001.flex"), Some((1, 2)));
        assert_eq!(parse_field("002003001.flex"), 0);
        assert_eq!(parse_field("002003004.flex"), 3);
    }

    #[test]
    fn non_pattern_name_is_not_groupable() {
        assert_eq!(parse_well("test.flex"), None);
        assert_eq!(parse_well("image001.flex"), None);
    }

    #[test]
    fn mea_picture_paths_get_flex_extension() {
        let mea =
            r#"<root><Picture path="dir/002003001"/><Picture path="dir\002003002.flex"/></root>"#;
        let names = parse_mea_flex_names(mea);
        assert_eq!(names, vec!["dir/002003001.flex", "dir/002003002.flex"]);
    }

    #[test]
    fn res_date_parsed() {
        let res = r#"<AnalysisResults date="01.02.2010  10:20:30" foo="bar">"#;
        assert_eq!(
            parse_res_date(res),
            Some("01.02.2010  10:20:30".to_string())
        );
    }

    #[test]
    fn raster_to_position_axis0_fastest() {
        // lengths {field=2, well=3, plate=1}; series 4 -> field 0, well 2.
        let p = raster_to_position(&[2, 3, 1], 4);
        assert_eq!(p, vec![0, 2, 0]);
    }

    #[test]
    fn flex_arrays_parse_name_and_factor() {
        let xml = r#"<Arrays><Array Name="1_ch1" Factor="2.0"/><Array Name="1_ch2" Factor="1"/></Arrays>"#;
        let (names, factors) = parse_flex_arrays(xml);
        assert_eq!(names, vec!["1_ch1", "1_ch2"]);
        assert_eq!(factors, vec!["2.0", "1"]);
    }
}
