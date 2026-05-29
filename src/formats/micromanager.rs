//! MicroManager format reader (open-source microscopy platform).
//!
//! MicroManager saves data as:
//!   - `metadata.txt` (or `*_metadata.txt`) — JSON with image dimensions
//!   - TIFF files (`MMStack_*.tif`, `img_*.tif`, etc.) — the actual pixel data
//!
//! Detection: file named `*_metadata.txt` or `metadata.txt`.
//! The JSON `Summary` block contains Width, Height, Channels, Slices, Frames,
//! PixelType, plus per-frame `FrameKey-<t>-<c>-<z>` blocks that map each plane
//! coordinate to a TIFF file.
//!
//! This follows the Java `MicromanagerReader`:
//!   - Each stage `Pos_*` sibling directory is a separate series.
//!   - Each plane index is mapped through a (z,c,t) -> filename map, falling
//!     back to a sorted TIFF list keyed by raster order.
//!   - Each TIFF may hold multiple pages; the inner page index is
//!     `plane % tiffReader.getImageCount()`.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::tiff::TiffReader;

// ── Minimal JSON key extractor ────────────────────────────────────────────────
/// Extract the integer value of a JSON key, e.g. `"Width": 512` or `"Width":512`.
fn json_int(json: &str, key: &str) -> Option<i64> {
    let pattern = format!("\"{}\"", key);
    let idx = json.find(&pattern)?;
    let rest = &json[idx + pattern.len()..];
    let rest = rest.trim_start();
    let rest = rest.strip_prefix(':').map(str::trim_start).unwrap_or(rest);
    let end = rest
        .find(|c: char| !c.is_ascii_digit() && c != '-')
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

fn json_str(json: &str, key: &str) -> Option<String> {
    let pattern = format!("\"{}\"", key);
    let idx = json.find(&pattern)?;
    let rest = &json[idx + pattern.len()..];
    let rest = rest.trim_start();
    let rest = rest.strip_prefix(':').map(str::trim_start).unwrap_or(rest);
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Extract a JSON array of strings (or numbers), e.g. `"ChNames": ["a","b"]`.
/// Returns the trimmed, unquoted elements. Mirrors Java `value.split(",")`.
fn json_str_array(json: &str, key: &str) -> Option<Vec<String>> {
    let pattern = format!("\"{}\"", key);
    let idx = json.find(&pattern)?;
    let rest = &json[idx + pattern.len()..];
    let rest = rest.trim_start();
    let rest = rest.strip_prefix(':').map(str::trim_start).unwrap_or(rest);
    let rest = rest.strip_prefix('[')?;
    let end = rest.find(']')?;
    let body = &rest[..end];
    let items: Vec<String> = body
        .split(',')
        .map(|s| s.trim().trim_matches('"').trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if items.is_empty() {
        None
    } else {
        Some(items)
    }
}

fn json_float(json: &str, key: &str) -> Option<f64> {
    let pattern = format!("\"{}\"", key);
    let idx = json.find(&pattern)?;
    let rest = &json[idx + pattern.len()..];
    let rest = rest.trim_start();
    let rest = rest.strip_prefix(':').map(str::trim_start).unwrap_or(rest);
    let rest = rest.trim_start_matches('"');
    let end = rest
        .find(|c: char| {
            !c.is_ascii_digit() && c != '-' && c != '.' && c != 'e' && c != 'E' && c != '+'
        })
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

fn positive_u32_from_json(json: &str, key: &str) -> Result<u32> {
    let value = json_int(json, key)
        .ok_or_else(|| BioFormatsError::Format(format!("MicroManager: missing {key}")))?;
    u32::try_from(value)
        .ok()
        .filter(|&v| v > 0)
        .ok_or_else(|| BioFormatsError::Format(format!("MicroManager: invalid {key} {value}")))
}

fn optional_positive_u32_from_json(json: &str, key: &str, default: u32) -> Result<u32> {
    match json_int(json, key) {
        Some(value) => u32::try_from(value)
            .ok()
            .filter(|&v| v > 0)
            .ok_or_else(|| BioFormatsError::Format(format!("MicroManager: invalid {key} {value}"))),
        None => Ok(default),
    }
}

fn pixel_type_from_str(s: &str) -> Result<PixelType> {
    match s.to_uppercase().as_str() {
        "GRAY8" | "RGB8" => Ok(PixelType::Uint8),
        "GRAY16" | "RGB16" => Ok(PixelType::Uint16),
        "GRAY32" | "RGB32" => Ok(PixelType::Float32),
        other => Err(BioFormatsError::UnsupportedFormat(format!(
            "MicroManager: unsupported PixelType {other}"
        ))),
    }
}

/// A (z, c, t) coordinate key used in the per-plane file map.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct Index {
    z: u32,
    c: u32,
    t: u32,
}

/// Per-position (per-series) parsed state, mirroring Java's `Position`.
struct Position {
    #[allow(dead_code)]
    metadata_file: PathBuf,
    meta: ImageMetadata,
    /// Map (z,c,t) -> absolute TIFF path.
    file_name_map: HashMap<Index, PathBuf>,
    /// Sorted fallback list of TIFF files (raster order).
    tiffs: Vec<PathBuf>,
}

impl Position {
    /// Convert a 1D raster plane index to (z, c, t) for the given dimension order.
    fn zct_coords(order: DimensionOrder, z: u32, c: u32, t: u32, no: u32) -> (u32, u32, u32) {
        let z = z.max(1);
        let c = c.max(1);
        let t = t.max(1);
        let dims: &[(char, u32)] = match order {
            DimensionOrder::XYCTZ => &[('C', c), ('T', t), ('Z', z)],
            DimensionOrder::XYCZT => &[('C', c), ('Z', z), ('T', t)],
            DimensionOrder::XYTCZ => &[('T', t), ('C', c), ('Z', z)],
            DimensionOrder::XYTZC => &[('T', t), ('Z', z), ('C', c)],
            DimensionOrder::XYZCT => &[('Z', z), ('C', c), ('T', t)],
            DimensionOrder::XYZTC => &[('Z', z), ('T', t), ('C', c)],
        };
        let mut remaining = no;
        let (mut zz, mut cc, mut tt) = (0u32, 0u32, 0u32);
        for (dim, len) in dims {
            let len = (*len).max(1);
            let value = remaining % len;
            remaining /= len;
            match dim {
                'Z' => zz = value,
                'C' => cc = value,
                'T' => tt = value,
                _ => {}
            }
        }
        (zz, cc, tt)
    }

    /// Resolve the TIFF file for a given plane index, mirroring Java `getFile`.
    fn file_for_plane(&self, no: u32) -> Option<PathBuf> {
        let m = &self.meta;
        let (z, c, t) = Self::zct_coords(m.dimension_order, m.size_z, m.size_c, m.size_t, no);
        let key = Index { z, c, t };
        if let Some(p) = self.file_name_map.get(&key) {
            return Some(p.clone());
        }
        if self.file_name_map.is_empty() {
            return self.tiffs.get(no as usize).cloned();
        }
        // Map exists but no entry for this coordinate: fall back to raster list.
        self.tiffs.get(no as usize).cloned()
    }
}

/// Parse a metadata.txt JSON file for a single position.
fn parse_position(meta_path: &Path) -> Result<Position> {
    let f = File::open(meta_path).map_err(BioFormatsError::Io)?;
    let mut json = String::new();
    BufReader::new(f)
        .read_to_string(&mut json)
        .map_err(BioFormatsError::Io)?;

    // Summary block: dimensions
    let summary_start = json.find("\"Summary\"").unwrap_or(0);
    let summary = &json[summary_start..];

    let width = positive_u32_from_json(summary, "Width")?;
    let height = positive_u32_from_json(summary, "Height")?;
    let channels = optional_positive_u32_from_json(summary, "Channels", 1)?;
    let slices = optional_positive_u32_from_json(summary, "Slices", 1)?;
    let frames = optional_positive_u32_from_json(summary, "Frames", 1)?;
    let pixel_type_str = json_str(summary, "PixelType").unwrap_or_else(|| "GRAY16".into());
    let mut pixel_type = pixel_type_from_str(&pixel_type_str)?;
    let mut bits = match json_int(summary, "BitDepth") {
        Some(value) => u8::try_from(value).ok().filter(|&v| v > 0).ok_or_else(|| {
            BioFormatsError::Format(format!("MicroManager: invalid BitDepth {value}"))
        })?,
        None => pixel_type.bytes_per_sample() as u8 * 8,
    };
    let is_rgb_summary = pixel_type_str.starts_with("RGB");

    // Dimension order from "SlicesFirst": false -> XYCZT, else XYZCT (Java default).
    let dimension_order = match json_str(&json, "SlicesFirst")
        .or_else(|| json_int(&json, "SlicesFirst").map(|v| v.to_string()))
    {
        Some(ref v) if v.eq_ignore_ascii_case("false") || v == "0" => DimensionOrder::XYCZT,
        _ => DimensionOrder::XYZCT,
    };

    let dir = meta_path.parent().unwrap_or_else(|| Path::new("."));

    // Build the per-plane file name map from "FrameKey-<t>-<c>-<z>" blocks.
    let mut file_name_map: HashMap<Index, PathBuf> = HashMap::new();
    parse_frame_keys(&json, dir, &mut file_name_map);

    // Fallback: sorted list of all TIFF files in the directory.
    let mut tiffs: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    p.extension()
                        .and_then(|e| e.to_str())
                        .map(|e| e.eq_ignore_ascii_case("tif") || e.eq_ignore_ascii_case("tiff"))
                        .unwrap_or(false)
                })
                .collect()
        })
        .unwrap_or_default();
    tiffs.sort();

    // Derive endianness/pixel type from the first available TIFF (Java reads the
    // first IFD: littleEndian and pixelType come from the IFD, not the JSON).
    let mut is_little_endian = true;
    let probe = file_name_map
        .values()
        .next()
        .cloned()
        .or_else(|| tiffs.first().cloned());
    if let Some(probe_path) = probe {
        let mut r = TiffReader::new();
        if r.set_id(&probe_path).is_ok() {
            let tm = r.metadata();
            is_little_endian = tm.is_little_endian;
            pixel_type = tm.pixel_type;
            if tm.bits_per_pixel > 0 {
                bits = tm.bits_per_pixel;
            }
            let _ = r.close();
        }
    }

    let image_count = channels
        .checked_mul(slices)
        .and_then(|v| v.checked_mul(frames))
        .ok_or_else(|| BioFormatsError::Format("MicroManager: image count overflow".into()))?;

    let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
    meta_map.insert(
        "format".into(),
        MetadataValue::String("MicroManager".into()),
    );
    meta_map.insert(
        "pixel_type_str".into(),
        MetadataValue::String(pixel_type_str),
    );

    // Richer metadata, mirroring Java MicromanagerReader.parsePosition:
    //   channel names (ChNames), channel colors (ChColors), pixel calibration
    //   (PixelSize_um / z-step_um), comment, time, position name.
    if let Some(names) = json_str_array(summary, "ChNames") {
        for (q, name) in names.iter().enumerate() {
            meta_map.insert(
                format!("channel_name[{q}]"),
                MetadataValue::String(name.clone()),
            );
        }
    }
    if let Some(colors) = json_str_array(summary, "ChColors") {
        for (q, color) in colors.iter().enumerate() {
            meta_map.insert(
                format!("channel_color[{q}]"),
                MetadataValue::String(color.clone()),
            );
        }
    }
    if let Some(px) = json_float(summary, "PixelSize_um") {
        if px > 0.0 {
            meta_map.insert("physicalSizeX".into(), MetadataValue::Float(px));
            meta_map.insert("physicalSizeY".into(), MetadataValue::Float(px));
        }
    }
    if let Some(step) = json_float(summary, "z-step_um") {
        if step > 0.0 {
            meta_map.insert("physicalSizeZ".into(), MetadataValue::Float(step));
        }
    }
    if let Some(comment) = json_str(summary, "Comment") {
        meta_map.insert("comment".into(), MetadataValue::String(comment));
    }
    if let Some(time) = json_str(summary, "Time") {
        meta_map.insert("time".into(), MetadataValue::String(time));
    }
    // PositionName appears in per-frame blocks; scan the whole document.
    if let Some(name) = json_str(&json, "PositionName") {
        if name != "null" && !name.is_empty() {
            meta_map.insert("image_name".into(), MetadataValue::String(name));
        }
    }

    let meta = ImageMetadata {
        size_x: width,
        size_y: height,
        size_z: slices,
        size_c: channels,
        size_t: frames,
        pixel_type,
        bits_per_pixel: bits,
        image_count,
        dimension_order,
        is_rgb: is_rgb_summary,
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

    Ok(Position {
        metadata_file: meta_path.to_path_buf(),
        meta,
        file_name_map,
        tiffs,
    })
}

/// Scan the JSON for `"FrameKey-<t>-<c>-<z>"` blocks and extract the `FileName`
/// for each, populating the (z,c,t) -> path map.
fn parse_frame_keys(json: &str, dir: &Path, map: &mut HashMap<Index, PathBuf>) {
    let mut search = 0;
    while let Some(rel) = json[search..].find("\"FrameKey-") {
        let abs = search + rel + 1; // position after the opening quote
        let rest = &json[abs..];
        // rest starts with FrameKey-<t>-<c>-<z>"
        let end_quote = match rest.find('"') {
            Some(e) => e,
            None => break,
        };
        let key = &rest[..end_quote]; // FrameKey-t-c-z
        search = abs + end_quote;

        let parts: Vec<&str> = key.trim_start_matches("FrameKey-").split('-').collect();
        if parts.len() < 3 {
            continue;
        }
        let t: u32 = match parts[0].parse() {
            Ok(v) => v,
            Err(_) => continue,
        };
        let c: u32 = match parts[1].parse() {
            Ok(v) => v,
            Err(_) => continue,
        };
        let z: u32 = match parts[2].parse() {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Search the block following this key for a "FileName" entry, stopping at
        // the next FrameKey (or end).
        let block_start = search;
        let block_end = json[block_start..]
            .find("\"FrameKey-")
            .map(|p| block_start + p)
            .unwrap_or(json.len());
        let block = &json[block_start..block_end];
        if let Some(fname) = json_str(block, "FileName") {
            let real = dir.join(&fname);
            if real.exists() {
                map.insert(Index { z, c, t }, real);
            } else {
                map.insert(Index { z, c, t }, dir.join(fname));
            }
        }
    }
}

/// Discover all sibling `Pos_*` position directories (multi-position series).
/// Returns the list of metadata.txt paths in sorted order, or just the single
/// supplied file if this is not a multi-position dataset.
fn discover_positions(meta_path: &Path) -> Vec<PathBuf> {
    let parent = match meta_path.parent() {
        Some(p) => p,
        None => return vec![meta_path.to_path_buf()],
    };
    let parent_name = parent
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();

    if parent_name.contains("Pos_") {
        // Sibling Pos_* directories each contain their own metadata.txt.
        let grandparent = match parent.parent() {
            Some(gp) => gp,
            None => return vec![meta_path.to_path_buf()],
        };
        let mut dirs: Vec<PathBuf> = std::fs::read_dir(grandparent)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .filter(|p| {
                        p.is_dir()
                            && p.file_name()
                                .and_then(|n| n.to_str())
                                .map(|n| n.contains("Pos_"))
                                .unwrap_or(false)
                    })
                    .map(|p| p.join("metadata.txt"))
                    .filter(|p| p.exists())
                    .collect()
            })
            .unwrap_or_default();
        dirs.sort();
        if dirs.is_empty() {
            return vec![meta_path.to_path_buf()];
        }
        return dirs;
    }

    vec![meta_path.to_path_buf()]
}

// ── Reader ────────────────────────────────────────────────────────────────────

pub struct MicromanagerReader {
    meta_path: Option<PathBuf>,
    positions: Vec<Position>,
    series: usize,
}

impl MicromanagerReader {
    pub fn new() -> Self {
        MicromanagerReader {
            meta_path: None,
            positions: Vec::new(),
            series: 0,
        }
    }
}

impl Default for MicromanagerReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for MicromanagerReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.to_ascii_lowercase())
            .unwrap_or_default();
        name == "metadata.txt" || name.ends_with("_metadata.txt") || name == "metadata.json"
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let position_files = discover_positions(path);
        let mut positions = Vec::with_capacity(position_files.len());
        for pf in position_files {
            positions.push(parse_position(&pf)?);
        }
        if positions.is_empty() {
            return Err(BioFormatsError::Format(
                "MicroManager: no positions found".into(),
            ));
        }
        self.positions = positions;
        self.series = 0;
        self.meta_path = Some(path.to_path_buf());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.meta_path = None;
        self.positions.clear();
        self.series = 0;
        Ok(())
    }

    fn series_count(&self) -> usize {
        self.positions.len().max(1)
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if s >= self.positions.len() {
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
        self.positions
            .get(self.series)
            .map(|position| &position.meta)
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let pos = self
            .positions
            .get(self.series)
            .ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= pos.meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let file = pos.file_for_plane(plane_index).ok_or_else(|| {
            BioFormatsError::Format(format!(
                "MicroManager: no TIFF file for plane {}",
                plane_index
            ))
        })?;
        let mut r = TiffReader::new();
        r.set_id(&file)?;
        let inner_count = r.metadata().image_count.max(1);
        let inner_idx = plane_index % inner_count;
        r.open_bytes(inner_idx)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let pos = self
            .positions
            .get(self.series)
            .ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= pos.meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let file = pos.file_for_plane(plane_index).ok_or_else(|| {
            BioFormatsError::Format(format!(
                "MicroManager: no TIFF file for plane {}",
                plane_index
            ))
        })?;
        let mut r = TiffReader::new();
        r.set_id(&file)?;
        let inner_count = r.metadata().image_count.max(1);
        let inner_idx = plane_index % inner_count;
        r.open_bytes_region(inner_idx, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self
            .positions
            .get(self.series)
            .map(|p| &p.meta)
            .ok_or(BioFormatsError::NotInitialized)?;
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = ((meta.size_x - tw) / 2, (meta.size_y - th) / 2);
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}
