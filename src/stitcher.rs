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
    /// Metadata per exposed series.
    metas: Vec<ImageMetadata>,
    /// Maps stitched plane indices to (file index, local series, local plane index).
    plane_maps: Vec<Vec<(usize, usize, u32)>>,
    /// The currently selected exposed series.
    current_series: usize,
    /// Whether this is Java FileStitcher's noStitch path: a single file should
    /// be exposed exactly like the wrapped reader, including all series.
    no_stitch: bool,
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

        Self::from_discovered_files(files, FilePattern::from_file(path).ok())
    }

    /// Open with explicit file list (no auto-discovery).
    pub fn from_files(files: Vec<PathBuf>) -> Result<Self> {
        if files.is_empty() {
            return Err(BioFormatsError::Format("Empty file list".into()));
        }

        let pattern = FilePattern::from_file_list(&files).ok();
        Self::from_discovered_files(files, pattern)
    }

    /// Open with an explicit file list and the `.pattern` text that produced it.
    pub fn from_files_with_pattern(files: Vec<PathBuf>, pattern_path: &Path) -> Result<Self> {
        if files.is_empty() {
            return Err(BioFormatsError::Format("Empty file list".into()));
        }

        let pattern = FilePattern::from_explicit_pattern(pattern_path)?;
        Self::from_discovered_files(files, Some(pattern))
    }

    /// Open with an explicit file list and an already-parsed file pattern.
    pub(crate) fn from_files_with_file_pattern(
        files: Vec<PathBuf>,
        pattern: FilePattern,
    ) -> Result<Self> {
        if files.is_empty() {
            return Err(BioFormatsError::Format("Empty file list".into()));
        }

        Self::from_discovered_files(files, Some(pattern))
    }

    fn from_discovered_files(files: Vec<PathBuf>, pattern: Option<FilePattern>) -> Result<Self> {
        let mut first = crate::registry::ImageReader::open(&files[0])?;
        if files.len() == 1 {
            let mut metas = Vec::with_capacity(first.series_count());
            let mut plane_maps = Vec::with_capacity(first.series_count());
            for series in 0..first.series_count() {
                first.set_series(series)?;
                let meta = first.metadata().clone();
                plane_maps.push(
                    (0..meta.image_count)
                        .map(|plane| (0, series, plane))
                        .collect(),
                );
                metas.push(meta);
            }
            let _ = first.close();
            return Ok(FileStitcher {
                files,
                metas,
                plane_maps,
                current_series: 0,
                no_stitch: true,
                current_reader: None,
            });
        }

        if first.series_count() > 1 {
            return Err(BioFormatsError::Format(
                "Unsupported grouping: file pattern contains multiple files and each file contains multiple series"
                    .into(),
            ));
        }
        let base_meta = first.metadata().clone();
        let (meta, plane_map) = stitch_layout(&files, &base_meta, pattern.as_ref())?;
        let _ = first.close();
        Ok(FileStitcher {
            files,
            metas: vec![meta],
            plane_maps: vec![plane_map],
            current_series: 0,
            no_stitch: false,
            current_reader: None,
        })
    }

    /// Resolve a stitched plane index to (file_index, local_plane_index).
    fn resolve_plane(&self, plane_index: u32) -> Result<(usize, usize, u32)> {
        let meta = self
            .metas
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        self.plane_maps
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?
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
) -> Result<(ImageMetadata, Vec<(usize, usize, u32)>)> {
    let mut meta = base_meta.clone();
    let file_axes = pattern
        .and_then(|pattern| infer_file_axes(files, pattern, base_meta))
        .unwrap_or_else(|| FileAxisLayout {
            file_coords: (0..files.len()).map(|i| (0, 0, i as u32)).collect(),
            size_z: 1,
            size_c: 1,
            size_t: files.len() as u32,
            axis_types: pattern.map(AxisGuesser::guess).unwrap_or_default(),
            adjusted_order: dimension_order_str(base_meta.dimension_order).to_string(),
        });

    meta.dimension_order =
        dimension_order_from_str(&file_axes.adjusted_order).unwrap_or(base_meta.dimension_order);
    meta.size_z = checked_axis_mul(base_meta.size_z, file_axes.size_z, "Z")?;
    meta.size_c = checked_axis_mul(base_meta.size_c, file_axes.size_c, "C")?;
    meta.size_t = checked_axis_mul(base_meta.size_t, file_axes.size_t, "T")?;
    let stitched_effective_c =
        checked_axis_mul(effective_size_c(base_meta), file_axes.size_c, "C")?;
    meta.image_count = meta
        .size_z
        .checked_mul(stitched_effective_c)
        .and_then(|v| v.checked_mul(meta.size_t))
        .ok_or_else(|| BioFormatsError::Format("Stitched plane count overflow".into()))?;

    let mut plane_map = vec![None; meta.image_count as usize];
    for (file_idx, &(file_z, file_c, file_t)) in file_axes.file_coords.iter().enumerate() {
        for local_plane in 0..base_meta.image_count {
            let (local_z, local_c, local_t) = plane_to_zct(local_plane, base_meta)
                .ok_or_else(|| BioFormatsError::Format("Invalid base plane index".into()))?;
            let z = file_z * base_meta.size_z + local_z;
            let c = file_c * effective_size_c(base_meta) + local_c;
            let t = file_t * base_meta.size_t + local_t;
            let stitched = zct_to_plane(z, c, t, &meta)
                .ok_or_else(|| BioFormatsError::Format("Invalid stitched plane index".into()))?;
            plane_map[stitched as usize] = Some((file_idx, 0, local_plane));
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
    adjusted_order: String,
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
        adjusted_order: guess.adjusted_order,
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

fn dimension_order_from_str(order: &str) -> Option<crate::common::metadata::DimensionOrder> {
    use crate::common::metadata::DimensionOrder::*;
    match order {
        "XYCTZ" => Some(XYCTZ),
        "XYCZT" => Some(XYCZT),
        "XYTCZ" => Some(XYTCZ),
        "XYTZC" => Some(XYTZC),
        "XYZCT" => Some(XYZCT),
        "XYZTC" => Some(XYZTC),
        _ => None,
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

fn rgb_channel_count(meta: &ImageMetadata) -> u32 {
    if !meta.is_rgb {
        return 1;
    }
    let zt = meta.size_z.max(1).saturating_mul(meta.size_t.max(1));
    if zt > 0 && meta.image_count >= zt {
        let effective_c = (meta.image_count / zt).max(1);
        if effective_c > 0 && meta.size_c >= effective_c && meta.size_c % effective_c == 0 {
            return (meta.size_c / effective_c).max(1);
        }
    }
    meta.size_c.max(1)
}

fn effective_size_c(meta: &ImageMetadata) -> u32 {
    if meta.is_rgb {
        (meta.size_c / rgb_channel_count(meta)).max(1)
    } else {
        meta.size_c.max(1)
    }
}

fn plane_to_zct(plane_index: u32, meta: &ImageMetadata) -> Option<(u32, u32, u32)> {
    for t in 0..meta.size_t {
        for z in 0..meta.size_z {
            for c in 0..effective_size_c(meta) {
                if zct_to_plane(z, c, t, meta)? == plane_index {
                    return Some((z, c, t));
                }
            }
        }
    }
    None
}

fn zct_to_plane(z: u32, c: u32, t: u32, meta: &ImageMetadata) -> Option<u32> {
    let effective_c = effective_size_c(meta);
    if z >= meta.size_z || c >= effective_c || t >= meta.size_t {
        return None;
    }
    Some(match meta.dimension_order {
        crate::common::metadata::DimensionOrder::XYZCT => {
            t * meta.size_z * effective_c + c * meta.size_z + z
        }
        crate::common::metadata::DimensionOrder::XYZTC => {
            c * meta.size_z * meta.size_t + t * meta.size_z + z
        }
        crate::common::metadata::DimensionOrder::XYCZT => {
            t * effective_c * meta.size_z + z * effective_c + c
        }
        crate::common::metadata::DimensionOrder::XYCTZ => {
            z * effective_c * meta.size_t + t * effective_c + c
        }
        crate::common::metadata::DimensionOrder::XYTCZ => {
            z * meta.size_t * effective_c + c * meta.size_t + t
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
        self.metas.clear();
        self.files.clear();
        self.plane_maps.clear();
        self.current_series = 0;
        self.no_stitch = false;
        Ok(())
    }

    fn series_count(&self) -> usize {
        self.metas.len()
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if s >= self.metas.len() {
            return Err(BioFormatsError::SeriesOutOfRange(s));
        }
        self.current_series = s;
        if self.no_stitch {
            if let Some((_, reader)) = &mut self.current_reader {
                reader.set_series(s)?;
            }
        }
        Ok(())
    }

    fn series(&self) -> usize {
        self.current_series
    }

    fn metadata(&self) -> &ImageMetadata {
        self.metas
            .get(self.current_series)
            .expect("FileStitcher not initialized")
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let (file_idx, local_series, local_plane) = self.resolve_plane(plane_index)?;
        let reader = self.ensure_reader(file_idx)?;
        reader.set_series(local_series)?;
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
        let (file_idx, local_series, local_plane) = self.resolve_plane(plane_index)?;
        let reader = self.ensure_reader(file_idx)?;
        reader.set_series(local_series)?;
        reader.open_bytes_region(local_plane, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let (file_idx, local_series, local_plane) = self.resolve_plane(plane_index)?;
        let reader = self.ensure_reader(file_idx)?;
        reader.set_series(local_series)?;
        reader.open_thumb_bytes(local_plane)
    }

    fn ome_metadata(&self) -> Option<OmeMetadata> {
        let meta = self.metas.get(self.current_series)?;
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
    /// Explicit file listing computed by the regex branch of Java
    /// `buildFiles` (the `blocks.length == 0` case). When `Some`, this is the
    /// authoritative file list (Java's `files` field) and is returned directly
    /// by [`FilePattern::filenames`] instead of enumerating blocks.
    regex_files: Option<Vec<PathBuf>>,
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
    ///
    /// Faithful to Java `FilePattern(Location file)` (FilePattern.java:129-131):
    /// it runs `findPattern(file)` → `findPattern(name, dir)` →
    /// `findPattern(name, dir, nameList)` to derive a pattern string, then feeds
    /// that string into the `FilePattern(String pattern)` constructor
    /// ([`FilePattern::from_explicit_pattern`]). If auto-discovery yields no
    /// pattern (e.g. directory unreadable or only one candidate), it falls back
    /// to the local numeric-run scan below.
    pub fn from_file(path: &Path) -> Result<Self> {
        if let Some(pattern) = Self::find_pattern_for_file(path) {
            return Self::from_explicit_pattern(Path::new(&pattern));
        }
        Self::from_file_scan(path)
    }

    /// Port of Java `FilePattern.findPattern(Location file)` /
    /// `findPattern(name, dir)` (FilePattern.java:381-416): list the file's
    /// directory and call [`find_pattern_in_dir`] with that candidate listing.
    /// Returns `None` when the directory cannot be listed (Java returns `null`).
    fn find_pattern_for_file(path: &Path) -> Option<String> {
        let name = path.file_name().and_then(|n| n.to_str())?;
        let dir = path.parent().unwrap_or(Path::new("."));
        let dir_for_listing = if dir.as_os_str().is_empty() {
            Path::new(".")
        } else {
            dir
        };
        let entries = std::fs::read_dir(dir_for_listing).ok()?;
        let name_list: Vec<String> = entries
            .flatten()
            .filter_map(|e| e.file_name().to_str().map(|s| s.to_string()))
            .collect();
        let dir_str = dir.to_str().unwrap_or("");
        find_pattern_in_dir(name, dir_str, &name_list)
    }

    /// Discover one [`FilePattern`] per series for the given exemplar file.
    ///
    /// Faithful to Java's use of `FilePattern.findSeriesPatterns` (the
    /// series-aware sibling of `findPattern`): the file's directory is listed
    /// and [`find_series_patterns`] returns a separate `<…>` pattern string for
    /// each series index (the S axis is held literal). Each returned string is
    /// then parsed via the `FilePattern(String)` constructor
    /// ([`FilePattern::from_explicit_pattern`]). When no series patterns are
    /// found, falls back to a single [`FilePattern::from_file`].
    pub fn from_file_series(path: &Path) -> Result<Vec<Self>> {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| BioFormatsError::Format("Invalid filename".into()))?;
        let dir = path.parent().unwrap_or(Path::new("."));
        let dir_for_listing = if dir.as_os_str().is_empty() {
            Path::new(".")
        } else {
            dir
        };

        let name_list: Vec<String> = match std::fs::read_dir(dir_for_listing) {
            Ok(entries) => entries
                .flatten()
                .filter_map(|e| e.file_name().to_str().map(|s| s.to_string()))
                .collect(),
            Err(_) => Vec::new(),
        };
        let dir_str = dir.to_str().unwrap_or("");
        let base = path.to_str().unwrap_or(name);

        let patterns = find_series_patterns(base, dir_str, &name_list);
        if patterns.is_empty() {
            return Ok(vec![Self::from_file(path)?]);
        }
        patterns
            .iter()
            .map(|p| Self::from_explicit_pattern(Path::new(p)))
            .collect()
    }

    /// Numeric-run scanning fallback for [`FilePattern::from_file`]. This is the
    /// original directory-scan implementation, used when `findPattern` cannot
    /// derive a pattern string.
    fn from_file_scan(path: &Path) -> Result<Self> {
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
                regex_files: None,
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
            regex_files: None,
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
                regex_files: None,
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
            regex_files: None,
        })
    }

    /// Build a block-less, regex-mode [`FilePattern`]. Faithful port of the
    /// `blocks.length == 0` branch of Java `FilePattern(String pattern)` /
    /// `buildFiles` (FilePattern.java:149-214, 761-818): the pattern string is
    /// compiled as a regular expression and matched against existing files via
    /// [`build_files_regex`]. When the regex matches nothing, Java falls back to
    /// `files = {pattern}` (FilePattern.java:210-212).
    fn from_regex_pattern(path: &Path) -> Result<Self> {
        let text = path
            .to_str()
            .ok_or_else(|| BioFormatsError::Format("Invalid filename".into()))?;

        let mut files: Vec<PathBuf> = build_files_regex(text)
            .into_iter()
            .map(PathBuf::from)
            .collect();
        if files.is_empty() {
            files.push(PathBuf::from(text));
        }

        Ok(FilePattern {
            dir: PathBuf::new(),
            source_pattern: Some(text.to_string()),
            source_root: source_pattern_root(Path::new(text)),
            full_path_pattern: true,
            prefix: String::new(),
            suffix: String::new(),
            blocks: Vec::new(),
            regex_files: Some(files),
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
            // Faithful port of Java `FilePattern(String pattern)` →
            // `buildFiles("", 0, fileList)` with `blocks.length == 0`: the
            // pattern is treated as a regular expression and matched against
            // the existing files (FilePattern.java:762-818). Java then falls
            // back to `files = {pattern}` when nothing matches
            // (FilePattern.java:210-212).
            return Self::from_regex_pattern(path);
        }

        Ok(FilePattern {
            dir: PathBuf::new(),
            source_pattern: Some(text.to_string()),
            source_root: source_pattern_root(Path::new(text)),
            full_path_pattern: true,
            prefix: String::new(),
            suffix: text[suffix_start..].to_string(),
            blocks,
            regex_files: None,
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
            regex_files: None,
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
    ///
    /// When the pattern was built in regex mode (Java's `blocks.length == 0`
    /// branch), the explicit file listing computed by [`build_files_regex`] is
    /// returned directly, mirroring Java `getFiles()` returning the cached
    /// `files` array.
    pub fn filenames(&self) -> Vec<PathBuf> {
        if let Some(files) = &self.regex_files {
            return files.clone();
        }
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

    // Faithful port of the range-block branch of Java
    // FilePatternBlock.explode() (FilePatternBlock.java:203-252). A single
    // comma-less token is either a constant (no '-') or a START-STOP:STEP
    // range. Java splits on '-' (no limit) taking elements[0] as the begin and
    // elements[1] as the rest, then splits the rest on ':' (-1 limit) for the
    // optional step.
    let dash_parts: Vec<&str> = part.split('-').collect();
    if dash_parts.len() < 2 {
        // No range: a single constant element (FilePatternBlock.java:204-208).
        reject_explicit_pattern_value(part)?;
        return Ok(vec![part.to_string()]);
    }
    let b = dash_parts[0];
    // elements[1].split(":", -1): Java only looks at the second '-' split
    // element for the STOP[:STEP] portion.
    let colon_parts: Vec<&str> = dash_parts[1].split(':').collect();
    let e = colon_parts[0];
    let s = if colon_parts.len() < 2 {
        "1"
    } else {
        colon_parts[1]
    };

    expand_range_block(b, e, s)
}

/// Faithful port of the numeric/alphabetic range expansion in Java
/// FilePatternBlock.explode() (FilePatternBlock.java:214-252).
///
/// Tries base-10 first; on failure falls back to base-36
/// (`Character.MAX_RADIX`) alphabetic ranges, matching Java's two-stage
/// `BigInteger` parse. Reproduces Java's quirks exactly: `fixed` is
/// `begin.len() == end.len()` (NOT max width), zero-padding pads to
/// `end.len()`, alphabetic values are upper/lower-cased per the case of the
/// first character of `begin`, and the element count is
/// `(end - begin) / step + 1`. Java yields no elements for a descending range
/// (`begin > end`) with a positive step; as a deliberate non-upstream extension
/// this counts DOWN instead (e.g. `<2-0>` → 2,1,0).
fn expand_range_block(b: &str, e: &str, s: &str) -> Result<Vec<String>> {
    // Stage 1: numeric (base 10).
    let parsed = match (b.parse::<i128>(), e.parse::<i128>(), s.parse::<i128>()) {
        (Ok(begin), Ok(end), Ok(step)) => Some((begin, end, step, true)),
        _ => {
            // Stage 2: alphabetic (base 36).
            match (from_radix36(b), from_radix36(e), from_radix36(s)) {
                (Some(begin), Some(end), Some(step)) => Some((begin, end, step, false)),
                _ => None,
            }
        }
    };
    let Some((begin, end, step, numeric)) = parsed else {
        return Err(BioFormatsError::Format(
            "FilePattern: invalid range delimiter(s)".to_string(),
        ));
    };
    if step == 0 {
        return Err(BioFormatsError::Format(
            "FilePattern: range step must be non-zero".to_string(),
        ));
    }

    let fixed = b.len() == e.len();

    // count = end.subtract(begin).divide(step).intValue() + 1
    let count = (end - begin) / step + 1;
    // Java's formula yields a non-positive count for a descending range
    // (begin > end) with a positive step — i.e. an empty expansion. As a
    // deliberate non-upstream extension we instead count DOWN in that case so
    // `<2-0>` expands to 2,1,0 (covered by the descending-range reader test).
    let (count, effective_step) = if count > 0 {
        (count, step)
    } else if step > 0 && begin > end {
        ((begin - end) / step + 1, -step)
    } else {
        return Ok(Vec::new());
    };
    let mut out = Vec::with_capacity(count as usize);
    let lower_first = b.chars().next().is_some_and(|c| c.is_lowercase());
    for i in 0..count {
        let v = begin + effective_step * i;
        let mut value = if numeric {
            v.to_string()
        } else {
            let raw = to_radix36(v);
            if lower_first {
                raw.to_lowercase()
            } else {
                raw.to_uppercase()
            }
        };
        let pad_chars = if fixed {
            (e.len() as isize - value.len() as isize).max(0) as usize
        } else {
            0
        };
        for _ in 0..pad_chars {
            value.insert(0, '0');
        }
        out.push(value);
    }
    Ok(out)
}

/// Parse a base-36 (`Character.MAX_RADIX`) unsigned magnitude, mirroring
/// Java `new BigInteger(s, Character.MAX_RADIX)`. Returns `None` if any
/// character is not a valid base-36 digit (Java would throw
/// `NumberFormatException`). An optional leading `+`/`-` sign is honored as in
/// Java's BigInteger string constructor.
fn from_radix36(s: &str) -> Option<i128> {
    if s.is_empty() {
        return None;
    }
    let (neg, digits) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    if digits.is_empty() {
        return None;
    }
    let mut value: i128 = 0;
    for c in digits.chars() {
        let d = c.to_digit(36)? as i128;
        value = value.checked_mul(36)?.checked_add(d)?;
    }
    Some(if neg { -value } else { value })
}

/// Render a non-negative magnitude in base 36 using lowercase digits, matching
/// Java `BigInteger.toString(Character.MAX_RADIX)` (which the caller then
/// upper/lower-cases). Negative values get a leading '-' as Java does.
fn to_radix36(mut v: i128) -> String {
    if v == 0 {
        return "0".to_string();
    }
    let neg = v < 0;
    if neg {
        v = -v;
    }
    let digits = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut buf = Vec::new();
    while v > 0 {
        buf.push(digits[(v % 36) as usize]);
        v /= 36;
    }
    if neg {
        buf.push(b'-');
    }
    buf.reverse();
    String::from_utf8(buf).unwrap()
}

/// Faithful port of Java `FilePattern.getBounds` (FilePattern.java:716-747).
///
/// Given a sorted list of numbers, produces a `<START-STOP[:STEP]>` block
/// expression if the numbers form an arithmetic progression with a constant,
/// positive step, otherwise returns `None`. When `fixed` is true the START
/// value is zero-padded so that START and STOP have equal width (the
/// fixed-width-block case used by the recursive `findPattern`).
///
/// `numbers` is assumed already sorted ascending, as in Java where the caller
/// runs `Arrays.sort` first.
pub fn get_bounds(numbers: &[i128], fixed: bool) -> Option<String> {
    if numbers.len() < 2 {
        return None;
    }
    let b = numbers[0];
    let e = numbers[numbers.len() - 1];
    let s = numbers[1] - b;
    if s == 0 {
        // step size must be positive
        return None;
    }
    for i in 2..numbers.len() {
        if numbers[i] - numbers[i - 1] != s {
            // step size is not constant
            return None;
        }
    }
    let sb = b.to_string();
    let se = e.to_string();
    let mut bounds = String::from("<");
    if fixed {
        let zeroes = se.len() as isize - sb.len() as isize;
        for _ in 0..zeroes {
            bounds.push('0');
        }
    }
    bounds.push_str(&sb);
    bounds.push('-');
    bounds.push_str(&se);
    if s != 1 {
        bounds.push(':');
        bounds.push_str(&s.to_string());
    }
    bounds.push('>');
    Some(bounds)
}

// ─────────────────────────────────────────────────────────────────────────────
// NumberFilter — helper filter for FilePattern.findPattern (NumberFilter.java)
// ─────────────────────────────────────────────────────────────────────────────

/// Faithful port of Java `NumberFilter` (NumberFilter.java): a filter for files
/// containing a numerical block sandwiched between fixed `pre`/`post` strings.
struct NumberFilter {
    pre: String,
    post: String,
}

impl NumberFilter {
    fn new(pre: &str, post: &str) -> Self {
        NumberFilter {
            pre: pre.to_string(),
            post: post.to_string(),
        }
    }

    /// Gets the number filling the asterisk position. Mirrors
    /// `NumberFilter.getNumber`: requires `name` to start with `pre` and end
    /// with `post`, then parses the middle as a `BigInteger` (base 10). Returns
    /// `None` if the bounds overlap or the middle is not a valid integer.
    fn get_number(&self, name: &str) -> Option<i128> {
        if !name.starts_with(&self.pre) || !name.ends_with(&self.post) {
            return None;
        }
        let ndx = self.pre.len();
        let end = name.len().checked_sub(self.post.len())?;
        if end < ndx {
            return None;
        }
        name[ndx..end].parse::<i128>().ok()
    }

    /// Tests if a name should be accepted (`NumberFilter.accept`).
    fn accept(&self, name: &str) -> bool {
        self.get_number(name).is_some()
    }
}

/// Faithful port of Java `AxisGuesser.getAxisType(String label)`
/// (AxisGuesser.java:372-387). Returns the axis-type ordinal (Z=1, T=2, C=3,
/// S=4, UNKNOWN=0) for a label, matching case-insensitively when the lowercased
/// label equals or *ends with* a known prefix. Note this differs from
/// [`AxisGuesser::guess_from_separator`], which matches the trailing alphabetic
/// segment exactly; this is the variant used by `findPattern`.
fn get_axis_type(label: &str) -> i32 {
    let lower = label.to_lowercase();
    for p in Z_PREFIXES {
        if *p == lower || lower.ends_with(p) {
            return 1;
        }
    }
    for p in C_PREFIXES {
        if *p == lower || lower.ends_with(p) {
            return 3;
        }
    }
    for p in T_PREFIXES {
        if *p == lower || lower.ends_with(p) {
            return 2;
        }
    }
    for p in S_PREFIXES {
        if *p == lower || lower.ends_with(p) {
            return 4;
        }
    }
    0
}

/// Filters the given list of filenames according to the specified filter.
/// Mirrors Java `FilePattern.matchFiles`.
fn match_files<'a>(in_files: &'a [String], filter: &NumberFilter) -> Vec<&'a str> {
    in_files
        .iter()
        .filter(|name| filter.accept(name))
        .map(|name| name.as_str())
        .collect()
}

/// Recursive helper for fixed-width numerical blocks. Faithful port of Java
/// `FilePattern.findPattern(name, nameList, ndx, end, p)`
/// (FilePattern.java:687-706).
///
/// Tries to split the fixed-width region `[ndx, end)` of `name` into one or more
/// numbered sub-blocks. For each candidate sub-block width `i` (largest first),
/// builds a `NumberFilter` for the surrounding text, collects the matching
/// numbers, and asks [`get_bounds`] for a fixed-width `<START-STOP:STEP>`
/// expression; if that succeeds it recurses on the remainder. Returns the
/// assembled pattern string, or `None` if no breakdown works.
fn find_pattern_fixed(
    name: &str,
    name_list: &[String],
    ndx: usize,
    end: usize,
    p: &str,
) -> Option<String> {
    if ndx == end {
        return Some(p.to_string());
    }
    let mut i = end - ndx;
    while i >= 1 {
        let filter = NumberFilter::new(&name[0..ndx], &name[ndx + i..]);
        let list = match_files(name_list, &filter);
        let mut numbers: Vec<i128> = Vec::with_capacity(list.len());
        let mut parse_ok = true;
        for s in &list {
            // new BigInteger(list[j].substring(ndx, ndx + i))
            match s.get(ndx..ndx + i).and_then(|sub| sub.parse::<i128>().ok()) {
                Some(n) => numbers.push(n),
                None => {
                    parse_ok = false;
                    break;
                }
            }
        }
        if parse_ok {
            numbers.sort();
            if let Some(bounds) = get_bounds(&numbers, true) {
                let next_p = format!("{p}{bounds}");
                if let Some(pat) = find_pattern_fixed(name, name_list, ndx + i, end, &next_p) {
                    return Some(pat);
                }
            }
        }
        i -= 1;
    }
    // no combination worked; this parse path is infeasible
    None
}

/// Faithful port of Java
/// `FilePattern.findPattern(String name, String dir, String[] nameList, int[] excludeAxes)`
/// (FilePattern.java:442-574).
///
/// Identifies the group pattern from a template filename `name`, the directory
/// prefix `dir`, and a list of candidate filenames `name_list`. Numerical blocks
/// are detected, and for each varying block a `<START-STOP:STEP>` expression is
/// substituted (fixed-width blocks are recursively broken down via
/// [`find_pattern_fixed`]; the series-axis 'S'/'E' special case is handled).
/// Returns the identified pattern string, or `None` if no consistent pattern can
/// be found.
///
/// `exclude_axes` holds AxisGuesser axis-type ordinals (e.g. `S_AXIS = 4`) whose
/// blocks should be left as literal text rather than turned into pattern blocks.
pub fn find_pattern_with_excludes(
    name: &str,
    dir: &str,
    name_list: &[String],
    exclude_axes: &[i32],
) -> Option<String> {
    // normalize dir: append the separator if non-empty and not already ending
    // with it (matches Java's `dir += File.separator`)
    let sep = std::path::MAIN_SEPARATOR.to_string();
    let dir = if dir.is_empty() {
        String::new()
    } else if !dir.ends_with(&sep) {
        format!("{dir}{sep}")
    } else {
        dir.to_string()
    };

    // locate numerical blocks
    let bytes = name.as_bytes();
    let len = name.len();
    let mut index_list: Vec<usize> = Vec::new();
    let mut end_list: Vec<usize> = Vec::new();
    let mut num = false;
    let mut ndx = 0usize;
    let mut e = 0usize;
    for i in 0..len {
        let c = bytes[i];
        if c.is_ascii_digit() {
            if num {
                e += 1;
            } else {
                num = true;
                ndx = i;
                e = ndx + 1;
            }
        } else if num {
            num = false;
            index_list.push(ndx);
            end_list.push(e);
        }
    }
    if num {
        index_list.push(ndx);
        end_list.push(e);
    }
    let q = index_list.len();

    let mut sb = String::from(&dir);

    for i in 0..q {
        let last = if i > 0 { end_list[i - 1] } else { 0 };
        let prefix = &name[last..index_list[i]];
        let axis_type = get_axis_type(prefix);
        if exclude_axes.contains(&axis_type) {
            sb.push_str(&name[last..end_list[i]]);
            continue;
        }
        sb.push_str(prefix);
        let pre = &name[0..index_list[i]];
        let post = &name[end_list[i]..];
        let filter = NumberFilter::new(pre, post);
        let list = match_files(name_list, &filter);
        if list.is_empty() {
            return None;
        }
        if list.len() == 1 {
            // false alarm; this number block is constant
            sb.push_str(&name[index_list[i]..end_list[i]]);
            continue;
        }

        // fixed width block iff all matching filenames are the same length
        let mut fix = true;
        for s in &list {
            if s.len() != len {
                fix = false;
                break;
            }
        }
        if fix {
            // tricky; this fixed-width block could represent multiple numberings
            let width = end_list[i] - index_list[i];

            // for each character, determine if it varies between filenames
            let mut same = vec![true; width];
            for j in 0..width {
                same[j] = true;
                let jx = index_list[i] + j;
                let c = bytes[jx];
                for s in &list {
                    if s.as_bytes()[jx] != c {
                        same[j] = false;
                        break;
                    }
                }
            }
            // break down each sub-block
            let mut j = 0usize;
            while j < width {
                let jx = index_list[i] + j;
                if same[j] {
                    // this character is the same in all filenames; lock it down
                    sb.push(bytes[jx] as char);
                    j += 1;
                } else {
                    // recursively split the block into variable prefix + const suffix
                    while j < width && !same[j] {
                        j += 1;
                    }
                    let p = find_pattern_fixed(name, name_list, jx, index_list[i] + j, "");
                    let c = if index_list[i] > 0 {
                        bytes[index_list[i] - 1] as char
                    } else {
                        '.'
                    };
                    // check if this block represents the series axis
                    match p {
                        None if c != 'S' && c != 's' && c != 'E' && c != 'e' => {
                            // unable to find an appropriate breakdown of numerical blocks
                            return None;
                        }
                        None => {
                            sb.push(bytes[end_list[i] - 1] as char);
                        }
                        Some(p) => {
                            sb.push_str(&p);
                        }
                    }
                }
            }
        } else {
            // assume variable-width block represents only one numbering
            let mut numbers: Vec<i128> = Vec::with_capacity(list.len());
            for s in &list {
                // filter.getNumber(list[j]) — guaranteed Some since accept passed
                numbers.push(filter.get_number(s)?);
            }
            numbers.sort();
            let bounds = get_bounds(&numbers, false)?;
            sb.push_str(&bounds);
        }
    }
    if q > 0 {
        sb.push_str(&name[end_list[q - 1]..]);
    } else {
        sb.push_str(name);
    }
    Some(sb)
}

/// Convenience port of Java `findPattern(name, dir, nameList)`
/// (FilePattern.java:427-429): no excluded axes.
pub fn find_pattern_in_dir(name: &str, dir: &str, name_list: &[String]) -> Option<String> {
    find_pattern_with_excludes(name, dir, name_list, &[])
}

/// Faithful port of Java
/// `FilePattern.findSeriesPatterns(String base, String dir, String[] nameList)`
/// (FilePattern.java:640-682).
///
/// Like [`find_pattern_in_dir`] but does not merge series indices into one
/// pattern block; instead it returns a separate pattern per series index. For
/// names `foo_s1_z1.ext, foo_s1_z2.ext, foo_s2_z1.ext, foo_s2_z2.ext` this
/// yields `foo_s1_z<1-2>.ext` and `foo_s2_z<1-2>.ext`.
///
/// `base` is the template path; `dir` and `name_list` are the candidate
/// directory/filenames. The returned patterns are de-duplicated and sorted.
pub fn find_series_patterns(base: &str, dir: &str, name_list: &[String]) -> Vec<String> {
    let sep = std::path::MAIN_SEPARATOR;

    // baseSuffix = everything after the first '.' of the base filename
    let base_name = match base.rfind(sep) {
        Some(idx) => &base[idx + 1..],
        None => base,
    };
    let base_suffix = match base_name.find('.') {
        Some(dot) => base_name[dot + 1..].to_string(),
        None => String::new(),
    };

    let absolute_base = absolute_path_string(base);

    let mut patterns: Vec<String> = Vec::new();
    let exclude = [4i32]; // AxisGuesser.S_AXIS
    for name in name_list {
        let Some(pattern) = find_pattern_with_excludes(name, dir, name_list, &exclude) else {
            continue;
        };
        // start = pattern.lastIndexOf(File.separator) + 1; (Java's "if < 0" can
        // never fire since lastIndexOf returns -1 → +1 = 0)
        let start = match pattern.rfind(sep) {
            Some(idx) => idx + 1,
            None => 0,
        };
        let pattern_suffix_src = &pattern[start..];
        let pattern_suffix = match pattern_suffix_src.find('.') {
            Some(dot) => pattern_suffix_src[dot + 1..].to_string(),
            None => String::new(),
        };

        let Some(check_pattern) = find_pattern_in_dir(name, dir, name_list) else {
            continue;
        };
        let check_files: Vec<String> =
            match FilePattern::from_explicit_pattern(Path::new(&check_pattern)) {
                Ok(fp) => fp
                    .filenames()
                    .iter()
                    .map(|p| absolute_path_string(&p.to_string_lossy()))
                    .collect(),
                Err(_) => Vec::new(),
            };

        let pattern_exists = Path::new(&pattern).exists();
        if !patterns.contains(&pattern)
            && (!pattern_exists || absolute_base == pattern)
            && pattern_suffix == base_suffix
            && check_files.iter().any(|f| f == &absolute_base)
        {
            patterns.push(pattern);
        }
    }
    patterns.sort();
    patterns
}

/// Resolve a path to an absolute, lexical form for the equality checks in
/// [`find_series_patterns`], mirroring `new Location(x).getAbsolutePath()`. We
/// avoid `canonicalize` (which requires the file to exist and resolves
/// symlinks); Java's `getAbsolutePath` is purely lexical.
fn absolute_path_string(path: &str) -> String {
    let p = Path::new(path);
    if p.is_absolute() {
        return p.to_string_lossy().to_string();
    }
    match std::env::current_dir() {
        Ok(cwd) => cwd.join(p).to_string_lossy().to_string(),
        Err(_) => p.to_string_lossy().to_string(),
    }
}

/// Faithful port of Java `FilePattern.findPattern(String[] names)`
/// (FilePattern.java:585-603): generates a regular-expression pattern that
/// matches exactly the given set of file names, as a `(?:A)|(?:B)|...`
/// alternation with the common directory prefix quoted. Currently assumes all
/// names share the same directory (matching the Java contract).
pub fn find_pattern_from_names(names: &[String]) -> String {
    if names.is_empty() {
        return String::new();
    }
    let sep = std::path::MAIN_SEPARATOR;
    let dir = match names[0].rfind(sep) {
        Some(idx) => &names[0][..idx + 1],
        None => "",
    };

    let mut pattern = String::new();
    pattern.push_str(&regex_quote(dir));
    for (i, full) in names.iter().enumerate() {
        pattern.push_str("(?:");
        let name = match full.rfind(sep) {
            Some(idx) => &full[idx + 1..],
            None => full.as_str(),
        };
        pattern.push_str(&regex_quote(name));
        pattern.push(')');
        if i < names.len() - 1 {
            pattern.push('|');
        }
    }
    pattern
}

/// Faithful port of Java `FilePattern.buildFiles` (FilePattern.java:761-835),
/// regex-mode branch (the `blocks.length == 0` case). When a pattern contains no
/// numerical blocks it is treated as a regular expression matched against the
/// names of existing files.
///
/// Behavior, mirroring Java:
/// 1. If the pattern names an existing file, return just that file.
/// 2. Otherwise extract the directory: if the pattern starts with `\Q` and a
///    `<separator>\E` occurs before the last path separator, the directory is
///    the quoted literal between `\Q` and that point; else the directory is the
///    substring up to the last path separator.
/// 3. Recursively list every file under that directory ([`get_all_files`]),
///    sort, then keep those whose *file name* matches the (post-directory)
///    regex. Existing matches are returned as absolute paths, non-existing as
///    the raw listing entry.
///
/// Returns the assembled file list (the Java `fileList`).
pub fn build_files_regex(pattern: &str) -> Vec<String> {
    let mut file_list: Vec<String> = Vec::new();

    if Path::new(pattern).exists() {
        file_list.push(pattern.to_string());
        return file_list;
    }

    let sep = std::path::MAIN_SEPARATOR.to_string();

    // int endRegex = pattern.indexOf(File.separator + "\\E") + 1;
    let sep_e = format!("{sep}\\E");
    let end_regex: isize = match pattern.find(&sep_e) {
        Some(idx) => idx as isize + 1,
        None => 0, // indexOf returns -1, +1 = 0
    };
    // int endNotRegex = pattern.lastIndexOf(File.separator) + 1;
    let end_not_regex: isize = match pattern.rfind(&sep) {
        Some(idx) => idx as isize + 1,
        None => 0,
    };

    let dir: String;
    let end: usize;
    if pattern.starts_with("\\Q") && end_regex > 0 && end_regex <= end_not_regex {
        // dir = pattern.substring(2, endRegex); end = endRegex + 2;
        dir = pattern[2..end_regex as usize].to_string();
        end = (end_regex + 2) as usize;
    } else {
        // dir = pattern.substring(0, endNotRegex); end = endNotRegex;
        dir = pattern[0..end_not_regex as usize].to_string();
        end = end_not_regex as usize;
    }

    // We have no Location.getIdMap() equivalent; fall back directly to the
    // filesystem-traversal branch (Java does this when the id map is empty).
    let (dir, files) = if dir.is_empty() || !Path::new(&dir).exists() {
        (".".to_string(), get_all_files("."))
    } else {
        (dir.clone(), get_all_files(&dir))
    };

    let mut files = files;
    files.sort();

    let base_pattern = &pattern[end.min(pattern.len())..];
    // try base pattern, fall back to whole pattern on syntax error
    let regex = match SimpleRegex::compile(base_pattern) {
        Some(r) => r,
        None => match SimpleRegex::compile(pattern) {
            Some(r) => r,
            None => return file_list,
        },
    };

    for f in &files {
        let path = Path::new(&dir).join(f);
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if regex.matches(&name) {
            if path.exists() {
                file_list.push(absolute_path_string(&path.to_string_lossy()));
            } else {
                file_list.push(f.clone());
            }
        }
    }

    file_list
}

/// Faithful port of Java `FilePattern.getAllFiles` (FilePattern.java:837-857):
/// recursively lists every regular file under `dir`, returning absolute paths.
fn get_all_files(dir: &str) -> Vec<String> {
    let mut files = Vec::new();
    let root = Path::new(dir);
    let Ok(entries) = std::fs::read_dir(root) else {
        return files;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let grandchildren = get_all_files(&path.to_string_lossy());
            files.extend(grandchildren);
        } else {
            files.push(absolute_path_string(&path.to_string_lossy()));
        }
    }
    files
}

/// Minimal regular-expression matcher covering the constructs Bio-Formats emits
/// in the [`build_files_regex`] path: literal `\Q...\E` quoted runs, `(?:...)`
/// non-capturing groups, `|` alternation, `.`, `*`, `+`, `?`, and `[...]`
/// character classes. This is *not* a general regex engine — it is a faithful
/// substitute for `java.util.regex.Pattern` over the patterns produced by
/// [`find_pattern_from_names`] and simple user patterns like `z.*\.tif`.
///
/// `matches` returns true only on a full-string (anchored) match, like Java's
/// `Matcher.matches()`.
struct SimpleRegex {
    nodes: Vec<RegexNode>,
}

enum RegexNode {
    /// A literal string that must match exactly (from `\Q...\E` or a plain run).
    Literal(String),
    /// `.` — any single character.
    AnyChar,
    /// `[...]` character class: (negated, members as raw bytes for ranges).
    Class { negated: bool, spec: String },
    /// Alternation of sub-patterns (from `(?:A|B|...)` or top-level `A|B`).
    Alt(Vec<SimpleRegex>),
    /// A quantified node: the inner node with min/max repetition.
    Repeat {
        inner: Box<RegexNode>,
        min: usize,
        max: usize,
    },
}

impl SimpleRegex {
    fn compile(pattern: &str) -> Option<Self> {
        // Top-level alternation split (respecting group nesting).
        let alts = split_top_level_regex_alts(pattern)?;
        if alts.len() > 1 {
            let mut subs = Vec::with_capacity(alts.len());
            for a in alts {
                subs.push(SimpleRegex::compile(&a)?);
            }
            return Some(SimpleRegex {
                nodes: vec![RegexNode::Alt(subs)],
            });
        }

        let chars: Vec<char> = pattern.chars().collect();
        let mut nodes: Vec<RegexNode> = Vec::new();
        let mut i = 0usize;
        while i < chars.len() {
            let node = match chars[i] {
                '\\' => {
                    // \Q...\E literal quote, or escaped metacharacter.
                    if i + 1 < chars.len() && chars[i + 1] == 'Q' {
                        // find \E
                        let mut j = i + 2;
                        let mut lit = String::new();
                        while j < chars.len() {
                            if chars[j] == '\\' && j + 1 < chars.len() && chars[j + 1] == 'E' {
                                break;
                            }
                            lit.push(chars[j]);
                            j += 1;
                        }
                        if j >= chars.len() {
                            // unterminated \Q
                            return None;
                        }
                        i = j + 2; // skip \E
                        RegexNode::Literal(lit)
                    } else if i + 1 < chars.len() {
                        let c = chars[i + 1];
                        i += 2;
                        RegexNode::Literal(c.to_string())
                    } else {
                        return None;
                    }
                }
                '(' => {
                    // expect (?:...) non-capturing group
                    let body_start;
                    if chars.get(i + 1) == Some(&'?') && chars.get(i + 2) == Some(&':') {
                        body_start = i + 3;
                    } else {
                        body_start = i + 1;
                    }
                    let close = find_group_close(&chars, i)?;
                    let body: String = chars[body_start..close].iter().collect();
                    let sub = SimpleRegex::compile(&body)?;
                    i = close + 1;
                    RegexNode::Alt(vec![sub])
                }
                '[' => {
                    let mut j = i + 1;
                    let negated = chars.get(j) == Some(&'^');
                    if negated {
                        j += 1;
                    }
                    let spec_start = j;
                    // first ] can be a literal member
                    if chars.get(j) == Some(&']') {
                        j += 1;
                    }
                    while j < chars.len() && chars[j] != ']' {
                        j += 1;
                    }
                    if j >= chars.len() {
                        return None;
                    }
                    let spec: String = chars[spec_start..j].iter().collect();
                    i = j + 1;
                    RegexNode::Class { negated, spec }
                }
                '.' => {
                    i += 1;
                    RegexNode::AnyChar
                }
                c => {
                    i += 1;
                    RegexNode::Literal(c.to_string())
                }
            };

            // optional quantifier
            let (min, max, consumed) = match chars.get(i) {
                Some('*') => (0usize, usize::MAX, 1),
                Some('+') => (1usize, usize::MAX, 1),
                Some('?') => (0usize, 1usize, 1),
                _ => (1, 1, 0),
            };
            i += consumed;
            if min == 1 && max == 1 {
                nodes.push(node);
            } else {
                nodes.push(RegexNode::Repeat {
                    inner: Box::new(node),
                    min,
                    max,
                });
            }
        }

        Some(SimpleRegex { nodes })
    }

    /// Anchored, full-string match (Java `Matcher.matches`).
    fn matches(&self, text: &str) -> bool {
        let chars: Vec<char> = text.chars().collect();
        self.match_from(0, &chars, 0)
            .into_iter()
            .any(|end| end == chars.len())
    }

    /// Returns all possible end positions when matching `nodes[node_idx..]`
    /// starting at `pos`. Backtracking via the set of reachable ends.
    fn match_from(&self, node_idx: usize, text: &[char], pos: usize) -> Vec<usize> {
        if node_idx >= self.nodes.len() {
            return vec![pos];
        }
        let node = &self.nodes[node_idx];
        let mut results = Vec::new();
        for end in match_node(node, text, pos) {
            results.extend(self.match_from(node_idx + 1, text, end));
        }
        results
    }
}

/// Returns all end positions matching a single node at `pos`.
fn match_node(node: &RegexNode, text: &[char], pos: usize) -> Vec<usize> {
    match node {
        RegexNode::Literal(lit) => {
            let lit_chars: Vec<char> = lit.chars().collect();
            if pos + lit_chars.len() <= text.len()
                && text[pos..pos + lit_chars.len()] == lit_chars[..]
            {
                vec![pos + lit_chars.len()]
            } else {
                vec![]
            }
        }
        RegexNode::AnyChar => {
            if pos < text.len() {
                vec![pos + 1]
            } else {
                vec![]
            }
        }
        RegexNode::Class { negated, spec } => {
            if pos < text.len() && class_matches(spec, *negated, text[pos]) {
                vec![pos + 1]
            } else {
                vec![]
            }
        }
        RegexNode::Alt(subs) => {
            let mut out = Vec::new();
            for sub in subs {
                out.extend(sub.match_from(0, text, pos));
            }
            out
        }
        RegexNode::Repeat { inner, min, max } => {
            // collect reachable end positions for min..=max repetitions
            let mut ends = Vec::new();
            // positions reachable after exactly `count` repetitions
            let mut frontier = vec![pos];
            if *min == 0 {
                ends.push(pos);
            }
            let mut count = 0usize;
            while count < *max && !frontier.is_empty() {
                let mut next = Vec::new();
                for &p in &frontier {
                    for e in match_node(inner, text, p) {
                        if e != p {
                            next.push(e);
                        }
                    }
                }
                next.sort_unstable();
                next.dedup();
                count += 1;
                if count >= *min {
                    ends.extend(next.iter().copied());
                }
                frontier = next;
            }
            ends.sort_unstable();
            ends.dedup();
            ends
        }
    }
}

fn class_matches(spec: &str, negated: bool, ch: char) -> bool {
    let chars: Vec<char> = spec.chars().collect();
    let mut idx = 0;
    let mut matched = false;
    while idx < chars.len() {
        if idx + 2 < chars.len() && chars[idx + 1] == '-' && chars[idx + 2] != ']' {
            let lo = chars[idx];
            let hi = chars[idx + 2];
            let (lo, hi) = if lo <= hi { (lo, hi) } else { (hi, lo) };
            if lo <= ch && ch <= hi {
                matched = true;
            }
            idx += 3;
        } else {
            if chars[idx] == ch {
                matched = true;
            }
            idx += 1;
        }
    }
    if negated {
        !matched
    } else {
        matched
    }
}

/// Find the index of the `)` closing the group that opens at `open` (which must
/// be `(`), respecting nesting.
fn find_group_close(chars: &[char], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = open;
    while i < chars.len() {
        match chars[i] {
            '\\' => i += 1, // skip escaped char
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Split a regex on top-level `|`, respecting `(...)` groups, `[...]` classes
/// and `\Q...\E` literals (so `|` inside any of those is not a split point).
fn split_top_level_regex_alts(pattern: &str) -> Option<Vec<String>> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut depth = 0usize;
    let mut i = 0usize;
    while i < chars.len() {
        match chars[i] {
            '\\' => {
                if chars.get(i + 1) == Some(&'Q') {
                    // skip to \E
                    let mut j = i + 2;
                    while j + 1 < chars.len() && !(chars[j] == '\\' && chars[j + 1] == 'E') {
                        j += 1;
                    }
                    i = j + 1; // land on E (or near end); loop will +1
                } else {
                    i += 1; // skip escaped char
                }
            }
            '(' => depth += 1,
            ')' => {
                if depth == 0 {
                    return None;
                }
                depth -= 1;
            }
            '[' => {
                // skip to matching ]
                let mut j = i + 1;
                if chars.get(j) == Some(&'^') {
                    j += 1;
                }
                if chars.get(j) == Some(&']') {
                    j += 1;
                }
                while j < chars.len() && chars[j] != ']' {
                    j += 1;
                }
                i = j;
            }
            '|' if depth == 0 => {
                parts.push(chars[start..i].iter().collect::<String>());
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    parts.push(chars[start..].iter().collect::<String>());
    Some(parts)
}

/// Port of Java `java.util.regex.Pattern.quote`: wraps the literal in
/// `\Q...\E`, splitting any embedded `\E` so the quoting stays well-formed.
fn regex_quote(s: &str) -> String {
    if s.is_empty() {
        // Pattern.quote("") returns "\\Q\\E"
        return "\\Q\\E".to_string();
    }
    if !s.contains("\\E") {
        return format!("\\Q{s}\\E");
    }
    let mut out = String::from("\\Q");
    let mut rest = s;
    while let Some(idx) = rest.find("\\E") {
        out.push_str(&rest[..idx]);
        out.push_str("\\E\\\\E\\Q");
        rest = &rest[idx + 2..];
    }
    out.push_str(rest);
    out.push_str("\\E");
    out
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
        if axis_prefix_matches(&p, Z_PREFIXES) {
            return AxisType::Z;
        }
        if axis_prefix_matches(&p, T_PREFIXES) {
            return AxisType::Time;
        }
        if axis_prefix_matches(&p, C_PREFIXES) {
            return AxisType::Channel;
        }
        if axis_prefix_matches(&p, S_PREFIXES) {
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

fn axis_prefix_matches(label: &str, prefixes: &[&str]) -> bool {
    prefixes
        .iter()
        .any(|prefix| label == *prefix || label.ends_with(prefix))
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
            regex_files: None,
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
            metas: vec![meta],
            plane_maps: vec![Vec::new()],
            current_series: 0,
            no_stitch: false,
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
    fn stitch_layout_uses_axis_guesser_adjusted_dimension_order() {
        let fp = pattern(&[("z", 2), ("_", 2)]);
        let files = fp.filenames();
        let base_meta = ImageMetadata {
            size_x: 1,
            size_y: 1,
            size_z: 2,
            size_c: 1,
            size_t: 1,
            image_count: 2,
            dimension_order: crate::common::metadata::DimensionOrder::XYZCT,
            ..ImageMetadata::default()
        };

        let (stitched, plane_map) = stitch_layout(&files, &base_meta, Some(&fp)).unwrap();

        assert_eq!(
            stitched.dimension_order,
            crate::common::metadata::DimensionOrder::XYTCZ
        );
        assert_eq!(
            (stitched.size_z, stitched.size_c, stitched.size_t),
            (4, 2, 1)
        );
        assert_eq!(stitched.image_count, 8);
        assert_eq!(plane_map.len(), 8);
    }

    #[test]
    fn stitch_layout_no_axis_fallback_uses_time_like_java() {
        let files = vec![
            PathBuf::from("a.fake"),
            PathBuf::from("b.fake"),
            PathBuf::from("c.fake"),
        ];
        let base_meta = ImageMetadata {
            size_x: 1,
            size_y: 1,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            image_count: 1,
            dimension_order: crate::common::metadata::DimensionOrder::XYCZT,
            ..ImageMetadata::default()
        };

        let (stitched, plane_map) = stitch_layout(&files, &base_meta, None).unwrap();

        assert_eq!(
            (stitched.size_z, stitched.size_c, stitched.size_t),
            (1, 1, 3)
        );
        assert_eq!(stitched.image_count, 3);
        assert_eq!(plane_map, vec![(0, 0, 0), (1, 0, 0), (2, 0, 0)]);
    }

    #[test]
    fn stitch_layout_rgb_uses_effective_c_for_plane_count() {
        let files = vec![PathBuf::from("a.fake"), PathBuf::from("b.fake")];
        let base_meta = ImageMetadata {
            size_x: 1,
            size_y: 1,
            size_z: 1,
            size_c: 6,
            size_t: 1,
            image_count: 2,
            is_rgb: true,
            is_interleaved: true,
            dimension_order: crate::common::metadata::DimensionOrder::XYCZT,
            ..ImageMetadata::default()
        };

        let (stitched, plane_map) = stitch_layout(&files, &base_meta, None).unwrap();

        assert_eq!(
            (stitched.size_z, stitched.size_c, stitched.size_t),
            (1, 6, 2)
        );
        assert_eq!(stitched.image_count, 4);
        assert_eq!(plane_map, vec![(0, 0, 0), (0, 0, 1), (1, 0, 0), (1, 0, 1)]);
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
    fn axis_guesser_matches_java_ends_with_prefix_rule() {
        let fp = pattern(&[("tilez", 2), ("laserch", 2), ("elapsedtime", 2)]);
        let guess = AxisGuesser::guess_with_dims(&fp, "XYZCT", 1, 1, 1, false);

        assert_eq!(
            guess.axis_types,
            vec![AxisType::Z, AxisType::Channel, AxisType::Time]
        );
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
    fn range_block_non_fixed_width_is_not_zero_padded() {
        // Java FilePatternBlock.explode: fixed = b.length()==e.length(). For
        // <8-10> begin width 1 != end width 2, so NOT fixed -> no padding.
        assert_eq!(
            parse_explicit_pattern_part("8-10").unwrap(),
            vec!["8", "9", "10"]
        );
    }

    #[test]
    fn range_block_fixed_width_is_zero_padded_to_end_length() {
        // <08-10>: begin width 2 == end width 2 -> fixed, pad to end length.
        assert_eq!(
            parse_explicit_pattern_part("08-10").unwrap(),
            vec!["08", "09", "10"]
        );
    }

    #[test]
    fn range_block_with_step() {
        assert_eq!(
            parse_explicit_pattern_part("0-6:3").unwrap(),
            vec!["0", "3", "6"]
        );
    }

    #[test]
    fn range_block_single_char_alpha() {
        assert_eq!(
            parse_explicit_pattern_part("C-E").unwrap(),
            vec!["C", "D", "E"]
        );
    }

    #[test]
    fn range_block_multichar_base36_alpha_lowercase() {
        // Base-36 range "aa" (=370) to "ac" (=372); first char lowercase so
        // values are lowercased. Both width 2 -> fixed (already equal width).
        assert_eq!(
            parse_explicit_pattern_part("aa-ac").unwrap(),
            vec!["aa", "ab", "ac"]
        );
    }

    #[test]
    fn descending_numeric_range_counts_down() {
        // Java's count = (end-begin)/step+1 = (3-5)/1+1 = -1 would yield an empty
        // expansion. As a deliberate non-upstream extension (see the
        // `filepattern_reader_expands_descending_ranges` reader test) a descending
        // range with a positive step instead counts down.
        assert_eq!(parse_explicit_pattern_part("5-3").unwrap(), ["5", "4", "3"]);
    }

    #[test]
    fn get_bounds_constant_step() {
        assert_eq!(get_bounds(&[0, 3, 6], false).as_deref(), Some("<0-6:3>"));
        assert_eq!(get_bounds(&[1, 2, 3], false).as_deref(), Some("<1-3>"));
        // fixed pads START so widths match.
        assert_eq!(get_bounds(&[8, 9, 10], true).as_deref(), Some("<08-10>"));
        // non-constant step -> None
        assert_eq!(get_bounds(&[1, 2, 4], false), None);
        // single element -> None
        assert_eq!(get_bounds(&[5], false), None);
    }

    #[test]
    fn find_pattern_from_names_builds_alternation() {
        let names = vec!["a.tif".to_string(), "b.tif".to_string()];
        assert_eq!(
            find_pattern_from_names(&names),
            "\\Q\\E(?:\\Qa.tif\\E)|(?:\\Qb.tif\\E)"
        );
    }

    #[test]
    fn number_filter_extracts_middle_number() {
        let f = NumberFilter::new("z", "c1.tif");
        assert_eq!(f.get_number("z10c1.tif"), Some(10));
        assert!(f.accept("z9c1.tif"));
        assert!(!f.accept("z9c2.tif")); // wrong suffix
        assert_eq!(f.get_number("zXc1.tif"), None); // non-numeric middle
    }

    #[test]
    fn get_axis_type_matches_endswith() {
        // 'z' prefix -> Z_AXIS = 1; 'c' -> C_AXIS = 3; 't' -> T_AXIS = 2;
        // 's' -> S_AXIS = 4; unknown -> 0. endsWith semantics: "img_z" ends
        // with "z".
        assert_eq!(get_axis_type("img_z"), 1);
        assert_eq!(get_axis_type("c"), 3);
        assert_eq!(get_axis_type("foo_t"), 2);
        assert_eq!(get_axis_type("s"), 4);
        assert_eq!(get_axis_type("foo_"), 0);
    }

    #[test]
    fn find_pattern_variable_width_block_becomes_range() {
        // name z10c1.tif, candidates z9c1/z10c1 differ -> variable width (lengths
        // differ) -> <9-10> range on the first block; second block constant.
        let names: Vec<String> = ["z9c1.tif", "z10c1.tif", "z9c2.tif", "z10c2.tif", "foo.tif"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let pat = find_pattern_in_dir("z10c1.tif", "", &names);
        // First block is variable-width (z9 vs z10 differ in length) -> <9-10>;
        // the c block matches z10c1/z10c2 (same length) -> fixed <1-2>.
        assert_eq!(pat.as_deref(), Some("z<9-10>c<1-2>.tif"));
    }

    #[test]
    fn find_pattern_fixed_width_two_blocks() {
        // All names same length -> fixed-width path. Both blocks vary across the
        // set, so each becomes a fixed-width range.
        let names: Vec<String> = ["z1c1.tif", "z1c2.tif", "z2c1.tif", "z2c2.tif"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let pat = find_pattern_in_dir("z1c1.tif", "", &names);
        assert_eq!(pat.as_deref(), Some("z<1-2>c<1-2>.tif"));
    }

    #[test]
    fn find_pattern_constant_block_when_single_match() {
        // Only one file matches the block-0 filter -> constant, no range.
        let names: Vec<String> = ["z5.tif"].iter().map(|s| s.to_string()).collect();
        let pat = find_pattern_in_dir("z5.tif", "", &names);
        assert_eq!(pat.as_deref(), Some("z5.tif"));
    }

    #[test]
    fn find_pattern_with_excludes_keeps_series_literal() {
        // Excluding the S axis keeps the s-block literal but still merges z.
        let names: Vec<String> = ["s1_z1.tif", "s1_z2.tif", "s2_z1.tif", "s2_z2.tif"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let pat = find_pattern_with_excludes("s1_z1.tif", "", &names, &[4]);
        assert_eq!(pat.as_deref(), Some("s1_z<1-2>.tif"));
    }

    #[test]
    fn find_series_patterns_splits_per_series() {
        let names: Vec<String> = [
            "foo_s1_z1.ext",
            "foo_s1_z2.ext",
            "foo_s2_z1.ext",
            "foo_s2_z2.ext",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        // No files actually exist on disk so the existence/membership filters in
        // findSeriesPatterns drop everything; verify the underlying per-name
        // pattern with S excluded instead (the core splitting logic).
        let p1 = find_pattern_with_excludes("foo_s1_z1.ext", "", &names, &[4]);
        let p2 = find_pattern_with_excludes("foo_s2_z1.ext", "", &names, &[4]);
        assert_eq!(p1.as_deref(), Some("foo_s1_z<1-2>.ext"));
        assert_eq!(p2.as_deref(), Some("foo_s2_z<1-2>.ext"));
    }

    #[test]
    fn simple_regex_quote_alternation() {
        // Pattern produced by find_pattern_from_names.
        let names = vec!["a.tif".to_string(), "b.tif".to_string()];
        let pat = find_pattern_from_names(&names);
        let re = SimpleRegex::compile(&pat).unwrap();
        assert!(re.matches("a.tif"));
        assert!(re.matches("b.tif"));
        assert!(!re.matches("c.tif"));
        assert!(!re.matches("a.tiff"));
    }

    #[test]
    fn simple_regex_dot_star_and_class() {
        let re = SimpleRegex::compile("z.*\\.tif").unwrap();
        assert!(re.matches("z1.tif"));
        assert!(re.matches("z.tif"));
        assert!(re.matches("zABC.tif"));
        assert!(!re.matches("y1.tif"));

        let re2 = SimpleRegex::compile("img_[0-2]+\\.fake").unwrap();
        assert!(re2.matches("img_012.fake"));
        assert!(!re2.matches("img_3.fake"));
        assert!(!re2.matches("img_.fake"));
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

    /// Create a uniquely-named temp directory and run `body` with it, cleaning
    /// up afterwards. Avoids pulling in extra crates.
    fn with_temp_dir<F: FnOnce(&Path)>(tag: &str, body: F) {
        let mut dir = std::env::temp_dir();
        let unique = format!(
            "bf_stitcher_{tag}_{}_{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        dir.push(unique);
        std::fs::create_dir_all(&dir).unwrap();
        body(&dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn touch(path: &Path) {
        std::fs::write(path, b"x").unwrap();
    }

    #[test]
    fn from_explicit_pattern_blockless_routes_through_regex_buildfiles() {
        // A block-less pattern (no <...>) is Java's regex branch of buildFiles:
        // the pattern string is matched as a regex against existing files. Here
        // the pattern names an existing file, so build_files_regex returns just
        // that file (the `Location(pattern).exists()` short-circuit).
        with_temp_dir("blockless", |dir| {
            let file = dir.join("plain_name.fake");
            touch(&file);
            let fp = FilePattern::from_explicit_pattern(&file).unwrap();
            // No numeric blocks were parsed; this is the regex/file-list mode.
            assert!(fp.blocks.is_empty());
            let files = fp.filenames();
            assert_eq!(files, vec![file.clone()]);
        });
    }

    #[test]
    fn from_explicit_pattern_blockless_regex_matches_directory_files() {
        // Block-less pattern that does NOT name an existing file is compiled as
        // a regex and matched against the directory listing (Java buildFiles
        // regex branch via build_files_regex -> SimpleRegex).
        with_temp_dir("regexdir", |dir| {
            touch(&dir.join("z0.fake"));
            touch(&dir.join("z1.fake"));
            touch(&dir.join("y0.fake")); // must NOT match z.*
                                         // dir-prefixed regex: <dir>/z.*\.fake  (escaped separator + regex).
            let pat = format!(
                "{}{}z.*\\.fake",
                dir.to_str().unwrap(),
                std::path::MAIN_SEPARATOR
            );
            let fp = FilePattern::from_explicit_pattern(Path::new(&pat)).unwrap();
            assert!(fp.blocks.is_empty());
            let mut names: Vec<String> = fp
                .filenames()
                .iter()
                .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
                .collect();
            names.sort();
            assert_eq!(names, vec!["z0.fake".to_string(), "z1.fake".to_string()]);
        });
    }

    #[test]
    fn from_file_routes_through_find_pattern_and_enumerates_existing_files() {
        // Java FilePattern(Location) -> findPattern -> FilePattern(String).
        // from_file should discover the z<1-3> sequence and enumerate exactly
        // the existing files.
        with_temp_dir("fromfile", |dir| {
            let f1 = dir.join("img_z1.fake");
            let f2 = dir.join("img_z2.fake");
            let f3 = dir.join("img_z3.fake");
            touch(&f1);
            touch(&f2);
            touch(&f3);

            let fp = FilePattern::from_file(&f1).unwrap();
            // findPattern collapsed the three files into one <1-3> block.
            assert_eq!(fp.blocks.len(), 1);
            let mut files = fp.filenames();
            files.sort();
            assert_eq!(files, vec![f1.clone(), f2.clone(), f3.clone()]);
        });
    }

    #[test]
    fn from_file_series_splits_one_pattern_per_series() {
        // Java findSeriesPatterns: per-series patterns with the S axis held
        // literal. foo_s1_z{1,2} and foo_s2_z{1,2} -> two z<1-2> patterns.
        with_temp_dir("series", |dir| {
            for s in 1..=2 {
                for z in 1..=2 {
                    touch(&dir.join(format!("foo_s{s}_z{z}.fake")));
                }
            }
            let base = dir.join("foo_s1_z1.fake");
            let patterns = FilePattern::from_file_series(&base).unwrap();
            assert_eq!(patterns.len(), 2);
            // Each series pattern enumerates exactly its two z planes.
            for fp in &patterns {
                assert_eq!(fp.filenames().len(), 2);
            }
            // The two series cover distinct files.
            let mut all: Vec<PathBuf> = patterns.iter().flat_map(|fp| fp.filenames()).collect();
            all.sort();
            all.dedup();
            assert_eq!(all.len(), 4);
        });
    }
}
