//! Prairie Technologies PrairieView and Leica TCS XML+TIFF series readers.
//!
//! Both formats use an XML metadata file that references companion TIFF files.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::path::confined_join;
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::tiff::TiffReader;

// ── Minimal XML attribute parser ──────────────────────────────────────────────

/// Extract the value of a named attribute from an XML tag string.
/// e.g. extract_attr(`key="pixelsPerLine" value="512"`, "value") → Some("512")
fn extract_attr<'a>(text: &'a str, attr: &str) -> Option<&'a str> {
    let search = format!("{}=\"", attr);
    let start = text.find(search.as_str())? + search.len();
    let end = text[start..].find('"')? + start;
    Some(&text[start..end])
}

fn extract_attr_owned(text: &str, attr: &str) -> Option<String> {
    extract_attr(text, attr).map(|s| s.to_string())
}

// ── Prairie Technologies Reader ───────────────────────────────────────────────

/// A single `<File channel=.. filename=..>` element, with optional page.
#[derive(Clone)]
struct PFile {
    channel: i32,
    filename: PathBuf,
    page: u32,
}

/// A `<Frame>` element: one focal-plane (or time) sample, holding one file per
/// active channel, plus the per-frame stage position (Java `Frame.getPositionX/Y/Z`).
struct PFrame {
    /// `index` attribute of the `<Frame>` (Java `Frame.getIndex()`).
    index: i32,
    files: Vec<PFile>,
    /// Stage X/Y/Z from `<PVStateValue key="positionCurrent">` SubindexedValues,
    /// or pre-5.2 `positionCurrent_XAxis` Keys, or `<Frame>` position attrs.
    pos_x: Option<f64>,
    pos_y: Option<f64>,
    pos_z: Option<f64>,
}

impl PFrame {
    fn file_for_channel(&self, channel: i32) -> Option<&PFile> {
        self.files.iter().find(|f| f.channel == channel)
    }
}

/// A `<Sequence>` element: a cycle (stage position or time point) containing an
/// ordered list of frames.
struct Sequence {
    is_time_series: bool,
    frames: Vec<PFrame>,
}

impl Sequence {
    /// Smallest `index` attribute among the frames (Java `Sequence.getIndexMin`).
    fn index_min(&self) -> i32 {
        self.frames.iter().map(|f| f.index).min().unwrap_or(0)
    }
    /// `indexMax - indexMin + 1` (Java `Sequence.getIndexCount`).
    fn index_count(&self) -> i32 {
        match (
            self.frames.iter().map(|f| f.index).min(),
            self.frames.iter().map(|f| f.index).max(),
        ) {
            (Some(lo), Some(hi)) => hi - lo + 1,
            _ => self.frames.len() as i32,
        }
    }
    /// Frame whose `index` attribute equals `index` (Java `Sequence.getFrame`).
    fn frame(&self, index: i32) -> Option<&PFrame> {
        self.frames.iter().find(|f| f.index == index)
    }
}

pub struct PrairieReader {
    path: Option<PathBuf>,
    /// Per-series core metadata (one entry per stage position).
    metas: Vec<ImageMetadata>,
    series: usize,
    sequences: Vec<Sequence>,
    /// Sorted active channel indices.
    channels: Vec<i32>,
    /// Whether frames act as time points rather than focal planes; one flag per
    /// series (parallel to `metas`), mirroring Java `framesAreTime[]`.
    frames_are_time: Vec<bool>,
    /// Number of stage positions == series count (Java `seriesCount`).
    size_p: usize,
}

impl PrairieReader {
    pub fn new() -> Self {
        PrairieReader {
            path: None,
            metas: Vec::new(),
            series: 0,
            sequences: Vec::new(),
            channels: Vec::new(),
            frames_are_time: Vec::new(),
            size_p: 1,
        }
    }
}

impl Default for PrairieReader {
    fn default() -> Self {
        Self::new()
    }
}

/// Locate the PrairieView `.xml` metadata file for a given entry path. Accepts
/// `.xml`, `.cfg`, `.env`, or `.tif`/`.tiff` entry points (matching Java).
fn find_prairie_xml(path: &Path) -> Option<PathBuf> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    if ext.as_deref() == Some("xml") {
        return Some(path.to_path_buf());
    }
    let parent = path.parent()?;
    // Build a prefix from the file stem, trimming "Config" / trailing "_..".
    let mut prefix = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();
    if ext.as_deref() == Some("cfg") {
        if let Some(i) = prefix.rfind("Config") {
            prefix.truncate(i);
        }
    }
    loop {
        let cand = parent.join(format!("{prefix}.xml"));
        if cand.exists() {
            return Some(cand);
        }
        match prefix.rfind('_') {
            Some(i) => prefix.truncate(i),
            None => break,
        }
    }
    // Fall back to the first .xml in the directory.
    std::fs::read_dir(parent).ok().and_then(|rd| {
        rd.filter_map(|e| e.ok()).map(|e| e.path()).find(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("xml"))
                .unwrap_or(false)
        })
    })
}

/// Result of parsing a PrairieView XML document: one core `ImageMetadata` per
/// stage position (series), the rasterized sequences, the active channels, the
/// per-series `framesAreTime` flags, and the (sizeP, sizeT) split.
struct PrairieParse {
    metas: Vec<ImageMetadata>,
    sequences: Vec<Sequence>,
    channels: Vec<i32>,
    frames_are_time: Vec<bool>,
    size_p: usize,
    size_t: usize,
}

/// Parse the PrairieView XML into sequences/frames/files plus per-series core
/// metadata.
fn parse_prairie_xml(path: &Path) -> Result<PrairieParse> {
    let content = std::fs::read_to_string(path).map_err(BioFormatsError::Io)?;
    let dir = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();

    let mut width = 0u32;
    let mut height = 0u32;
    let mut bits = 0u32;

    let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
    meta_map.insert(
        "format".into(),
        MetadataValue::String("Prairie TIFF".into()),
    );

    // Read global PVStateValue hints and document-level attributes.
    for line in content.lines() {
        let line = line.trim();

        // <PVScan date=".." version=".."> top-level attributes.
        if line.contains("<PVScan") {
            if let Some(date) = extract_attr_owned(line, "date") {
                meta_map.insert("date".into(), MetadataValue::String(date));
            }
            if let Some(version) = extract_attr_owned(line, "version") {
                meta_map.insert("version".into(), MetadataValue::String(version));
            }
        }

        if line.contains("PVStateValue") {
            if let (Some(key), Some(val)) = (extract_attr(line, "key"), extract_attr(line, "value"))
            {
                match key {
                    "pixelsPerLine" => width = val.parse().unwrap_or(width),
                    "linesPerFrame" => height = val.parse().unwrap_or(height),
                    "bitDepth" => bits = val.parse().unwrap_or(bits),
                    // Physical calibration (microns per pixel) and zoom, mirroring
                    // the OME PhysicalSize / DetectorZoom Java populates.
                    "micronsPerPixel" => {
                        if let Ok(v) = val.parse::<f64>() {
                            if v > 0.0 {
                                meta_map
                                    .entry("physicalSizeX".into())
                                    .or_insert(MetadataValue::Float(v));
                                meta_map
                                    .entry("physicalSizeY".into())
                                    .or_insert(MetadataValue::Float(v));
                            }
                        }
                    }
                    "opticalZoom" => {
                        if let Ok(v) = val.parse::<f64>() {
                            meta_map.insert("opticalZoom".into(), MetadataValue::Float(v));
                        }
                    }
                    _ => {}
                }
            }
        }

        // Per-axis micronsPerPixel: <IndexedValue index="XAxis" value="0.5"/>.
        if line.contains("micronsPerPixel") || line.contains("IndexedValue") {
            if let (Some(idx), Some(val)) =
                (extract_attr(line, "index"), extract_attr(line, "value"))
            {
                if let Ok(v) = val.parse::<f64>() {
                    if v > 0.0 {
                        match idx {
                            "XAxis" => {
                                meta_map.insert("physicalSizeX".into(), MetadataValue::Float(v));
                            }
                            "YAxis" => {
                                meta_map.insert("physicalSizeY".into(), MetadataValue::Float(v));
                            }
                            "ZAxis" => {
                                meta_map.insert("physicalSizeZ".into(), MetadataValue::Float(v));
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    // Parse Sequence / Frame / File structure.
    let mut sequences: Vec<Sequence> = Vec::new();
    let mut active_channels: BTreeSet<i32> = BTreeSet::new();
    let mut cur_seq: Option<Sequence> = None;
    let mut cur_frame: Option<PFrame> = None;
    let mut next_frame_index: i32 = 0;
    // State for parsing a `<PVStateValue key="positionCurrent">` block (5.2+):
    // which axis the current `<SubindexedValues>`/`<IndexedValue>` belongs to,
    // and whether we are inside the positionCurrent block at all.
    let mut in_position_block = false;
    let mut cur_axis: Option<char> = None;

    for raw in content.lines() {
        let line = raw.trim();

        if line.contains("<Sequence") {
            // flush a dangling frame/sequence
            if let Some(frame) = cur_frame.take() {
                if let Some(seq) = cur_seq.as_mut() {
                    seq.frames.push(frame);
                }
            }
            if let Some(seq) = cur_seq.take() {
                sequences.push(seq);
            }
            let ty = extract_attr(line, "type").unwrap_or("");
            cur_seq = Some(Sequence {
                // Java: isTimeSeries() == ("TSeries Timed Element".equals(type)).
                is_time_series: ty == "TSeries Timed Element"
                    || ty.to_ascii_lowercase().contains("tseries"),
                frames: Vec::new(),
            });
            next_frame_index = 0;
        }

        if line.contains("</Sequence>") {
            if let Some(frame) = cur_frame.take() {
                if let Some(seq) = cur_seq.as_mut() {
                    seq.frames.push(frame);
                }
            }
            if let Some(seq) = cur_seq.take() {
                sequences.push(seq);
            }
        }

        if line.contains("<Frame") {
            if let Some(frame) = cur_frame.take() {
                if let Some(seq) = cur_seq.as_mut() {
                    seq.frames.push(frame);
                }
            }
            // If no Sequence was declared, start an implicit one.
            if cur_seq.is_none() {
                cur_seq = Some(Sequence {
                    is_time_series: false,
                    frames: Vec::new(),
                });
            }
            // `index` attribute, or sequential fallback (Java requires it but we
            // tolerate its absence in synthetic data).
            let index = extract_attr(line, "index")
                .and_then(|v| v.parse::<i32>().ok())
                .unwrap_or(next_frame_index);
            next_frame_index = index + 1;
            in_position_block = false;
            cur_axis = None;
            cur_frame = Some(PFrame {
                index,
                files: Vec::new(),
                // `<Frame>` position attributes act as a fallback for the
                // SubindexedValues form below.
                pos_x: extract_attr(line, "positionX").and_then(|v| v.parse().ok()),
                pos_y: extract_attr(line, "positionY").and_then(|v| v.parse().ok()),
                pos_z: extract_attr(line, "positionZ").and_then(|v| v.parse().ok()),
            });
        }

        // ── Per-frame stage position parsing ───────────────────────────────
        // Java reads Frame.getPositionX/Y/Z from a `positionCurrent` value
        // table. We capture it from the line-based XML in three forms.
        if cur_frame.is_some() {
            // pre-5.2 single Keys: <Key key="positionCurrent_XAxis" value=".."/>
            if line.contains("<Key") {
                if let (Some(key), Some(val)) =
                    (extract_attr(line, "key"), extract_attr(line, "value"))
                {
                    if let Some(rest) = key.strip_prefix("positionCurrent_") {
                        if let Ok(v) = val.split(',').next().unwrap_or(val).parse::<f64>() {
                            if let Some(f) = cur_frame.as_mut() {
                                match rest {
                                    "XAxis" => f.pos_x = Some(v),
                                    "YAxis" => f.pos_y = Some(v),
                                    "ZAxis" => f.pos_z = Some(v),
                                    _ => {}
                                }
                            }
                        }
                    }
                }
            }

            // 5.2+ block start/end.
            if line.contains("key=\"positionCurrent\"") {
                in_position_block = true;
                cur_axis = None;
                // Inline IndexedValue on the same line is handled below.
            }
            if line.contains("</PVStateValue>") {
                in_position_block = false;
                cur_axis = None;
            }

            if in_position_block {
                // Track the axis declared by <SubindexedValues index="XAxis">.
                if line.contains("<SubindexedValues") {
                    cur_axis = match extract_attr(line, "index") {
                        Some("XAxis") => Some('x'),
                        Some("YAxis") => Some('y'),
                        Some("ZAxis") => Some('z'),
                        _ => None,
                    };
                }
                // First <SubindexedValue subindex="0" value=".."> per axis is the
                // value Java's single-entry ValueTable short-circuit returns.
                if line.contains("<SubindexedValue ") {
                    let is_zero = extract_attr(line, "subindex")
                        .map(|s| s == "0")
                        .unwrap_or(true);
                    if is_zero {
                        if let (Some(axis), Some(val)) = (
                            cur_axis,
                            extract_attr(line, "value").and_then(|v| v.parse::<f64>().ok()),
                        ) {
                            if let Some(f) = cur_frame.as_mut() {
                                match axis {
                                    'x' if f.pos_x.is_none() => f.pos_x = Some(val),
                                    'y' if f.pos_y.is_none() => f.pos_y = Some(val),
                                    'z' if f.pos_z.is_none() => f.pos_z = Some(val),
                                    _ => {}
                                }
                            }
                        }
                    }
                }
                // 5.2+ <IndexedValue index="XAxis" value=".."/> form.
                if line.contains("<IndexedValue") {
                    if let (Some(idx), Some(val)) = (
                        extract_attr(line, "index"),
                        extract_attr(line, "value").and_then(|v| v.parse::<f64>().ok()),
                    ) {
                        if let Some(f) = cur_frame.as_mut() {
                            match idx {
                                "XAxis" => f.pos_x = Some(val),
                                "YAxis" => f.pos_y = Some(val),
                                "ZAxis" => f.pos_z = Some(val),
                                _ => {}
                            }
                        }
                    }
                }
            }
        }

        if line.contains("<File") {
            if let Some(fname) = extract_attr(line, "filename") {
                let channel = extract_attr(line, "channel")
                    .and_then(|c| c.parse::<i32>().ok())
                    .unwrap_or(1);
                let page = extract_attr(line, "page")
                    .and_then(|p| p.parse::<u32>().ok())
                    .unwrap_or(1)
                    .saturating_sub(1);
                active_channels.insert(channel);
                // Per-channel name (Java setChannelName from File.getChannelName).
                if let Some(cname) = extract_attr_owned(line, "channelName") {
                    meta_map
                        .entry(format!("channel_name[{}]", channel))
                        .or_insert(MetadataValue::String(cname));
                }
                let Some(filename) = confined_join(&dir, fname) else {
                    continue;
                };
                let pfile = PFile {
                    channel,
                    filename,
                    page,
                };
                if cur_frame.is_none() {
                    if cur_seq.is_none() {
                        cur_seq = Some(Sequence {
                            is_time_series: false,
                            frames: Vec::new(),
                        });
                    }
                    cur_frame = Some(PFrame {
                        index: next_frame_index,
                        files: Vec::new(),
                        pos_x: None,
                        pos_y: None,
                        pos_z: None,
                    });
                    next_frame_index += 1;
                }
                if let Some(frame) = cur_frame.as_mut() {
                    frame.files.push(pfile);
                }
            }
        }
    }
    if let Some(frame) = cur_frame.take() {
        if let Some(seq) = cur_seq.as_mut() {
            seq.frames.push(frame);
        }
    }
    if let Some(seq) = cur_seq.take() {
        sequences.push(seq);
    }

    let has_files = sequences
        .iter()
        .any(|s| s.frames.iter().any(|f| !f.files.is_empty()));
    if !has_files {
        return Err(BioFormatsError::UnsupportedFormat(
            "PrairieView XML does not reference any companion TIFF image files".into(),
        ));
    }

    let channels: Vec<i32> = active_channels.into_iter().collect();
    if channels.is_empty() {
        return Err(BioFormatsError::Format(
            "PrairieView XML does not declare any file channels".into(),
        ));
    }
    let size_c = channels.len() as u32;

    // NB: Both stage positions and time points are rasterized into the list of
    // Sequences. So sequenceCount = sizeT * seriesCount (Java
    // populateCoreMetadata). We separate the two by comparing per-frame stage
    // positions (Java computeSizeT / positionsMatch).
    let sequence_count = sequences.len().max(1);
    let size_t = compute_size_t(&sequences, sequence_count);
    let size_p = sequence_count / size_t.max(1); // seriesCount

    // Derive pixel type / dimensions from the first available TIFF.
    let mut pixel_type = match bits {
        0 => PixelType::Uint16,
        8 => PixelType::Uint8,
        16 => PixelType::Uint16,
        32 => PixelType::Float32,
        _ => {
            return Err(BioFormatsError::Format(format!(
                "Prairie: unsupported bitDepth {bits}"
            )))
        }
    };
    let mut is_little_endian = true;
    let first_file = sequences
        .iter()
        .flat_map(|s| s.frames.iter())
        .flat_map(|f| f.files.iter())
        .map(|pf| pf.filename.clone())
        .next();
    if let Some(ff) = first_file {
        let mut r = TiffReader::new();
        r.set_id(&ff).map_err(|e| {
            BioFormatsError::Format(format!(
                "Prairie: companion TIFF {} could not be read before metadata was initialized: {e}",
                ff.display()
            ))
        })?;
        let tm = r.metadata();
        pixel_type = tm.pixel_type;
        is_little_endian = tm.is_little_endian;
        if width == 0 {
            width = tm.size_x;
        }
        if height == 0 {
            height = tm.size_y;
        }
        if bits == 0 {
            bits = tm.bits_per_pixel as u32;
        }
        let _ = r.close();
    }

    let mut checked_tiffs: HashMap<PathBuf, u32> = HashMap::new();
    for pf in sequences
        .iter()
        .flat_map(|s| s.frames.iter())
        .flat_map(|f| f.files.iter())
    {
        let pages = if let Some(pages) = checked_tiffs.get(&pf.filename) {
            *pages
        } else {
            let mut r = TiffReader::new();
            r.set_id(&pf.filename).map_err(|e| {
                BioFormatsError::Format(format!(
                    "Prairie: companion TIFF {} could not be read before metadata was initialized: {e}",
                    pf.filename.display()
                ))
            })?;
            let tm = r.metadata();
            if tm.size_x != width || tm.size_y != height {
                return Err(BioFormatsError::Format(format!(
                    "Prairie: companion TIFF {} has dimensions {}x{}, expected {width}x{height}",
                    pf.filename.display(),
                    tm.size_x,
                    tm.size_y
                )));
            }
            let pages = tm.image_count.max(1);
            let _ = r.close();
            checked_tiffs.insert(pf.filename.clone(), pages);
            pages
        };
        if pf.page >= pages {
            return Err(BioFormatsError::Format(format!(
                "Prairie: TIFF page {} out of range for {} ({} pages)",
                pf.page,
                pf.filename.display(),
                pages
            )));
        }
    }
    if width == 0 || height == 0 {
        return Err(BioFormatsError::Format(format!(
            "Prairie: invalid image dimensions {width}x{height}"
        )));
    }
    if bits == 0 {
        bits = 16;
    }

    // Original metadata Java populates in populateOriginalMetadata.
    meta_map.insert(
        "sequenceCount".into(),
        MetadataValue::Int(sequence_count as i64),
    );
    meta_map.insert(
        "activeChannelCount".into(),
        MetadataValue::Int(size_c as i64),
    );

    // Build one CoreMetadata (ImageMetadata) per stage position (series).
    // Rasterization order is sequences[sizeP * t + p]; for series s the first
    // sequence is sequences[s] (t == 0).
    let mut metas: Vec<ImageMetadata> = Vec::with_capacity(size_p);
    let mut frames_are_time: Vec<bool> = Vec::with_capacity(size_p);
    for s in 0..size_p {
        // sequences[sizeP * t + s] with t == 0
        let seq = &sequences[s];
        let index_count = seq.index_count().max(1) as u32;
        // framesAreTime: sequence is a TSeries and there is only one time point.
        let fat = seq.is_time_series && size_t == 1;
        frames_are_time.push(fat);

        let (size_z, this_size_t) = if fat {
            (1u32, index_count)
        } else {
            (index_count, size_t as u32)
        };
        let image_count = size_z * size_c * this_size_t;

        let mut sm = meta_map.clone();
        sm.insert("cycle".into(), MetadataValue::Int(s as i64));
        sm.insert("indexCount".into(), MetadataValue::Int(index_count as i64));

        metas.push(ImageMetadata {
            size_x: width,
            size_y: height,
            size_z,
            size_c,
            size_t: this_size_t,
            pixel_type,
            bits_per_pixel: bits as u8,
            image_count,
            dimension_order: DimensionOrder::XYCZT,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian,
            resolution_count: 1,
            series_metadata: sm,
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        });
    }

    Ok(PrairieParse {
        metas,
        sequences,
        channels,
        frames_are_time,
        size_p,
        size_t,
    })
}

/// Whether two optional positions are equal (Java `equal(Length, Length)`):
/// both null is equal; one null is not equal.
fn pos_eq(a: Option<f64>, b: Option<f64>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

/// Scan the parsed sequences to separate time points from stage positions
/// (Java `computeSizeT`). Returns the number of time points `sizeT`; the number
/// of stage positions (series) is `sequenceCount / sizeT`.
fn compute_size_t(sequences: &[Sequence], sequence_count: usize) -> usize {
    // Guess at different possible "spans" for the rasterization.
    for size_p in 1..=sequence_count {
        if sequence_count % size_p != 0 {
            continue; // not a valid combo
        }
        let size_t = sequence_count / size_p;
        if positions_match(sequences, size_t, size_p) {
            return size_t;
        }
    }
    1
}

/// Verify that stage coordinates match for all (P, Z) across time (Java
/// `positionsMatch`). Rasterization order is XYCZpT, so
/// `sequence(t, p, sizeP) = sequences[sizeP * t + p]`.
fn positions_match(sequences: &[Sequence], size_t: usize, size_p: usize) -> bool {
    for p in 0..size_p {
        // sequences[sizeP * t + p] with t == 0
        let initial_sequence = &sequences[p];
        let index_min = initial_sequence.index_min();
        let index_count = initial_sequence.index_count();
        for z in 0..index_count {
            let index = z + index_min;
            let Some(initial_frame) = initial_sequence.frame(index) else {
                break;
            };
            let (xi, yi, zi) = (
                initial_frame.pos_x,
                initial_frame.pos_y,
                initial_frame.pos_z,
            );
            for t in 1..size_t {
                let seq = &sequences[size_p * t + p];
                let Some(frame) = seq.frame(index) else {
                    continue;
                };
                if !pos_eq(frame.pos_x, xi) || !pos_eq(frame.pos_y, yi) || !pos_eq(frame.pos_z, zi)
                {
                    return false;
                }
            }
        }
    }
    true
}

/// Compute (z, c, t) for a plane index under XYCZT order.
fn zct_xyczt(index: u32, size_z: u32, size_c: u32, _size_t: u32) -> (u32, u32, u32) {
    let size_c = size_c.max(1);
    let size_z = size_z.max(1);
    let c = index % size_c;
    let z = (index / size_c) % size_z;
    let t = index / (size_c * size_z);
    (z, c, t)
}

impl PrairieReader {
    /// Resolve the (file path, page) for a plane index in the current series,
    /// mirroring Java's sequence/frame/channel lookup.
    fn file_for_plane(&self, plane_index: u32) -> Option<(PathBuf, u32)> {
        let s = self.series;
        let meta = self.metas.get(s)?;
        let (z, c, t) = zct_xyczt(plane_index, meta.size_z, meta.size_c, meta.size_t);
        let frames_are_time = *self.frames_are_time.get(s).unwrap_or(&false);

        // sequence(t, s): actualT = framesAreTime ? 0 : t; index = sizeP*actualT + s.
        let actual_t = if frames_are_time { 0 } else { t as usize };
        let seq_idx = self.size_p * actual_t + s;
        let sequence = self.sequences.get(seq_idx)?;

        // frameIndex(seq, z, t, s) = (framesAreTime ? t : z) + indexMin.
        let frame_attr_index = (if frames_are_time { t } else { z }) as i32 + sequence.index_min();
        let frame = sequence.frame(frame_attr_index)?;

        let channel = *self.channels.get(c as usize).unwrap_or(&1);
        let file = frame.file_for_channel(channel)?;
        Some((file.filename.clone(), file.page))
    }
}

impl FormatReader for PrairieReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| {
                let e = e.to_ascii_lowercase();
                e == "xml" || e == "cfg" || e == "env" || e == "tif" || e == "tiff"
            })
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        let s = std::str::from_utf8(&header[..header.len().min(256)]).unwrap_or("");
        s.contains("<PVScan")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let xml = find_prairie_xml(path)
            .ok_or_else(|| BioFormatsError::Format("Prairie XML file not found".into()))?;

        // Verify the XML is actually a PrairieView document.
        let content_prefix = {
            let mut f = std::fs::File::open(&xml).map_err(BioFormatsError::Io)?;
            let mut buf = vec![0u8; 256];
            use std::io::Read;
            let n = f.read(&mut buf).map_err(BioFormatsError::Io)?;
            buf[..n].to_vec()
        };
        if !self.is_this_type_by_bytes(&content_prefix) {
            return Err(BioFormatsError::Format("Not a PrairieView XML file".into()));
        }

        let parsed = parse_prairie_xml(&xml)?;
        self.path = Some(xml);
        self.metas = parsed.metas;
        self.series = 0;
        self.sequences = parsed.sequences;
        self.channels = parsed.channels;
        self.frames_are_time = parsed.frames_are_time;
        self.size_p = parsed.size_p;
        let _ = parsed.size_t;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.metas.clear();
        self.series = 0;
        self.sequences.clear();
        self.channels.clear();
        self.frames_are_time.clear();
        self.size_p = 1;
        Ok(())
    }

    fn series_count(&self) -> usize {
        self.metas.len()
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.metas.is_empty() {
            return Err(BioFormatsError::NotInitialized);
        }
        if s >= self.metas.len() {
            Err(BioFormatsError::SeriesOutOfRange(s))
        } else {
            self.series = s;
            Ok(())
        }
    }
    fn series(&self) -> usize {
        self.series
    }

    fn metadata(&self) -> &ImageMetadata {
        self.metas
            .get(self.series)
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self
            .metas
            .get(self.series)
            .ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let (tiff_path, page) = self.file_for_plane(plane_index).ok_or_else(|| {
            BioFormatsError::Format(format!("Prairie: no file for plane {}", plane_index))
        })?;
        let mut tiff = TiffReader::new();
        tiff.set_id(&tiff_path)?;
        let inner = tiff.metadata().image_count.max(1);
        if page >= inner {
            return Err(BioFormatsError::Format(format!(
                "Prairie: TIFF page {page} out of range for {} ({} pages)",
                tiff_path.display(),
                inner
            )));
        }
        tiff.open_bytes(page)
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
        let meta = self.metas.get(self.series).unwrap();
        crate::formats::lei::crop_region(&full, meta, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self
            .metas
            .get(self.series)
            .ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}

// ── Leica TCS Reader ──────────────────────────────────────────────────────────

pub struct LeicaTcsReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    tiff_files: Vec<PathBuf>,
}

impl LeicaTcsReader {
    pub fn new() -> Self {
        LeicaTcsReader {
            path: None,
            meta: None,
            tiff_files: Vec::new(),
        }
    }
}

impl Default for LeicaTcsReader {
    fn default() -> Self {
        Self::new()
    }
}

fn parse_leica_xml(path: &Path) -> Result<(ImageMetadata, Vec<PathBuf>)> {
    let content = std::fs::read_to_string(path).map_err(BioFormatsError::Io)?;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));

    let mut width = 0u32;
    let mut height = 0u32;
    let mut size_z = 0u32;
    let mut size_t = 0u32;
    let mut num_channels = 0u32;
    let mut pixel_type = PixelType::Uint16;
    let mut is_rgb = false;
    let mut tiff_files: Vec<PathBuf> = Vec::new();

    for line in content.lines() {
        let line = line.trim();

        // <Image Width Height> as a coarse fallback for X/Y.
        if line.contains("<Image") {
            if let Some(w) = extract_attr(line, "Width").and_then(|v| v.parse::<u32>().ok()) {
                if width == 0 {
                    width = w;
                }
            }
            if let Some(h) = extract_attr(line, "Height").and_then(|v| v.parse::<u32>().ok()) {
                if height == 0 {
                    height = h;
                }
            }
        }

        // <ChannelDescription ...> increments the channel count (Java handler).
        if line.contains("<ChannelDescription") {
            num_channels += 1;
        }

        // <DimensionDescription DimID=.. NumberOfElements=.. BytesInc=..>
        // ports loci LeicaHandler's switch on DimID (1=X, 2=Y, 3=Z, 4=T) with
        // the XZ/XT scan Y-axis swap behaviour.
        if line.contains("<DimensionDescription") {
            let len = extract_attr(line, "NumberOfElements")
                .and_then(|v| v.parse::<u32>().ok())
                .ok_or_else(|| {
                    BioFormatsError::Format("Leica TCS: missing or invalid NumberOfElements".into())
                })?;
            if len == 0 {
                return Err(BioFormatsError::Format(
                    "Leica TCS: NumberOfElements must be non-zero".into(),
                ));
            }
            let id = extract_attr(line, "DimID")
                .and_then(|v| v.parse::<i32>().ok())
                .unwrap_or(0);
            let n_bytes = extract_attr(line, "BytesInc")
                .and_then(|v| v.parse::<i32>().ok())
                .unwrap_or(0);

            match id {
                1 => {
                    width = len;
                    let mut nb = n_bytes;
                    is_rgb = nb != 0 && nb % 3 == 0;
                    if is_rgb {
                        nb /= 3;
                    }
                    pixel_type = match nb {
                        1 => PixelType::Uint8,
                        2 => PixelType::Uint16,
                        4 => PixelType::Float32,
                        _ => {
                            return Err(BioFormatsError::Format(format!(
                                "Leica TCS: unsupported BytesInc {n_bytes}"
                            )))
                        }
                    };
                }
                2 => {
                    if height != 0 {
                        // Y already set; this dimension is really Z or T.
                        if size_z <= 1 {
                            size_z = len;
                        } else if size_t <= 1 {
                            size_t = len;
                        }
                    } else {
                        height = len;
                    }
                }
                3 => {
                    if height == 0 {
                        // XZ scan: swap Y and Z.
                        height = len;
                        size_z = 1;
                    } else {
                        size_z = len;
                    }
                }
                4 => {
                    if height == 0 {
                        // XT scan: swap Y and T.
                        height = len;
                        size_t = 1;
                    } else {
                        size_t = len;
                    }
                }
                _ => {}
            }
        }

        // Companion TIFF attachments.
        if line.contains("<Attachment") || line.contains("FileName") {
            if let Some(fname) =
                extract_attr_owned(line, "Name").or_else(|| extract_attr_owned(line, "FileName"))
            {
                if fname.to_ascii_lowercase().ends_with(".tif")
                    || fname.to_ascii_lowercase().ends_with(".tiff")
                {
                    if let Some(path) = confined_join(dir, &fname) {
                        tiff_files.push(path);
                    }
                }
            }
        }
    }

    if tiff_files.is_empty() {
        return Err(BioFormatsError::UnsupportedFormat(
            "Leica TCS XML does not reference any companion TIFF image files".into(),
        ));
    }

    let mut probe = TiffReader::new();
    if probe.set_id(&tiff_files[0]).is_ok() {
        let tm = probe.metadata();
        if width == 0 {
            width = tm.size_x;
        }
        if height == 0 {
            height = tm.size_y;
        }
        let _ = probe.close();
    } else if width == 0 || height == 0 {
        return Err(BioFormatsError::Format(format!(
            "Leica TCS: companion TIFF {} could not be read before metadata was initialized",
            tiff_files[0].display()
        )));
    }
    if width == 0 || height == 0 {
        return Err(BioFormatsError::Format(format!(
            "Leica TCS: invalid image dimensions {width}x{height}"
        )));
    }
    if num_channels == 0 {
        num_channels = 1;
    }
    if size_z == 0 {
        size_z = 1;
    }
    if size_t == 0 {
        size_t = 1;
    }

    // imageCount = sizeZ * sizeT * (rgb ? 1 : sizeC) (Java LeicaHandler).
    let plane_channels = if is_rgb { 1 } else { num_channels };
    let mut image_count = size_z * size_t * plane_channels;

    // If the dimension metadata is missing/inconsistent with the number of
    // companion TIFFs, fall back to treating each TIFF as one plane.
    let n_files = tiff_files.len() as u32;
    if image_count != n_files {
        // Distribute the available TIFFs across the parsed C/Z/T if possible;
        // otherwise treat each file as a Z plane (previous behaviour).
        if n_files % (num_channels.max(1)) == 0 && num_channels > 1 {
            size_z = n_files / num_channels;
            size_t = 1;
            image_count = n_files;
        } else if image_count == 0 {
            size_z = n_files;
            size_t = 1;
            num_channels = 1;
            image_count = n_files;
        }
    }

    let bits_per_pixel = match pixel_type {
        PixelType::Uint8 => 8,
        PixelType::Float32 => 32,
        _ => 16,
    };

    let meta = ImageMetadata {
        size_x: width,
        size_y: height,
        size_z,
        size_c: num_channels,
        size_t,
        pixel_type,
        bits_per_pixel,
        image_count,
        // Leica TCS rasterises channels fastest (XYCZT).
        dimension_order: DimensionOrder::XYCZT,
        is_rgb,
        is_interleaved: is_rgb,
        is_indexed: false,
        is_little_endian: true,
        resolution_count: 1,
        series_metadata: HashMap::new(),
        lookup_table: None,
        modulo_z: None,
        modulo_c: None,
        modulo_t: None,
    };

    Ok((meta, tiff_files))
}

impl FormatReader for LeicaTcsReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("xml"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        let s = std::str::from_utf8(&header[..header.len().min(256)]).unwrap_or("");
        s.contains("<LAS") || s.contains("<LEICA")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let content_prefix = {
            let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
            let mut buf = vec![0u8; 256];
            use std::io::Read;
            let n = f.read(&mut buf).map_err(BioFormatsError::Io)?;
            buf[..n].to_vec()
        };
        if !self.is_this_type_by_bytes(&content_prefix) {
            return Err(BioFormatsError::Format("Not a Leica TCS XML file".into()));
        }

        let (meta, tiff_files) = parse_leica_xml(path)?;
        self.path = Some(path.to_path_buf());
        self.meta = Some(meta);
        self.tiff_files = tiff_files;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.tiff_files.clear();
        Ok(())
    }

    fn series_count(&self) -> usize {
        usize::from(self.meta.is_some())
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.meta.is_none() || s != 0 {
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
        let file_count = self.tiff_files.len();
        let file_index = plane_index as usize % file_count;
        let page = plane_index as usize / file_count;
        let tiff_path = self.tiff_files[file_index].clone();
        let mut tiff = crate::tiff::TiffReader::new();
        tiff.set_id(&tiff_path)?;
        let inner = tiff.metadata().image_count.max(1) as usize;
        if page >= inner {
            return Err(BioFormatsError::Format(format!(
                "Leica TCS: TIFF page {page} out of range for {} ({} pages)",
                tiff_path.display(),
                inner
            )));
        }
        tiff.open_bytes(page as u32)
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
        crate::formats::lei::crop_region(&full, meta, x, y, w, h)
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
mod prairie_tests {
    use super::*;
    use crate::ImageWriter;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("bioformats_prairie_{nanos}_{name}"))
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = temp_path(name);
        std::fs::create_dir(&dir).unwrap();
        dir
    }

    /// Build a sequence with a single frame at index 0 carrying the given XYZ
    /// stage position.
    fn seq_at(x: f64, y: f64, z: f64) -> Sequence {
        Sequence {
            is_time_series: false,
            frames: vec![PFrame {
                index: 0,
                files: Vec::new(),
                pos_x: Some(x),
                pos_y: Some(y),
                pos_z: Some(z),
            }],
        }
    }

    #[test]
    fn prairie_all_same_position_is_single_series_many_timepoints() {
        // Three sequences sharing one stage position -> sizeT=3, seriesCount=1
        // (Java computeSizeT: sizeP=1 matches first).
        let seqs = vec![
            seq_at(1.0, 2.0, 3.0),
            seq_at(1.0, 2.0, 3.0),
            seq_at(1.0, 2.0, 3.0),
        ];
        let size_t = compute_size_t(&seqs, 3);
        assert_eq!(size_t, 3);
        assert_eq!(3 / size_t, 1); // seriesCount
    }

    #[test]
    fn prairie_distinct_positions_split_into_series() {
        // Rasterization XYCZpT with sizeP=2, sizeT=2: order is
        // seq[0]=p0/t0, seq[1]=p1/t0, seq[2]=p0/t1, seq[3]=p1/t1.
        // Positions must match per p across t: p0 -> A, p1 -> B.
        let a = (10.0, 20.0, 30.0);
        let b = (40.0, 50.0, 60.0);
        let seqs = vec![
            seq_at(a.0, a.1, a.2), // p0 t0
            seq_at(b.0, b.1, b.2), // p1 t0
            seq_at(a.0, a.1, a.2), // p0 t1
            seq_at(b.0, b.1, b.2), // p1 t1
        ];
        // sizeP=1 (sizeT=4) fails because positions differ; sizeP=2 (sizeT=2)
        // matches.
        let size_t = compute_size_t(&seqs, 4);
        assert_eq!(size_t, 2);
        assert_eq!(4 / size_t, 2); // two stage-position series
    }

    #[test]
    fn prairie_positions_match_detects_mismatch() {
        // With sizeP=2 but a position that changes over time, positionsMatch
        // must reject the grouping.
        let seqs = vec![
            seq_at(1.0, 1.0, 1.0), // p0 t0
            seq_at(2.0, 2.0, 2.0), // p1 t0
            seq_at(9.0, 9.0, 9.0), // p0 t1 -- differs from p0 t0
            seq_at(2.0, 2.0, 2.0), // p1 t1
        ];
        assert!(!positions_match(&seqs, 2, 2));
    }

    #[test]
    fn prairie_missing_channel_does_not_fall_back_to_first_file() {
        let frame = PFrame {
            index: 0,
            files: vec![PFile {
                channel: 1,
                filename: PathBuf::from("channel_1.tif"),
                page: 0,
            }],
            pos_x: None,
            pos_y: None,
            pos_z: None,
        };

        assert!(frame.file_for_channel(2).is_none());
    }

    #[test]
    fn prairie_companion_tiff_page_uses_exact_index() {
        let dir = temp_dir("exact_page");
        let tiff = dir.join("scan_001.tif");
        let meta = ImageMetadata {
            size_x: 1,
            size_y: 1,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type: PixelType::Uint8,
            bits_per_pixel: 8,
            image_count: 1,
            ..Default::default()
        };
        ImageWriter::save(&tiff, &meta, &[vec![9]]).unwrap();
        let xml = dir.join("scan.xml");
        std::fs::write(
            &xml,
            r#"<PVScan>
<PVStateValue key="pixelsPerLine" value="1"/>
<PVStateValue key="linesPerFrame" value="1"/>
<PVStateValue key="bitDepth" value="8"/>
<Sequence>
<Frame index="0">
<File filename="scan_001.tif" channel="1" page="2"/>
</Frame>
</Sequence>
</PVScan>"#,
        )
        .unwrap();

        let err = PrairieReader::new().set_id(&xml).unwrap_err();

        assert!(
            matches!(err, BioFormatsError::Format(ref message) if message.contains("TIFF page 1 out of range")),
            "unexpected error: {err:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
