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
use crate::common::metadata::{ImageMetadata, MetadataValue};
use crate::common::ome_metadata::OmeMetadata;
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
        let pattern = FilePattern::from_file_list(&files).ok();
        let (meta, plane_map) = stitch_layout(&files, &base_meta, pattern.as_ref())?;
        let _ = first.close();

        Ok(FileStitcher {
            files,
            meta: Some(meta),
            plane_map,
            current_reader: None,
        })
    }

    /// Open with an explicit file list and the `.pattern` text that produced it.
    pub fn from_files_with_pattern(files: Vec<PathBuf>, pattern_path: &Path) -> Result<Self> {
        if files.is_empty() {
            return Err(BioFormatsError::Format("Empty file list".into()));
        }

        let mut first = crate::registry::ImageReader::open(&files[0])?;
        let base_meta = first.metadata().clone();
        let pattern = FilePattern::from_explicit_pattern(pattern_path)?;
        let (meta, plane_map) = stitch_layout(&files, &base_meta, Some(&pattern))?;
        let _ = first.close();

        Ok(FileStitcher {
            files,
            meta: Some(meta),
            plane_map,
            current_reader: None,
        })
    }

    /// Open with an explicit file list and an already-parsed file pattern.
    pub(crate) fn from_files_with_file_pattern(
        files: Vec<PathBuf>,
        pattern: FilePattern,
    ) -> Result<Self> {
        if files.is_empty() {
            return Err(BioFormatsError::Format("Empty file list".into()));
        }

        let mut first = crate::registry::ImageReader::open(&files[0])?;
        let base_meta = first.metadata().clone();
        let (meta, plane_map) = stitch_layout(&files, &base_meta, Some(&pattern))?;
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
            axis_types: pattern.map(AxisGuesser::guess).unwrap_or_default(),
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
    if let Some(pattern) = pattern {
        annotate_file_pattern_metadata(&mut meta, pattern, &file_axes);
    }
    Ok((meta, plane_map))
}

struct FileAxisLayout {
    file_coords: Vec<(u32, u32, u32)>,
    size_z: u32,
    size_c: u32,
    size_t: u32,
    axis_types: Vec<AxisType>,
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
        let name = pattern.match_text_for_path(file)?;
        file_values.push(pattern.match_filename(&name)?);
    }

    let axis_len = |axis_type| {
        guessed
            .iter()
            .position(|axis| *axis == axis_type)
            .map(|idx| pattern.blocks[idx].value_labels().len() as u32)
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
            let ordinal = pattern.blocks[idx].position_of(value)? as u32;
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
        axis_types: guessed,
    })
}

fn annotate_file_pattern_metadata(
    meta: &mut ImageMetadata,
    pattern: &FilePattern,
    file_axes: &FileAxisLayout,
) {
    if let Some(source_pattern) = &pattern.source_pattern {
        meta.series_metadata.insert(
            "FilePattern pattern".to_string(),
            MetadataValue::String(source_pattern.clone()),
        );
        meta.series_metadata.insert(
            "FilePattern Pattern".to_string(),
            MetadataValue::String(source_pattern.clone()),
        );
        meta.series_metadata.insert(
            "File pattern".to_string(),
            MetadataValue::String(source_pattern.clone()),
        );
        meta.series_metadata.insert(
            "FilePattern".to_string(),
            MetadataValue::String(source_pattern.clone()),
        );
    }
    if let Some(source_root) = &pattern.source_root {
        let source_root = source_root.display().to_string();
        meta.series_metadata.insert(
            "FilePattern root".to_string(),
            MetadataValue::String(source_root.clone()),
        );
        meta.series_metadata.insert(
            "FilePattern Root".to_string(),
            MetadataValue::String(source_root),
        );
    }
    meta.series_metadata.insert(
        "FilePattern file count".to_string(),
        MetadataValue::Int(file_axes.file_coords.len() as i64),
    );
    meta.series_metadata.insert(
        "FilePattern File Count".to_string(),
        MetadataValue::Int(file_axes.file_coords.len() as i64),
    );

    let axes = file_axes
        .axis_types
        .iter()
        .map(|axis| axis.as_str())
        .collect::<Vec<_>>()
        .join(",");
    meta.series_metadata.insert(
        "FilePattern axes".to_string(),
        MetadataValue::String(axes.clone()),
    );
    meta.series_metadata
        .insert("FilePattern Axes".to_string(), MetadataValue::String(axes));
    meta.series_metadata.insert(
        "FilePattern block count".to_string(),
        MetadataValue::Int(pattern.blocks.len() as i64),
    );
    meta.series_metadata.insert(
        "FilePattern Block Count".to_string(),
        MetadataValue::Int(pattern.blocks.len() as i64),
    );

    for (idx, block) in pattern.blocks.iter().enumerate() {
        let axis = file_axes
            .axis_types
            .get(idx)
            .copied()
            .unwrap_or(AxisType::Unknown)
            .as_str();
        meta.series_metadata.insert(
            format!("FilePattern block {idx} axis"),
            MetadataValue::String(axis.to_string()),
        );
        meta.series_metadata.insert(
            format!("FilePattern Block {idx} Axis"),
            MetadataValue::String(axis.to_string()),
        );
        meta.series_metadata.insert(
            format!("FilePattern Axis {idx} Type"),
            MetadataValue::String(axis.to_string()),
        );
        meta.series_metadata.insert(
            format!("Axis {idx} Type"),
            MetadataValue::String(axis.to_string()),
        );
        if let Some(token) = &block.token {
            meta.series_metadata.insert(
                format!("FilePattern block {idx} token"),
                MetadataValue::String(token.clone()),
            );
            meta.series_metadata.insert(
                format!("FilePattern Block {idx} Token"),
                MetadataValue::String(token.clone()),
            );
            meta.series_metadata.insert(
                format!("FilePattern Axis {idx} Token"),
                MetadataValue::String(token.clone()),
            );
            meta.series_metadata.insert(
                format!("Axis {idx} Token"),
                MetadataValue::String(token.clone()),
            );
        }
        let values = block.value_labels().join(",");
        meta.series_metadata.insert(
            format!("FilePattern block {idx} values"),
            MetadataValue::String(values.clone()),
        );
        meta.series_metadata.insert(
            format!("FilePattern Block {idx} Values"),
            MetadataValue::String(values.clone()),
        );
        meta.series_metadata.insert(
            format!("FilePattern Axis {idx} Values"),
            MetadataValue::String(values.clone()),
        );
        meta.series_metadata
            .insert(format!("Axis {idx} Values"), MetadataValue::String(values));
        let value_count = block.value_labels().len() as i64;
        meta.series_metadata.insert(
            format!("FilePattern block {idx} count"),
            MetadataValue::Int(value_count),
        );
        meta.series_metadata.insert(
            format!("FilePattern Block {idx} Count"),
            MetadataValue::Int(value_count),
        );
        meta.series_metadata.insert(
            format!("FilePattern Axis {idx} Size"),
            MetadataValue::Int(value_count),
        );
        meta.series_metadata
            .insert(format!("Axis {idx} Size"), MetadataValue::Int(value_count));
    }

    for (block_idx, block) in pattern.blocks.iter().enumerate() {
        if file_axes.axis_types.get(block_idx) != Some(&AxisType::Channel) {
            continue;
        }
        for (channel_idx, label) in block.value_labels().iter().enumerate() {
            meta.series_metadata.insert(
                format!("FilePattern channel {channel_idx} name"),
                MetadataValue::String(label.clone()),
            );
            meta.series_metadata.insert(
                format!("FilePattern Channel {channel_idx} Name"),
                MetadataValue::String(label.clone()),
            );
            meta.series_metadata.insert(
                format!("Channel {channel_idx} Name"),
                MetadataValue::String(label.clone()),
            );
            meta.series_metadata.insert(
                format!("Channel:{channel_idx}:Name"),
                MetadataValue::String(label.clone()),
            );
            meta.series_metadata.insert(
                format!("FilePattern Channel {channel_idx} Label"),
                MetadataValue::String(label.clone()),
            );
        }
    }
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

    fn ome_metadata(&self) -> Option<OmeMetadata> {
        let meta = self.meta.as_ref()?;
        let mut ome = OmeMetadata::from_image_metadata(meta);
        let image = ome.images.get_mut(0)?;
        for (idx, channel) in image.channels.iter_mut().enumerate() {
            if let Some(MetadataValue::String(name)) =
                meta.series_metadata.get(&format!("Channel {idx} Name"))
            {
                channel.name = Some(name.clone());
            }
        }
        Some(ome)
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
    /// Exact resolved pattern text when the pattern came from a bounded
    /// `.pattern` expansion.
    source_pattern: Option<String>,
    /// Root directory used for resolving the source pattern.
    source_root: Option<PathBuf>,
    /// Whether matching/enumeration uses the full pattern path instead of only
    /// a filename relative to `dir`.
    full_path_pattern: bool,
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
    /// Exact block token from an explicit bounded pattern or glob wildcard.
    pub token: Option<String>,
    /// Number of digits (for zero-padding).
    pub width: usize,
    /// Range of values found.
    pub min: u64,
    pub max: u64,
    /// All values found (sorted).
    pub values: Vec<u64>,
    /// Exact explicit labels from `.pattern` blocks, including non-numeric
    /// channel names. Numeric-only auto-discovered patterns leave this unset.
    pub labels: Option<Vec<String>>,
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
                source_pattern: None,
                source_root: None,
                full_path_pattern: false,
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
                token: None,
                width,
                min: val,
                max: val,
                values: vec![val],
                labels: None,
            });
            last_end = end;
        }
        let suffix: String = chars[last_end..].iter().collect();
        let prefix = String::new(); // prefix is captured in first block's separator

        // Scan directory to find all matching files and expand ranges
        let mut pattern = FilePattern {
            dir: dir.clone(),
            source_pattern: None,
            source_root: None,
            full_path_pattern: false,
            prefix,
            suffix: suffix.clone(),
            blocks,
        };
        pattern.scan_directory()?;
        Ok(pattern)
    }

    /// Parse a file pattern from an explicit list of files.
    ///
    /// This is used by `.pattern` files: Java `FilePatternReader` expands the
    /// requested pattern first, then stitches exactly that set. Directory-wide
    /// scanning would accidentally pull unrelated files into an explicit
    /// pattern, so keep the numeric value table bounded to the supplied list.
    pub fn from_file_list(files: &[PathBuf]) -> Result<Self> {
        let first = files
            .first()
            .ok_or_else(|| BioFormatsError::Format("Empty file list".into()))?;
        let mut pattern = Self::from_file_shape(first)?;
        pattern.scan_file_list(files)?;
        Ok(pattern)
    }

    fn from_file_shape(path: &Path) -> Result<Self> {
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
                source_pattern: None,
                source_root: None,
                full_path_pattern: false,
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
                token: None,
                width,
                min: val,
                max: val,
                values: vec![val],
                labels: None,
            });
            last_end = end;
        }
        let suffix: String = chars[last_end..].iter().collect();
        let prefix = String::new(); // prefix is captured in first block's separator

        Ok(FilePattern {
            dir,
            source_pattern: None,
            source_root: None,
            full_path_pattern: false,
            prefix,
            suffix,
            blocks,
        })
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
                        let Some(val) = val.parse::<u64>().ok() else {
                            continue;
                        };
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

    fn scan_file_list(&mut self, files: &[PathBuf]) -> Result<()> {
        for block in &mut self.blocks {
            if block.labels.is_none() {
                block.values.clear();
            }
        }

        for file in files {
            let name = self
                .match_text_for_path(file)
                .ok_or_else(|| BioFormatsError::Format("Invalid filename".into()))?;
            let values = self.match_filename(&name).ok_or_else(|| {
                BioFormatsError::UnsupportedFormat(format!(
                    "FilePattern: explicit files do not share one numeric pattern: {name}"
                ))
            })?;
            for (i, val) in values.into_iter().enumerate() {
                let block = &mut self.blocks[i];
                let Some(val) = val.parse::<u64>().ok() else {
                    continue;
                };
                if block.values.is_empty() {
                    block.min = val;
                    block.max = val;
                } else {
                    block.min = block.min.min(val);
                    block.max = block.max.max(val);
                }
                if !block.values.contains(&val) {
                    block.values.push(val);
                }
            }
        }

        for block in &mut self.blocks {
            block.values.sort();
        }
        Ok(())
    }

    /// Parse an explicit `.pattern` path containing one or more `<...>`,
    /// `[...]`, or `{...}` blocks.
    pub fn from_explicit_pattern(path: &Path) -> Result<Self> {
        let text = path
            .to_str()
            .ok_or_else(|| BioFormatsError::Format("Invalid filename".into()))?;

        let mut blocks = Vec::new();
        let mut suffix_start = 0usize;
        let mut rest = text;
        while let Some((start_rel, open, close)) = find_next_explicit_pattern_block(rest) {
            let start = suffix_start + start_rel;
            let after_start = start + open.len_utf8();
            let end = find_matching_explicit_pattern_block_end(text, start, open, close)?;
            let separator = text[suffix_start..start].to_string();
            let token = text[start..end + close.len_utf8()].to_string();
            let labels = parse_explicit_pattern_block(&text[after_start..end])?;
            let numeric_values: Vec<u64> = labels
                .iter()
                .filter_map(|label| label.parse::<u64>().ok())
                .collect();
            let width = labels.iter().map(|label| label.len()).max().unwrap_or(1);
            blocks.push(FilePatternBlock {
                separator,
                token: Some(token),
                width,
                min: numeric_values.iter().copied().min().unwrap_or(0),
                max: numeric_values.iter().copied().max().unwrap_or(0),
                values: numeric_values,
                labels: Some(labels),
            });
            suffix_start = end + close.len_utf8();
            rest = &text[suffix_start..];
        }

        if blocks.is_empty() {
            return Self::from_file_shape(path);
        }

        Ok(FilePattern {
            dir: PathBuf::new(),
            source_pattern: Some(text.to_string()),
            source_root: source_pattern_root(Path::new(text)),
            full_path_pattern: true,
            prefix: String::new(),
            suffix: text[suffix_start..].to_string(),
            blocks,
        })
    }

    /// Parse a simple `*`/`?` glob pattern after it has been expanded.
    ///
    /// Each contiguous wildcard run becomes one explicit-label block whose
    /// values are bounded to the matched files. This is intentionally only used
    /// for `.pattern` glob expansion, not for directory-wide auto-discovery.
    pub(crate) fn from_expanded_glob(pattern: &Path, files: &[PathBuf]) -> Result<Self> {
        let text = pattern
            .to_str()
            .ok_or_else(|| BioFormatsError::Format("Invalid filename".into()))?;
        let glob_shape = GlobPatternShape::parse(text);
        if glob_shape.wildcards.is_empty() {
            return Self::from_file_list(files);
        }

        let mut labels: Vec<Vec<String>> = vec![Vec::new(); glob_shape.wildcards.len()];
        for file in files {
            let file_text = file
                .to_str()
                .ok_or_else(|| BioFormatsError::Format("Invalid filename".into()))?;
            let captures = glob_shape.match_text(file_text).ok_or_else(|| {
                BioFormatsError::Format(format!(
                    "FilePattern: expanded glob file does not match pattern: {file_text}"
                ))
            })?;
            for (idx, capture) in captures.into_iter().enumerate() {
                if !labels[idx].contains(&capture) {
                    labels[idx].push(capture);
                }
            }
        }
        for block_labels in &mut labels {
            block_labels.sort();
        }

        let blocks = glob_shape
            .separators
            .iter()
            .zip(glob_shape.wildcards.iter())
            .zip(labels)
            .map(|((separator, wildcard), labels)| {
                let numeric_values: Vec<u64> = labels
                    .iter()
                    .filter_map(|label| label.parse::<u64>().ok())
                    .collect();
                FilePatternBlock {
                    separator: separator.clone(),
                    token: Some(wildcard.clone()),
                    width: labels.iter().map(|label| label.len()).max().unwrap_or(1),
                    min: numeric_values.iter().copied().min().unwrap_or(0),
                    max: numeric_values.iter().copied().max().unwrap_or(0),
                    values: numeric_values,
                    labels: Some(labels),
                }
            })
            .collect();

        Ok(FilePattern {
            dir: PathBuf::new(),
            source_pattern: Some(text.to_string()),
            source_root: source_pattern_root(Path::new(text)),
            full_path_pattern: true,
            prefix: String::new(),
            suffix: glob_shape.suffix,
            blocks,
        })
    }

    fn match_text_for_path(&self, path: &Path) -> Option<String> {
        if self.full_path_pattern {
            path.to_str().map(|text| text.to_string())
        } else {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.to_string())
        }
    }

    /// Try to extract numeric values from a filename that matches this pattern.
    fn match_filename(&self, name: &str) -> Option<Vec<String>> {
        let mut pos = 0;
        let mut values = Vec::new();
        let mut recursive_empty_consumed_separator = false;
        for block in &self.blocks {
            // Match separator
            let mut separator = block.separator.as_str();
            if recursive_empty_consumed_separator && separator.starts_with('/') {
                separator = &separator[1..];
            }
            if !name[pos..].starts_with(separator) {
                return None;
            }
            pos += separator.len();
            if let Some(labels) = &block.labels {
                let label = labels
                    .iter()
                    .filter(|label| name[pos..].starts_with(label.as_str()))
                    .max_by_key(|label| label.len())?;
                pos += label.len();
                recursive_empty_consumed_separator =
                    block.token.as_deref() == Some("**") && label.is_empty();
                values.push(label.clone());
            } else {
                recursive_empty_consumed_separator = false;
                // Extract digits
                let digit_start = pos;
                while pos < name.len() && name.as_bytes()[pos].is_ascii_digit() {
                    pos += 1;
                }
                if pos == digit_start {
                    return None;
                }
                values.push(name[digit_start..pos].to_string());
            }
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
            if self.full_path_pattern {
                return vec![PathBuf::from(name)];
            }
            return vec![self.dir.join(name)];
        }
        let block = &self.blocks[block_idx];
        let mut results = Vec::new();
        for value in block.value_labels() {
            let next = format!("{}{}{}", current, block.separator, value);
            results.extend(self.enumerate_blocks(block_idx + 1, next));
        }
        results
    }

    /// Total number of files in this pattern.
    pub fn file_count(&self) -> usize {
        self.blocks
            .iter()
            .map(|b| b.value_labels().len())
            .product::<usize>()
            .max(1)
    }
}

impl FilePatternBlock {
    fn value_labels(&self) -> Vec<String> {
        self.labels.clone().unwrap_or_else(|| {
            self.values
                .iter()
                .map(|value| format!("{value:0>width$}", width = self.width))
                .collect()
        })
    }

    fn position_of(&self, value: &str) -> Option<usize> {
        self.value_labels()
            .iter()
            .position(|candidate| candidate == value)
    }

    fn has_non_numeric_labels(&self) -> bool {
        self.labels
            .as_ref()
            .is_some_and(|labels| labels.iter().any(|label| label.parse::<u64>().is_err()))
    }
}

struct GlobPatternShape {
    separators: Vec<String>,
    wildcards: Vec<String>,
    suffix: String,
}

impl GlobPatternShape {
    fn parse(pattern: &str) -> Self {
        let mut separators = Vec::new();
        let mut wildcards = Vec::new();
        let mut literal_start = 0usize;
        let mut iter = pattern.char_indices().peekable();
        while let Some((idx, ch)) = iter.next() {
            let Some(mut wildcard_end) = glob_atom_end(pattern, idx, ch) else {
                continue;
            };
            separators.push(pattern[literal_start..idx].to_string());
            while let Some(&(next_idx, next_ch)) = iter.peek() {
                let Some(next_end) = glob_atom_end(pattern, next_idx, next_ch) else {
                    break;
                };
                let _ = iter.next();
                wildcard_end = next_end;
            }
            wildcards.push(pattern[idx..wildcard_end].to_string());
            literal_start = wildcard_end;
        }
        let suffix = pattern[literal_start..].to_string();
        Self {
            separators,
            wildcards,
            suffix,
        }
    }

    fn match_text(&self, text: &str) -> Option<Vec<String>> {
        let mut pos = 0usize;
        let mut captures = Vec::with_capacity(self.wildcards.len());
        let mut recursive_empty_consumed_separator = false;
        for idx in 0..self.wildcards.len() {
            let mut separator = self.separators[idx].as_str();
            if recursive_empty_consumed_separator && separator.starts_with('/') {
                separator = &separator[1..];
            }
            if !text[pos..].starts_with(separator) {
                return None;
            }
            pos += separator.len();

            let next_literal = self
                .separators
                .get(idx + 1)
                .map(String::as_str)
                .unwrap_or(self.suffix.as_str());
            let capture_end = find_capture_end(&text[pos..], &self.wildcards[idx], next_literal)?;
            let capture = &text[pos..pos + capture_end];
            if !simple_glob_matches(&self.wildcards[idx], capture) {
                return None;
            }
            recursive_empty_consumed_separator = self.wildcards[idx] == "**" && capture.is_empty();
            captures.push(capture.to_string());
            pos += capture_end;
        }
        if text[pos..] != self.suffix {
            return None;
        }
        Some(captures)
    }
}

fn glob_atom_end(pattern: &str, idx: usize, ch: char) -> Option<usize> {
    match ch {
        '*' | '?' => Some(idx + ch.len_utf8()),
        '[' => pattern[idx + ch.len_utf8()..]
            .find(']')
            .map(|end_rel| idx + ch.len_utf8() + end_rel + 1),
        _ => None,
    }
}

fn find_capture_end(text: &str, wildcard: &str, next_literal: &str) -> Option<usize> {
    if wildcard == "**" {
        if let Some(trimmed) = next_literal.strip_prefix('/') {
            if text.starts_with(trimmed) {
                return Some(0);
            }
        }
    }
    if !wildcard.contains('*') {
        let count = fixed_width_glob_atoms(wildcard)?;
        let end = text
            .char_indices()
            .nth(count)
            .map_or(text.len(), |(idx, _)| idx);
        return text[end..].starts_with(next_literal).then_some(end);
    }

    if next_literal.is_empty() {
        return Some(text.len());
    }
    text.match_indices(next_literal)
        .map(|(idx, _)| idx)
        .find(|idx| simple_glob_matches(wildcard, &text[..*idx]))
}

fn fixed_width_glob_atoms(wildcard: &str) -> Option<usize> {
    let mut count = 0usize;
    let mut iter = wildcard.char_indices().peekable();
    while let Some((idx, ch)) = iter.next() {
        match ch {
            '?' => count += 1,
            '[' => {
                let end = glob_atom_end(wildcard, idx, ch)?;
                while let Some(&(next_idx, _)) = iter.peek() {
                    if next_idx >= end {
                        break;
                    }
                    let _ = iter.next();
                }
                count += 1;
            }
            _ => return None,
        }
    }
    Some(count)
}

fn simple_glob_matches(pattern: &str, name: &str) -> bool {
    let pattern_bytes = pattern.as_bytes();
    let name = name.as_bytes();
    let mut p = 0usize;
    let mut n = 0usize;
    let mut star = None;
    let mut after_star_name = 0usize;

    while n < name.len() {
        if p < pattern_bytes.len() && (pattern_bytes[p] == b'?' || pattern_bytes[p] == name[n]) {
            p += 1;
            n += 1;
        } else if p < pattern_bytes.len() && pattern_bytes[p] == b'[' {
            let Some((end, matched)) = glob_bracket_class_matches(pattern_bytes, p, name[n]) else {
                return false;
            };
            if matched {
                p = end + 1;
                n += 1;
            } else if let Some(star_pos) = star {
                p = star_pos + 1;
                after_star_name += 1;
                n = after_star_name;
            } else {
                return false;
            }
        } else if p < pattern_bytes.len() && pattern_bytes[p] == b'*' {
            star = Some(p);
            p += 1;
            after_star_name = n;
        } else if let Some(star_pos) = star {
            p = star_pos + 1;
            after_star_name += 1;
            n = after_star_name;
        } else {
            return false;
        }
    }

    while p < pattern_bytes.len() && pattern_bytes[p] == b'*' {
        p += 1;
    }
    p == pattern_bytes.len()
}

fn glob_bracket_class_matches(pattern: &[u8], start: usize, byte: u8) -> Option<(usize, bool)> {
    let mut idx = start + 1;
    if idx >= pattern.len() {
        return None;
    }
    let negated = pattern[idx] == b'!' || pattern[idx] == b'^';
    if negated {
        idx += 1;
    }

    let mut matched = false;
    let mut saw_member = false;
    while idx < pattern.len() {
        if pattern[idx] == b']' && saw_member {
            return Some((idx, if negated { !matched } else { matched }));
        }

        let first = pattern[idx];
        saw_member = true;
        if idx + 2 < pattern.len() && pattern[idx + 1] == b'-' && pattern[idx + 2] != b']' {
            let last = pattern[idx + 2];
            let (lo, hi) = if first <= last {
                (first, last)
            } else {
                (last, first)
            };
            if lo <= byte && byte <= hi {
                matched = true;
            }
            idx += 3;
        } else {
            if first == byte {
                matched = true;
            }
            idx += 1;
        }
    }
    None
}

fn find_next_explicit_pattern_block(pattern: &str) -> Option<(usize, char, char)> {
    pattern.char_indices().find_map(|(idx, ch)| match ch {
        '<' => Some((idx, '<', '>')),
        '[' => Some((idx, '[', ']')),
        '{' => Some((idx, '{', '}')),
        _ => None,
    })
}

fn find_matching_explicit_pattern_block_end(
    pattern: &str,
    start: usize,
    open: char,
    close: char,
) -> Result<usize> {
    let mut stack = vec![close];
    for (rel_idx, ch) in pattern[start + open.len_utf8()..].char_indices() {
        let idx = start + open.len_utf8() + rel_idx;
        match ch {
            '<' => stack.push('>'),
            '[' => stack.push(']'),
            '{' => stack.push('}'),
            '>' | ']' | '}' => {
                if stack.pop() != Some(ch) {
                    return Err(BioFormatsError::Format(format!(
                        "FilePattern: unmatched pattern block delimiter {ch}"
                    )));
                }
                if stack.is_empty() {
                    return Ok(idx);
                }
            }
            _ => {}
        }
    }
    Err(BioFormatsError::Format(format!(
        "FilePattern: unterminated {open}{close} pattern block"
    )))
}

fn expand_explicit_pattern_text(pattern: &str, out: &mut Vec<String>) -> Result<()> {
    let Some((start, open, close)) = find_next_explicit_pattern_block(pattern) else {
        reject_explicit_pattern_value(pattern)?;
        out.push(pattern.to_string());
        return Ok(());
    };
    let end = find_matching_explicit_pattern_block_end(pattern, start, open, close)?;
    let prefix = &pattern[..start];
    let suffix = &pattern[end + close.len_utf8()..];
    for value in parse_explicit_pattern_block(&pattern[start + open.len_utf8()..end])? {
        let candidate = format!("{prefix}{value}{suffix}");
        expand_explicit_pattern_text(&candidate, out)?;
    }
    Ok(())
}

fn parse_explicit_pattern_block(block: &str) -> Result<Vec<String>> {
    if block.trim().is_empty() {
        return Err(BioFormatsError::Format(
            "FilePattern: empty pattern block".to_string(),
        ));
    }
    let mut values = Vec::new();
    for part in split_top_level_explicit_pattern_commas(block)? {
        let part = part.trim();
        if part.is_empty() {
            return Err(BioFormatsError::Format(
                "FilePattern: empty pattern list entry".to_string(),
            ));
        }
        values.extend(parse_explicit_pattern_part(part)?);
    }
    Ok(values)
}

fn parse_explicit_pattern_part(part: &str) -> Result<Vec<String>> {
    if find_next_explicit_pattern_block(part).is_some() {
        let mut nested = Vec::new();
        expand_explicit_pattern_text(part, &mut nested)?;
        for value in &nested {
            reject_explicit_pattern_value(value)?;
        }
        return Ok(nested);
    }

    let (range, step) = match part.split_once(':') {
        Some((range, step)) => (
            range,
            step.parse::<i64>().map_err(|_| {
                BioFormatsError::Format("FilePattern: invalid range step".to_string())
            })?,
        ),
        None => (part, 1),
    };
    if step <= 0 {
        return Err(BioFormatsError::Format(
            "FilePattern: range step must be positive".to_string(),
        ));
    }

    if let Some((first_text, last_text)) = range.split_once('-') {
        if let (Ok(first), Ok(last)) = (first_text.parse::<i64>(), last_text.parse::<i64>()) {
            let width = first_text.len().max(last_text.len());
            let mut out = Vec::new();
            let mut value = first;
            if first <= last {
                while value <= last {
                    out.push(format!("{value:0width$}"));
                    value += step;
                }
            } else {
                while value >= last {
                    out.push(format!("{value:0width$}"));
                    value -= step;
                }
            }
            return Ok(out);
        }

        if first_text.chars().count() == 1 && last_text.chars().count() == 1 {
            let first = first_text.chars().next().unwrap();
            let last = last_text.chars().next().unwrap();
            if first.is_ascii_alphabetic() && last.is_ascii_alphabetic() {
                let mut out = Vec::new();
                let mut value = first as i64;
                let last = last as i64;
                if value <= last {
                    while value <= last {
                        out.push(char::from_u32(value as u32).unwrap().to_string());
                        value += step;
                    }
                } else {
                    while value >= last {
                        out.push(char::from_u32(value as u32).unwrap().to_string());
                        value -= step;
                    }
                }
                return Ok(out);
            }
        }

        return Err(BioFormatsError::Format(
            "FilePattern: invalid range bounds".to_string(),
        ));
    }

    reject_explicit_pattern_value(range)?;
    Ok(vec![range.to_string()])
}

fn split_top_level_explicit_pattern_commas(block: &str) -> Result<Vec<&str>> {
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut stack = Vec::new();
    for (idx, ch) in block.char_indices() {
        match ch {
            '<' => stack.push('>'),
            '[' => stack.push(']'),
            '{' => stack.push('}'),
            '>' | ']' | '}' => {
                if stack.pop() != Some(ch) {
                    return Err(BioFormatsError::Format(format!(
                        "FilePattern: unmatched pattern block delimiter {ch}"
                    )));
                }
            }
            ',' if stack.is_empty() => {
                parts.push(&block[start..idx]);
                start = idx + ch.len_utf8();
            }
            _ => {}
        }
    }
    if let Some(close) = stack.pop() {
        return Err(BioFormatsError::Format(format!(
            "FilePattern: unterminated pattern block delimiter {close}"
        )));
    }
    parts.push(&block[start..]);
    Ok(parts)
}

fn reject_explicit_pattern_value(value: &str) -> Result<()> {
    if value.is_empty()
        || value.contains('<')
        || value.contains('>')
        || value.contains('[')
        || value.contains(']')
        || value.contains('{')
        || value.contains('}')
        || value.contains('/')
        || value.contains('\\')
    {
        return Err(BioFormatsError::Format(
            "FilePattern: invalid value".to_string(),
        ));
    }
    Ok(())
}

fn source_pattern_root(path: &Path) -> Option<PathBuf> {
    let mut root = PathBuf::new();
    for component in path.components() {
        let component_text = component.as_os_str().to_string_lossy();
        if component_text.contains('<')
            || component_text.contains('[')
            || component_text.contains('{')
            || component_text.contains('*')
            || component_text.contains('?')
        {
            break;
        }
        root.push(component.as_os_str());
    }
    (!root.as_os_str().is_empty()).then_some(root)
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

impl AxisType {
    fn as_str(self) -> &'static str {
        match self {
            AxisType::Z => "Z",
            AxisType::Channel => "C",
            AxisType::Time => "T",
            AxisType::Series => "S",
            AxisType::Unknown => "unknown",
        }
    }
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
            .map(|block| {
                let axis = Self::guess_from_separator(&block.separator);
                if axis == AxisType::Unknown && block.has_non_numeric_labels() {
                    AxisType::Channel
                } else {
                    axis
                }
            })
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
            source_pattern: None,
            source_root: None,
            full_path_pattern: false,
            prefix: String::new(),
            suffix: ".tif".into(),
            blocks: blocks
                .iter()
                .map(|(sep, n)| FilePatternBlock {
                    separator: (*sep).into(),
                    token: None,
                    width: 3,
                    min: 0,
                    max: (*n as u64).saturating_sub(1),
                    values: (0..*n as u64).collect(),
                    labels: None,
                })
                .collect(),
        }
    }

    #[test]
    fn glob_bracket_classes_match_shell_style_members_ranges_and_negation() {
        assert!(simple_glob_matches("img_[AB][0-2].fake", "img_A2.fake"));
        assert!(simple_glob_matches("img_[!AB].fake", "img_C.fake"));
        assert!(simple_glob_matches("img_[^0-2].fake", "img_9.fake"));
        assert!(!simple_glob_matches("img_[AB].fake", "img_C.fake"));
        assert!(!simple_glob_matches("img_[!AB].fake", "img_A.fake"));
    }

    #[test]
    fn expanded_glob_uses_bracket_classes_as_captured_blocks() {
        let pattern = PathBuf::from("/tmp/img_c[AB]_t?.fake");
        let files = vec![
            PathBuf::from("/tmp/img_cA_t0.fake"),
            PathBuf::from("/tmp/img_cA_t1.fake"),
            PathBuf::from("/tmp/img_cB_t0.fake"),
            PathBuf::from("/tmp/img_cB_t1.fake"),
        ];

        let fp = FilePattern::from_expanded_glob(&pattern, &files).unwrap();
        assert_eq!(fp.blocks.len(), 2);
        assert_eq!(fp.blocks[0].value_labels(), vec!["A", "B"]);
        assert_eq!(fp.blocks[1].value_labels(), vec!["0", "1"]);

        let guess = AxisGuesser::guess(&fp);
        assert_eq!(guess, vec![AxisType::Channel, AxisType::Time]);
    }

    #[test]
    fn file_stitcher_ome_metadata_uses_filepattern_channel_names() {
        let mut meta = ImageMetadata {
            size_x: 1,
            size_y: 1,
            size_c: 2,
            image_count: 2,
            ..ImageMetadata::default()
        };
        meta.series_metadata.insert(
            "Channel 0 Name".into(),
            MetadataValue::String("DAPI".into()),
        );
        meta.series_metadata.insert(
            "Channel 1 Name".into(),
            MetadataValue::String("FITC".into()),
        );
        let stitcher = FileStitcher {
            files: Vec::new(),
            meta: Some(meta),
            plane_map: Vec::new(),
            current_reader: None,
        };

        let ome = stitcher.ome_metadata().unwrap();
        assert_eq!(ome.images[0].channels[0].name.as_deref(), Some("DAPI"));
        assert_eq!(ome.images[0].channels[1].name.as_deref(), Some("FITC"));
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
