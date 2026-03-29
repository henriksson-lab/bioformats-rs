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
            return Err(BioFormatsError::Format("No files found for stitching".into()));
        }

        // Open the first file to get base metadata
        let mut first = crate::registry::ImageReader::open(&files[0])?;
        let base_meta = first.metadata().clone();

        let plane_count = files.len() as u32 * base_meta.image_count;
        let meta = ImageMetadata {
            image_count: plane_count,
            size_z: base_meta.size_z * files.len() as u32,
            ..base_meta
        };
        let _ = first.close();

        Ok(FileStitcher {
            files,
            meta: Some(meta),
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
        let planes_per_file = base_meta.image_count;
        let plane_count = files.len() as u32 * planes_per_file;
        let meta = ImageMetadata {
            image_count: plane_count,
            size_z: base_meta.size_z * files.len() as u32,
            ..base_meta
        };
        let _ = first.close();

        Ok(FileStitcher {
            files,
            meta: Some(meta),
            current_reader: None,
        })
    }

    /// Resolve a stitched plane index to (file_index, local_plane_index).
    fn resolve_plane(&self, plane_index: u32) -> Result<(usize, u32)> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        // Each file has the same number of planes
        let planes_per_file = meta.image_count / self.files.len() as u32;
        let file_idx = (plane_index / planes_per_file) as usize;
        let local_plane = plane_index % planes_per_file;
        Ok((file_idx, local_plane))
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
    Err(BioFormatsError::UnsupportedFormat(path.display().to_string()))
}

/// Discover a file sequence from a single exemplar file.
///
/// Looks for numeric patterns in the filename and finds all matching files.
/// E.g., `img_001.tif` → looks for `img_000.tif`, `img_001.tif`, `img_002.tif`, ...
fn discover_sequence(path: &Path) -> Result<Vec<PathBuf>> {
    let parent = path.parent().unwrap_or(Path::new("."));
    let stem = path.file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| BioFormatsError::Format("Invalid filename".into()))?;
    let ext = path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");

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
        let entry_ext = entry.path().extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_string();

        if entry_ext != ext { continue; }

        let entry_stem = entry.path().file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();

        if !entry_stem.starts_with(&prefix) { continue; }
        if !entry_stem.ends_with(&suffix) { continue; }

        let mid = &entry_stem[prefix.len()..entry_stem.len() - suffix.len()];
        if mid.len() != num_width { continue; }
        if let Ok(n) = mid.parse::<u64>() {
            matches.push((n, entry.path()));
        }
    }

    matches.sort_by_key(|(n, _)| *n);
    Ok(matches.into_iter().map(|(_, p)| p).collect())
}

impl FormatReader for FileStitcher {
    fn is_this_type_by_name(&self, _path: &Path) -> bool { false }
    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool { false }

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
        Ok(())
    }

    fn series_count(&self) -> usize { 1 }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if s != 0 { Err(BioFormatsError::SeriesOutOfRange(s)) } else { Ok(()) }
    }

    fn series(&self) -> usize { 0 }

    fn metadata(&self) -> &ImageMetadata {
        self.meta.as_ref().expect("FileStitcher not initialized")
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let (file_idx, local_plane) = self.resolve_plane(plane_index)?;
        let reader = self.ensure_reader(file_idx)?;
        reader.open_bytes(local_plane)
    }

    fn open_bytes_region(&mut self, plane_index: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
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
        let filename = path.file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| BioFormatsError::Format("Invalid filename".into()))?;

        // Find all numeric runs in the filename
        let chars: Vec<char> = filename.chars().collect();
        let mut runs: Vec<(usize, usize)> = Vec::new(); // (start, end) of each numeric run
        let mut i = 0;
        while i < chars.len() {
            if chars[i].is_ascii_digit() {
                let start = i;
                while i < chars.len() && chars[i].is_ascii_digit() { i += 1; }
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
        let mut pattern = FilePattern { dir: dir.clone(), prefix, suffix: suffix.clone(), blocks };
        pattern.scan_directory()?;
        Ok(pattern)
    }

    /// Scan the directory and expand block ranges based on found files.
    fn scan_directory(&mut self) -> Result<()> {
        let entries = std::fs::read_dir(&self.dir).map_err(BioFormatsError::Io)?;

        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();

            if !name_str.ends_with(&self.suffix) { continue; }

            // Try to match each block
            if let Some(values) = self.match_filename(&name_str) {
                for (i, val) in values.into_iter().enumerate() {
                    if i < self.blocks.len() {
                        let block = &mut self.blocks[i];
                        if val < block.min { block.min = val; }
                        if val > block.max { block.max = val; }
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
            if !name[pos..].starts_with(&block.separator) { return None; }
            pos += block.separator.len();
            // Extract digits
            let digit_start = pos;
            while pos < name.len() && name.as_bytes()[pos].is_ascii_digit() { pos += 1; }
            if pos == digit_start { return None; }
            let val: u64 = name[digit_start..pos].parse().ok()?;
            values.push(val);
        }
        // Match suffix
        if &name[pos..] != self.suffix { return None; }
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
            let next = format!("{}{}{:0>width$}", current, block.separator, val, width = block.width);
            results.extend(self.enumerate_blocks(block_idx + 1, next));
        }
        results
    }

    /// Total number of files in this pattern.
    pub fn file_count(&self) -> usize {
        self.blocks.iter().map(|b| b.values.len()).product::<usize>().max(1)
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

/// Heuristic axis guesser — infers which dimension each numeric block in a
/// filename pattern represents.
///
/// Equivalent to Java Bio-Formats' `AxisGuesser`.
pub struct AxisGuesser;

impl AxisGuesser {
    /// Guess axis types for each block in a FilePattern.
    pub fn guess(pattern: &FilePattern) -> Vec<AxisType> {
        pattern.blocks.iter().map(|block| {
            Self::guess_from_separator(&block.separator)
        }).collect()
    }

    /// Infer axis type from the text preceding a numeric block.
    fn guess_from_separator(sep: &str) -> AxisType {
        let lower = sep.to_ascii_lowercase();
        // Check for common axis indicators
        if lower.contains('z') || lower.contains("slice") || lower.contains("depth")
            || lower.contains("sec") || lower.contains("plane") {
            return AxisType::Z;
        }
        if lower.contains('c') && !lower.contains("tc")
            || lower.contains("ch") || lower.contains("channel")
            || lower.contains("wave") || lower.contains("lambda") {
            return AxisType::Channel;
        }
        if lower.contains('t') || lower.contains("time") || lower.contains("frame")
            || lower.contains("tp") || lower.contains("point") {
            return AxisType::Time;
        }
        if lower.contains('s') || lower.contains("series") || lower.contains("pos")
            || lower.contains("stage") || lower.contains("well") || lower.contains("field") {
            return AxisType::Series;
        }
        AxisType::Unknown
    }
}
