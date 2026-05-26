//! Prairie Technologies PrairieView and Leica TCS XML+TIFF series readers.
//!
//! Both formats use an XML metadata file that references companion TIFF files.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
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
/// active channel.
struct PFrame {
    files: Vec<PFile>,
}

impl PFrame {
    fn file_for_channel(&self, channel: i32) -> Option<&PFile> {
        self.files
            .iter()
            .find(|f| f.channel == channel)
            .or_else(|| self.files.first())
    }
}

/// A `<Sequence>` element: a cycle (stage position or time point) containing an
/// ordered list of frames.
struct Sequence {
    is_time_series: bool,
    frames: Vec<PFrame>,
}

pub struct PrairieReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    sequences: Vec<Sequence>,
    /// Sorted active channel indices.
    channels: Vec<i32>,
    /// Whether frames act as time points rather than focal planes.
    frames_are_time: bool,
}

impl PrairieReader {
    pub fn new() -> Self {
        PrairieReader {
            path: None,
            meta: None,
            sequences: Vec::new(),
            channels: Vec::new(),
            frames_are_time: false,
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
        rd.filter_map(|e| e.ok())
            .map(|e| e.path())
            .find(|p| {
                p.extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.eq_ignore_ascii_case("xml"))
                    .unwrap_or(false)
            })
    })
}

/// Parse the PrairieView XML into sequences/frames/files plus core metadata.
fn parse_prairie_xml(path: &Path) -> Result<(ImageMetadata, Vec<Sequence>, Vec<i32>, bool)> {
    let content = std::fs::read_to_string(path).map_err(BioFormatsError::Io)?;
    let dir = path.parent().unwrap_or_else(|| Path::new(".")).to_path_buf();

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
            if let (Some(idx), Some(val)) = (extract_attr(line, "index"), extract_attr(line, "value"))
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
                is_time_series: ty.to_ascii_lowercase().contains("tseries"),
                frames: Vec::new(),
            });
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
            cur_frame = Some(PFrame { files: Vec::new() });
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
                let pfile = PFile {
                    channel,
                    filename: dir.join(fname),
                    page,
                };
                if cur_frame.is_none() {
                    if cur_seq.is_none() {
                        cur_seq = Some(Sequence {
                            is_time_series: false,
                            frames: Vec::new(),
                        });
                    }
                    cur_frame = Some(PFrame { files: Vec::new() });
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

    let has_files = sequences.iter().any(|s| s.frames.iter().any(|f| !f.files.is_empty()));
    if !has_files {
        return Err(BioFormatsError::UnsupportedFormat(
            "PrairieView XML does not reference any companion TIFF image files".into(),
        ));
    }

    let channels: Vec<i32> = active_channels.into_iter().collect();
    let size_c = channels.len().max(1) as u32;

    // sequenceCount = sizeT * seriesCount; for a single series we treat all
    // sequences as time points. indexCount = frames in the first sequence.
    let sequence_count = sequences.len().max(1) as u32;
    let index_count = sequences
        .first()
        .map(|s| s.frames.len())
        .unwrap_or(0)
        .max(1) as u32;

    // framesAreTime: single TSeries sequence -> frames are time points.
    let frames_are_time = sequence_count == 1
        && sequences.first().map(|s| s.is_time_series).unwrap_or(false);

    let (size_z, size_t) = if frames_are_time {
        (1u32, index_count)
    } else {
        (index_count, sequence_count)
    };

    // Derive pixel type / dimensions from the first available TIFF.
    let mut pixel_type = match bits {
        8 => PixelType::Uint8,
        16 => PixelType::Uint16,
        32 => PixelType::Float32,
        _ => PixelType::Uint16,
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
        if r.set_id(&ff).is_ok() {
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
    }
    if width == 0 {
        width = 512;
    }
    if height == 0 {
        height = 512;
    }
    if bits == 0 {
        bits = 16;
    }

    let image_count = size_z * size_c * size_t;

    // Original metadata Java populates in populateOriginalMetadata.
    meta_map.insert(
        "sequenceCount".into(),
        MetadataValue::Int(sequence_count as i64),
    );
    meta_map.insert(
        "activeChannelCount".into(),
        MetadataValue::Int(size_c as i64),
    );

    let meta = ImageMetadata {
        size_x: width,
        size_y: height,
        size_z,
        size_c,
        size_t,
        pixel_type,
        bits_per_pixel: bits as u8,
        image_count,
        dimension_order: DimensionOrder::XYCZT,
        is_rgb: false,
        is_interleaved: false,
        is_indexed: false,
        is_little_endian,
        resolution_count: 1,
        series_metadata: meta_map,
        lookup_table: None,
        modulo_z: None,
        modulo_c: None,
        modulo_t: None,
    };

    Ok((meta, sequences, channels, frames_are_time))
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
    /// Resolve the (file path, page) for a plane index, mirroring Java's
    /// sequence/frame/channel lookup.
    fn file_for_plane(&self, plane_index: u32) -> Option<(PathBuf, u32)> {
        let meta = self.meta.as_ref()?;
        let (z, c, t) = zct_xyczt(plane_index, meta.size_z, meta.size_c, meta.size_t);

        // sequence = time point t (or 0 if frames are time points)
        let seq_idx = if self.frames_are_time { 0 } else { t as usize };
        let sequence = self.sequences.get(seq_idx)?;

        let frame_idx = if self.frames_are_time { t } else { z } as usize;
        let frame = sequence.frames.get(frame_idx)?;

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

        let (meta, sequences, channels, frames_are_time) = parse_prairie_xml(&xml)?;
        self.path = Some(xml);
        self.meta = Some(meta);
        self.sequences = sequences;
        self.channels = channels;
        self.frames_are_time = frames_are_time;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.sequences.clear();
        self.channels.clear();
        self.frames_are_time = false;
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
        self.meta.as_ref().expect("set_id not called")
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let (tiff_path, page) = self.file_for_plane(plane_index).ok_or_else(|| {
            BioFormatsError::Format(format!(
                "Prairie: no file for plane {}",
                plane_index
            ))
        })?;
        let mut tiff = TiffReader::new();
        tiff.set_id(&tiff_path)?;
        let inner = tiff.metadata().image_count.max(1);
        tiff.open_bytes(page % inner)
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
                .unwrap_or(1)
                .max(1);
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
                        _ => pixel_type,
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
                    tiff_files.push(dir.join(&fname));
                }
            }
        }
    }

    if tiff_files.is_empty() {
        return Err(BioFormatsError::UnsupportedFormat(
            "Leica TCS XML does not reference any companion TIFF image files".into(),
        ));
    }

    if width == 0 {
        width = 512;
    }
    if height == 0 {
        height = 512;
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
        self.meta.as_ref().expect("set_id not called")
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let tiff_path = self.tiff_files[plane_index as usize % self.tiff_files.len()].clone();
        let mut tiff = crate::tiff::TiffReader::new();
        tiff.set_id(&tiff_path)?;
        tiff.open_bytes(0)
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
