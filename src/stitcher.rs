//! FileStitcher — assembles multi-file datasets into a single reader.
//!
//! Equivalent to Java Bio-Formats' `FileStitcher`. Given a file pattern
//! or a single file from a series, discovers all related files and presents
//! them as one multi-dimensional image.
//!
//! # Pattern Syntax
//! - Numeric ranges: `img_<000-099>.tif` (matches img_000.tif through img_099.tif)
//! - Wildcards: `img_*.tif`
//!
//! # Example
//! ```no_run
//! use bioformats::FileStitcher;
//! use bioformats::FormatReader;
//! use std::path::Path;
//!
//! let mut reader = FileStitcher::open(Path::new("img_t000_c000.tif")).unwrap();
//! let meta = reader.metadata();
//! println!("Total planes: {}", meta.image_count);
//! ```

use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::ImageMetadata;
use crate::common::reader::FormatReader;

/// File stitcher that combines multiple files into one multi-dimensional reader.
pub struct FileStitcher {
    /// One inner reader per file, opened lazily.
    files: Vec<PathBuf>,
    /// Metadata for the stitched dataset.
    meta: Option<ImageMetadata>,
    /// Maps stitched plane indices to (file index, local plane index).
    plane_map: Vec<(usize, u32)>,
    /// The currently-open reader (index into `files`).
    current_reader: Option<(usize, Box<dyn FormatReader>)>,
}

impl FileStitcher {
    /// Open a stitched dataset starting from one file in the sequence.
    ///
    /// Discovers related files by analyzing the filename for numeric patterns
    /// and finding all files that match the same pattern.
    pub fn open(path: &Path) -> Result<Self> {
        let files = discover_sequence(path)?;
        if files.is_empty() {
            return Err(BioFormatsError::Format(
                "No files found for stitching".into(),
            ));
        }

        // Open the first file to get base metadata
        let mut first = crate::registry::ImageReader::open(&files[0])?;
        let base_meta = first.metadata().clone();

        let pattern = FilePattern::from_file(&files[0]).ok();
        let (meta, plane_map) = stitch_layout(&files, &base_meta, pattern.as_ref())?;
        let _ = first.close();

        Ok(FileStitcher {
            files,
            meta: Some(meta),
            plane_map,
            current_reader: None,
        })
    }

    /// Open with explicit file list (no auto-discovery).
    pub fn from_files(files: Vec<PathBuf>) -> Result<Self> {
        if files.is_empty() {
            return Err(BioFormatsError::Format("Empty file list".into()));
        }

        let mut first = crate::registry::ImageReader::open(&files[0])?;
        let base_meta = first.metadata().clone();
        let pattern = FilePattern::from_file(&files[0]).ok();
        let (meta, plane_map) = stitch_layout(&files, &base_meta, pattern.as_ref())?;
        let _ = first.close();

        Ok(FileStitcher {
            files,
            meta: Some(meta),
            plane_map,
            current_reader: None,
        })
    }

    /// Resolve a stitched plane index to (file_index, local_plane_index).
    fn resolve_plane(&self, plane_index: u32) -> Result<(usize, u32)> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        self.plane_map
            .get(plane_index as usize)
            .copied()
            .ok_or(BioFormatsError::PlaneOutOfRange(plane_index))
    }

    /// Ensure the reader for `file_idx` is open.
    fn ensure_reader(&mut self, file_idx: usize) -> Result<&mut Box<dyn FormatReader>> {
        if let Some((idx, _)) = &self.current_reader {
            if *idx == file_idx {
                return Ok(&mut self.current_reader.as_mut().unwrap().1);
            }
        }
        // Close current reader and open the new one
        if let Some((_, mut r)) = self.current_reader.take() {
            let _ = r.close();
        }
        let reader = open_reader(&self.files[file_idx])?;
        self.current_reader = Some((file_idx, reader));
        Ok(&mut self.current_reader.as_mut().unwrap().1)
    }
}

/// Open a format reader for the given file.
fn open_reader(path: &Path) -> Result<Box<dyn FormatReader>> {
    let header = crate::common::io::peek_header(path, 512).unwrap_or_default();
    for r in crate::registry::all_readers_pub() {
        if r.is_this_type_by_bytes(&header) {
            let mut r = r;
            r.set_id(path)?;
            return Ok(r);
        }
    }
    for r in crate::registry::all_readers_pub() {
        if r.is_this_type_by_name(path) {
            let mut r = r;
            r.set_id(path)?;
            return Ok(r);
        }
    }
    Err(BioFormatsError::UnsupportedFormat(
        path.display().to_string(),
    ))
}

/// Discover a file sequence from a single exemplar file.
///
/// Looks for numeric patterns in the filename and finds all matching files.
/// E.g., `img_001.tif` → looks for `img_000.tif`, `img_001.tif`, `img_002.tif`, ...
fn discover_sequence(path: &Path) -> Result<Vec<PathBuf>> {
    if let Ok(pattern) = FilePattern::from_file(path) {
        let mut files: Vec<PathBuf> = pattern
            .filenames()
            .into_iter()
            .filter(|p| p.exists())
            .collect();
        if !files.is_empty() {
            files.sort();
            return Ok(files);
        }
    }

    let parent = path.parent().unwrap_or(Path::new("."));
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| BioFormatsError::Format("Invalid filename".into()))?;
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");

    // Find the rightmost numeric run in the stem
    let chars: Vec<char> = stem.chars().collect();
    let num_end = chars.len();
    let mut num_start = num_end;
    // Walk backwards to find digits
    while num_start > 0 && chars[num_start - 1].is_ascii_digit() {
        num_start -= 1;
    }

    if num_start == num_end {
        // No numeric part — just return the single file
        return Ok(vec![path.to_path_buf()]);
    }

    let prefix: String = chars[..num_start].iter().collect();
    let suffix: String = chars[num_end..].iter().collect();
    let num_width = num_end - num_start;

    // List all files in the directory that match the pattern
    let entries = std::fs::read_dir(parent).map_err(BioFormatsError::Io)?;
    let mut matches: Vec<(u64, PathBuf)> = Vec::new();

    for entry in entries.flatten() {
        let entry_ext = entry
            .path()
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_string();

        if entry_ext != ext {
            continue;
        }

        let entry_stem = entry
            .path()
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();

        if !entry_stem.starts_with(&prefix) {
            continue;
        }
        if !entry_stem.ends_with(&suffix) {
            continue;
        }

        let mid = &entry_stem[prefix.len()..entry_stem.len() - suffix.len()];
        if mid.len() != num_width {
            continue;
        }
        if let Ok(n) = mid.parse::<u64>() {
            matches.push((n, entry.path()));
        }
    }

    matches.sort_by_key(|(n, _)| *n);
    Ok(matches.into_iter().map(|(_, p)| p).collect())
}

fn stitch_layout(
    files: &[PathBuf],
    base_meta: &ImageMetadata,
    pattern: Option<&FilePattern>,
) -> Result<(ImageMetadata, Vec<(usize, u32)>)> {
    let mut meta = base_meta.clone();
    let file_axes = pattern
        .and_then(|pattern| infer_file_axes(files, pattern, base_meta))
        .unwrap_or_else(|| FileAxisLayout {
            file_coords: (0..files.len()).map(|i| (i as u32, 0, 0)).collect(),
            size_z: files.len() as u32,
            size_c: 1,
            size_t: 1,
        });

    meta.size_z = checked_axis_mul(base_meta.size_z, file_axes.size_z, "Z")?;
    meta.size_c = checked_axis_mul(base_meta.size_c, file_axes.size_c, "C")?;
    meta.size_t = checked_axis_mul(base_meta.size_t, file_axes.size_t, "T")?;
    meta.image_count = meta
        .size_z
        .checked_mul(meta.size_c)
        .and_then(|v| v.checked_mul(meta.size_t))
        .ok_or_else(|| BioFormatsError::Format("Stitched plane count overflow".into()))?;

    let mut plane_map = vec![None; meta.image_count as usize];
    for (file_idx, &(file_z, file_c, file_t)) in file_axes.file_coords.iter().enumerate() {
        for local_plane in 0..base_meta.image_count {
            let (local_z, local_c, local_t) = plane_to_zct(local_plane, base_meta)
                .ok_or_else(|| BioFormatsError::Format("Invalid base plane index".into()))?;
            let z = file_z * base_meta.size_z + local_z;
            let c = file_c * base_meta.size_c + local_c;
            let t = file_t * base_meta.size_t + local_t;
            let stitched = zct_to_plane(z, c, t, &meta)
                .ok_or_else(|| BioFormatsError::Format("Invalid stitched plane index".into()))?;
            plane_map[stitched as usize] = Some((file_idx, local_plane));
        }
    }

    let plane_map = plane_map
        .into_iter()
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| BioFormatsError::Format("Incomplete stitched plane map".into()))?;
    Ok((meta, plane_map))
}

struct FileAxisLayout {
    file_coords: Vec<(u32, u32, u32)>,
    size_z: u32,
    size_c: u32,
    size_t: u32,
}

fn infer_file_axes(
    files: &[PathBuf],
    pattern: &FilePattern,
    base_meta: &ImageMetadata,
) -> Option<FileAxisLayout> {
    // Feed the reader's per-file dimension order and Z/T/C sizes into the
    // guesser so Java's step 2 (Z/T swap) and step 3 (size-aware back-fill)
    // apply. The single-file dimension order is uncertain for a stitched
    // dataset, so pass `is_certain = false`.
    let guess = AxisGuesser::guess_with_dims(
        pattern,
        dimension_order_str(base_meta.dimension_order),
        base_meta.size_z,
        base_meta.size_t,
        base_meta.size_c,
        false,
    );
    let guessed = guess.axis_types;
    if !guessed
        .iter()
        .any(|axis| matches!(axis, AxisType::Z | AxisType::Channel | AxisType::Time))
    {
        return None;
    }
    if has_duplicate_inferred_axis(&guessed) {
        return None;
    }

    let mut file_values = Vec::with_capacity(files.len());
    for file in files {
        let name = file.file_name()?.to_str()?;
        file_values.push(pattern.match_filename(name)?);
    }

    let axis_len = |axis_type| {
        guessed
            .iter()
            .position(|axis| *axis == axis_type)
            .map(|idx| pattern.blocks[idx].values.len() as u32)
            .unwrap_or(1)
    };
    let size_z = axis_len(AxisType::Z);
    let size_c = axis_len(AxisType::Channel);
    let size_t = axis_len(AxisType::Time);

    let mut file_coords = Vec::with_capacity(files.len());
    for values in file_values {
        let mut z = 0;
        let mut c = 0;
        let mut t = 0;
        for (idx, value) in values.iter().enumerate() {
            let ordinal = pattern.blocks[idx].values.iter().position(|v| v == value)? as u32;
            match guessed[idx] {
                AxisType::Z => z = ordinal,
                AxisType::Channel => c = ordinal,
                AxisType::Time => t = ordinal,
                AxisType::Series | AxisType::Unknown => {}
            }
        }
        file_coords.push((z, c, t));
    }

    Some(FileAxisLayout {
        file_coords,
        size_z,
        size_c,
        size_t,
    })
}

/// Map a [`DimensionOrder`] to its 5-character string form (e.g. "XYZCT"),
/// matching the `dimOrder` strings used by Java's AxisGuesser.
fn dimension_order_str(order: crate::common::metadata::DimensionOrder) -> &'static str {
    use crate::common::metadata::DimensionOrder::*;
    match order {
        XYCTZ => "XYCTZ",
        XYCZT => "XYCZT",
        XYTCZ => "XYTCZ",
        XYTZC => "XYTZC",
        XYZCT => "XYZCT",
        XYZTC => "XYZTC",
    }
}

fn has_duplicate_inferred_axis(axes: &[AxisType]) -> bool {
    [AxisType::Z, AxisType::Channel, AxisType::Time]
        .iter()
        .any(|axis| axes.iter().filter(|candidate| *candidate == axis).count() > 1)
}

fn checked_axis_mul(base: u32, files: u32, axis: &str) -> Result<u32> {
    base.checked_mul(files)
        .ok_or_else(|| BioFormatsError::Format(format!("Stitched {axis} size overflow")))
}

fn plane_to_zct(plane_index: u32, meta: &ImageMetadata) -> Option<(u32, u32, u32)> {
    for t in 0..meta.size_t {
        for z in 0..meta.size_z {
            for c in 0..meta.size_c {
                if zct_to_plane(z, c, t, meta)? == plane_index {
                    return Some((z, c, t));
                }
            }
        }
    }
    None
}

fn zct_to_plane(z: u32, c: u32, t: u32, meta: &ImageMetadata) -> Option<u32> {
    if z >= meta.size_z || c >= meta.size_c || t >= meta.size_t {
        return None;
    }
    Some(match meta.dimension_order {
        crate::common::metadata::DimensionOrder::XYZCT => {
            t * meta.size_z * meta.size_c + c * meta.size_z + z
        }
        crate::common::metadata::DimensionOrder::XYZTC => {
            c * meta.size_z * meta.size_t + t * meta.size_z + z
        }
        crate::common::metadata::DimensionOrder::XYCZT => {
            t * meta.size_c * meta.size_z + z * meta.size_c + c
        }
        crate::common::metadata::DimensionOrder::XYCTZ => {
            z * meta.size_c * meta.size_t + t * meta.size_c + c
        }
        crate::common::metadata::DimensionOrder::XYTCZ => {
            z * meta.size_t * meta.size_c + c * meta.size_t + t
        }
        crate::common::metadata::DimensionOrder::XYTZC => {
            c * meta.size_t * meta.size_z + z * meta.size_t + t
        }
    })
}

impl FormatReader for FileStitcher {
    fn is_this_type_by_name(&self, _path: &Path) -> bool {
        false
    }
    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        *self = Self::open(path)?;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        if let Some((_, mut r)) = self.current_reader.take() {
            let _ = r.close();
        }
        self.meta = None;
        self.files.clear();
        self.plane_map.clear();
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
        self.meta.as_ref().expect("FileStitcher not initialized")
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let (file_idx, local_plane) = self.resolve_plane(plane_index)?;
        let reader = self.ensure_reader(file_idx)?;
        reader.open_bytes(local_plane)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let (file_idx, local_plane) = self.resolve_plane(plane_index)?;
        let reader = self.ensure_reader(file_idx)?;
        reader.open_bytes_region(local_plane, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let (file_idx, local_plane) = self.resolve_plane(plane_index)?;
        let reader = self.ensure_reader(file_idx)?;
        reader.open_thumb_bytes(local_plane)
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// FilePattern — parse and match file name patterns
// ═══════════════════════════════════════════════════════════════════════════════

/// A parsed file pattern that can match and enumerate file sequences.
///
/// Equivalent to Java Bio-Formats' `FilePattern` class.
///
/// # Pattern Syntax
/// Given a filename like `img_t003_c002.tif`, FilePattern detects numeric
/// blocks and generates the pattern `img_t<000-NNN>_c<000-MMM>.tif`.
#[derive(Debug, Clone)]
pub struct FilePattern {
    /// Directory containing the files.
    pub dir: PathBuf,
    /// Prefix before the first numeric block.
    pub prefix: String,
    /// Suffix after the last numeric block (including extension).
    pub suffix: String,
    /// Numeric blocks: (prefix_before_block, digit_width, min_value, max_value).
    pub blocks: Vec<FilePatternBlock>,
}

/// One numeric block in a file pattern.
#[derive(Debug, Clone)]
pub struct FilePatternBlock {
    /// Text between this block and the previous one (or start of name).
    pub separator: String,
    /// Number of digits (for zero-padding).
    pub width: usize,
    /// Range of values found.
    pub min: u64,
    pub max: u64,
    /// All values found (sorted).
    pub values: Vec<u64>,
}

impl FilePattern {
    /// Parse a file pattern from a single exemplar file.
    pub fn from_file(path: &Path) -> Result<Self> {
        let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
        let filename = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| BioFormatsError::Format("Invalid filename".into()))?;

        // Find all numeric runs in the filename
        let chars: Vec<char> = filename.chars().collect();
        let mut runs: Vec<(usize, usize)> = Vec::new(); // (start, end) of each numeric run
        let mut i = 0;
        while i < chars.len() {
            if chars[i].is_ascii_digit() {
                let start = i;
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
                runs.push((start, i));
            } else {
                i += 1;
            }
        }

        if runs.is_empty() {
            return Ok(FilePattern {
                dir,
                prefix: filename.to_string(),
                suffix: String::new(),
                blocks: Vec::new(),
            });
        }

        // Build blocks
        let mut blocks = Vec::new();
        let mut last_end = 0;
        for &(start, end) in &runs {
            let separator: String = chars[last_end..start].iter().collect();
            let width = end - start;
            let val_str: String = chars[start..end].iter().collect();
            let val: u64 = val_str.parse().unwrap_or(0);
            blocks.push(FilePatternBlock {
                separator,
                width,
                min: val,
                max: val,
                values: vec![val],
            });
            last_end = end;
        }
        let suffix: String = chars[last_end..].iter().collect();
        let prefix = String::new(); // prefix is captured in first block's separator

        // Scan directory to find all matching files and expand ranges
        let mut pattern = FilePattern {
            dir: dir.clone(),
            prefix,
            suffix: suffix.clone(),
            blocks,
        };
        pattern.scan_directory()?;
        Ok(pattern)
    }

    /// Scan the directory and expand block ranges based on found files.
    fn scan_directory(&mut self) -> Result<()> {
        let entries = std::fs::read_dir(&self.dir).map_err(BioFormatsError::Io)?;

        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();

            if !name_str.ends_with(&self.suffix) {
                continue;
            }

            // Try to match each block
            if let Some(values) = self.match_filename(&name_str) {
                for (i, val) in values.into_iter().enumerate() {
                    if i < self.blocks.len() {
                        let block = &mut self.blocks[i];
                        if val < block.min {
                            block.min = val;
                        }
                        if val > block.max {
                            block.max = val;
                        }
                        if !block.values.contains(&val) {
                            block.values.push(val);
                        }
                    }
                }
            }
        }

        // Sort values
        for block in &mut self.blocks {
            block.values.sort();
        }
        Ok(())
    }

    /// Try to extract numeric values from a filename that matches this pattern.
    fn match_filename(&self, name: &str) -> Option<Vec<u64>> {
        let mut pos = 0;
        let mut values = Vec::new();
        for block in &self.blocks {
            // Match separator
            if !name[pos..].starts_with(&block.separator) {
                return None;
            }
            pos += block.separator.len();
            // Extract digits
            let digit_start = pos;
            while pos < name.len() && name.as_bytes()[pos].is_ascii_digit() {
                pos += 1;
            }
            if pos == digit_start {
                return None;
            }
            let val: u64 = name[digit_start..pos].parse().ok()?;
            values.push(val);
        }
        // Match suffix
        if &name[pos..] != self.suffix {
            return None;
        }
        Some(values)
    }

    /// Generate all filenames matching this pattern.
    pub fn filenames(&self) -> Vec<PathBuf> {
        if self.blocks.is_empty() {
            return vec![self.dir.join(format!("{}{}", self.prefix, self.suffix))];
        }
        self.enumerate_blocks(0, String::new())
    }

    fn enumerate_blocks(&self, block_idx: usize, current: String) -> Vec<PathBuf> {
        if block_idx >= self.blocks.len() {
            let name = format!("{}{}", current, self.suffix);
            return vec![self.dir.join(name)];
        }
        let block = &self.blocks[block_idx];
        let mut results = Vec::new();
        for &val in &block.values {
            let next = format!(
                "{}{}{:0>width$}",
                current,
                block.separator,
                val,
                width = block.width
            );
            results.extend(self.enumerate_blocks(block_idx + 1, next));
        }
        results
    }

    /// Total number of files in this pattern.
    pub fn file_count(&self) -> usize {
        self.blocks
            .iter()
            .map(|b| b.values.len())
            .product::<usize>()
            .max(1)
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// AxisGuesser — heuristic dimension detection from filenames
// ═══════════════════════════════════════════════════════════════════════════════

/// Axis type for a numeric block in a filename pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AxisType {
    Z,
    Channel,
    Time,
    Series,
    Unknown,
}

/// Result of running the axis guesser: the axis type for each pattern block,
/// plus the (possibly Z/T-swapped) adjusted within-file dimension order.
///
/// Mirrors the relevant outputs of Java `AxisGuesser` (`getAxisTypes`,
/// `getAdjustedOrder`, `isCertain`).
#[derive(Debug, Clone)]
pub struct AxisGuess {
    /// Guessed axis type per pattern block.
    pub axis_types: Vec<AxisType>,
    /// Adjusted within-file dimension order (as a 5-char string like "XYZCT").
    pub adjusted_order: String,
    /// Whether the guess is confident.
    pub certain: bool,
}

/// Heuristic axis guesser — infers which dimension each numeric block in a
/// filename pattern represents.
///
/// Equivalent to Java Bio-Formats' `AxisGuesser`.
pub struct AxisGuesser;

// Known prefix sets, matched exactly against the trailing alphabetic segment of
// a block's preceding text. Mirrors AxisGuesser.{Z,T,C,S}_PREFIXES in Java.
const Z_PREFIXES: &[&str] = &["fp", "sec", "z", "zs", "focal", "focalplane"];
const T_PREFIXES: &[&str] = &["t", "tl", "tp", "time"];
const C_PREFIXES: &[&str] = &["c", "ch", "w", "wavelength"];
const S_PREFIXES: &[&str] = &["s", "series", "sp"];

impl AxisGuesser {
    /// Guess axis types for each block in a FilePattern, without per-file
    /// dimension sizes.
    ///
    /// This is the convenience entry point: it assumes each within-file
    /// dimension has size 1 (so every axis is "free" for back-filling) and an
    /// uncertain dimension order. Under those assumptions Java's step 2 (Z/T
    /// swap) never fires (it requires `sizeZ > 1` or `sizeT > 1`), so this is
    /// equivalent to running the full guesser with `sizeZ = sizeT = sizeC = 1`.
    /// Returns just the axis types; callers needing the adjusted order should
    /// use [`AxisGuesser::guess_with_dims`].
    pub fn guess(pattern: &FilePattern) -> Vec<AxisType> {
        Self::guess_with_dims(pattern, "XYZCT", 1, 1, 1, false).axis_types
    }

    /// Full port of Java `AxisGuesser`'s constructor (steps 1–3), using the
    /// per-file dimension order and Z/T/C sizes from the reader.
    ///
    /// - Step 1: assign each block from its known prefix (Z/T/C/S prefix sets).
    /// - Step 2 (`AxisGuesser.java` lines ~242-257): if the order is uncertain
    ///   and exactly one of Z/T was found as a prefix while that dimension has
    ///   size > 1 within each file (and the other has size 1), swap Z and T in
    ///   the adjusted order and swap the working `sizeZ`/`sizeT`.
    /// - Step 3 (lines ~259-293): back-fill remaining UNKNOWN blocks into the
    ///   first free Z/T/C dimension (free = not found AND size == 1), otherwise
    ///   onto the last axis of the (adjusted) order.
    ///
    /// `dim_order` is a string such as "XYZCT"; only the relative positions of
    /// 'Z', 'T' and 'C' (and the final axis) are used.
    pub fn guess_with_dims(
        pattern: &FilePattern,
        dim_order: &str,
        size_z: u32,
        size_t: u32,
        size_c: u32,
        is_certain: bool,
    ) -> AxisGuess {
        let mut axis_types: Vec<AxisType> = pattern
            .blocks
            .iter()
            .map(|block| Self::guess_from_separator(&block.separator))
            .collect();

        let found_z = axis_types.iter().any(|a| *a == AxisType::Z);
        let found_t = axis_types.iter().any(|a| *a == AxisType::Time);
        let found_c = axis_types.iter().any(|a| *a == AxisType::Channel);

        // -- 2) check for special cases where dimension order should be swapped --
        let mut new_order: Vec<char> = dim_order.chars().collect();
        let mut size_z = size_z;
        let mut size_t = size_t;
        if !is_certain
            && ((found_z && !found_t && size_z > 1 && size_t == 1)
                || (found_t && !found_z && size_t > 1 && size_z == 1))
        {
            // swap Z and T dimensions in the adjusted order
            if let (Some(index_z), Some(index_t)) = (
                new_order.iter().position(|&c| c == 'Z'),
                new_order.iter().position(|&c| c == 'T'),
            ) {
                new_order[index_z] = 'T';
                new_order[index_t] = 'Z';
            }
            std::mem::swap(&mut size_z, &mut size_t);
        }

        // -- 3) fill in remaining axis types --
        let mut can_be_z = !found_z && size_z == 1;
        let mut can_be_t = !found_t && size_t == 1;
        let mut can_be_c = !found_c && size_c == 1;
        let mut certain = is_certain;

        let last_axis = new_order.last().copied().unwrap_or('T');

        for axis in axis_types.iter_mut() {
            if *axis != AxisType::Unknown {
                continue;
            }
            certain = false;
            if can_be_z {
                *axis = AxisType::Z;
                can_be_z = false;
            } else if can_be_t {
                *axis = AxisType::Time;
                can_be_t = false;
            } else if can_be_c {
                *axis = AxisType::Channel;
                can_be_c = false;
            } else {
                *axis = match last_axis {
                    'C' => AxisType::Channel,
                    'Z' => AxisType::Z,
                    _ => AxisType::Time,
                };
            }
        }

        AxisGuess {
            axis_types,
            adjusted_order: new_order.into_iter().collect(),
            certain,
        }
    }

    /// Infer axis type from the text preceding a numeric block, matching the
    /// trailing alphanumeric segment against the exact Java prefix sets.
    fn guess_from_separator(sep: &str) -> AxisType {
        let p = Self::trailing_segment(sep);
        if Z_PREFIXES.contains(&p.as_str()) {
            return AxisType::Z;
        }
        if T_PREFIXES.contains(&p.as_str()) {
            return AxisType::Time;
        }
        if C_PREFIXES.contains(&p.as_str()) {
            return AxisType::Channel;
        }
        if S_PREFIXES.contains(&p.as_str()) {
            return AxisType::Series;
        }
        AxisType::Unknown
    }

    /// Extract the "useful prefix segment": lowercase the text, strip trailing
    /// digits and divider characters (space, '-', '_', '.'), then take the run
    /// of trailing ASCII letters. Mirrors AxisGuesser's char-array walk.
    fn trailing_segment(sep: &str) -> String {
        let ch: Vec<char> = sep.to_ascii_lowercase().chars().collect();
        if ch.is_empty() {
            return String::new();
        }
        let mut l: isize = ch.len() as isize - 1;
        while l >= 0 {
            let c = ch[l as usize];
            if c.is_ascii_digit() || c == ' ' || c == '-' || c == '_' || c == '.' {
                l -= 1;
            } else {
                break;
            }
        }
        let mut f: isize = l;
        while f >= 0 && ch[f as usize].is_ascii_lowercase() {
            f -= 1;
        }
        if l < 0 || f + 1 > l {
            return String::new();
        }
        ch[(f + 1) as usize..=(l as usize)].iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a FilePattern with one block per `(separator, value_count)` pair,
    /// bypassing directory scanning.
    fn pattern(blocks: &[(&str, usize)]) -> FilePattern {
        FilePattern {
            dir: PathBuf::from("."),
            prefix: String::new(),
            suffix: ".tif".into(),
            blocks: blocks
                .iter()
                .map(|(sep, n)| FilePatternBlock {
                    separator: (*sep).into(),
                    width: 3,
                    min: 0,
                    max: (*n as u64).saturating_sub(1),
                    values: (0..*n as u64).collect(),
                })
                .collect(),
        }
    }

    #[test]
    fn zt_swap_when_only_z_found_and_size_z_gt_one() {
        // Java AxisGuesser step 2: pattern "z<*>_<*>" with sizes {Z,T,C}=2,1,1
        // and uncertain order => Z/T are swapped in the adjusted order, and the
        // unknown second block becomes C (since after the swap canBeT is gone,
        // sizeT becomes 2 so the only free dimension is C).
        let fp = pattern(&[("z", 2), ("_", 2)]);
        let guess = AxisGuesser::guess_with_dims(&fp, "XYZCT", 2, 1, 1, false);
        // newOrder has Z and T swapped: XYZCT -> XYTCT? No: only the Z and T
        // chars swap positions, giving "XYTCZ".
        assert_eq!(guess.adjusted_order, "XYTCZ");
        // First block was matched as Z by prefix.
        assert_eq!(guess.axis_types[0], AxisType::Z);
        // After the swap, working sizeZ=1 (was T) so canBeZ is false (foundZ),
        // canBeT is false (working sizeT=2), canBeC true -> second block = C.
        assert_eq!(guess.axis_types[1], AxisType::Channel);
        assert!(!guess.certain);
    }

    #[test]
    fn no_swap_when_order_certain() {
        let fp = pattern(&[("z", 2), ("_", 2)]);
        let guess = AxisGuesser::guess_with_dims(&fp, "XYZCT", 2, 1, 1, true);
        assert_eq!(guess.adjusted_order, "XYZCT");
        assert_eq!(guess.axis_types[0], AxisType::Z);
        // Order certain so step 2 skipped; sizeZ=2 (foundZ), sizeT=1 so the
        // unknown block back-fills to T.
        assert_eq!(guess.axis_types[1], AxisType::Time);
    }

    #[test]
    fn no_swap_when_both_sizes_one() {
        // With all sizes 1 (the convenience `guess` case) step 2 never fires.
        let fp = pattern(&[("t", 2), ("_", 2)]);
        let guess = AxisGuesser::guess_with_dims(&fp, "XYZCT", 1, 1, 1, false);
        assert_eq!(guess.adjusted_order, "XYZCT");
        assert_eq!(guess.axis_types[0], AxisType::Time);
        // foundT, sizeZ==1 -> unknown block back-fills to Z first.
        assert_eq!(guess.axis_types[1], AxisType::Z);
    }

    #[test]
    fn unknown_blocks_backfill_then_last_axis() {
        // Three unknown blocks, all sizes 1: back-fill Z, T, C in order.
        let fp = pattern(&[("a", 2), ("b", 2), ("d", 2)]);
        let guess = AxisGuesser::guess_with_dims(&fp, "XYZCT", 1, 1, 1, false);
        assert_eq!(guess.axis_types[0], AxisType::Z);
        assert_eq!(guess.axis_types[1], AxisType::Time);
        assert_eq!(guess.axis_types[2], AxisType::Channel);
    }
}
