//! OME-Zarr / OME-NGFF reader.
//!
//! Pure-Rust translation of Java Bio-Formats `loci.formats.in.ZarrReader`,
//! using the native [`zarrs`] crate instead of the JZarr backend.
//!
//! A `.ome.zarr` (or `.zarr`) dataset is a Zarr group hierarchy whose group
//! metadata (`.zattrs` for Zarr v2, `zarr.json` for v3) carries OME-NGFF
//! attributes:
//!
//! - `multiscales`: a list of images. Each has `axes` (t/c/z/y/x with
//!   name/type/unit) and `datasets` (one Zarr array per resolution level, each
//!   with a `coordinateTransformations` scale giving physical sizes / per-level
//!   downsampling).
//! - `omero`: rendering metadata (per-channel name, colour, window) → OME
//!   channels.
//! - `plate` / `well`: optional HCS layout.
//!
//! Mapping to our reader model (mirroring `ZarrReader.java`):
//! - Each *multiscales image* (a group containing a `multiscales` attribute)
//!   becomes one **series**.
//! - Each *dataset* within a multiscales image becomes one **resolution level**
//!   of that series (like the TIFF pyramid in `svs.rs`).
//! - Shapes are normalised to 5D `t,c,z,y,x`; `image_count = Z*C*T`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;
use zarrs::array::{Array, ArraySubset};
use zarrs::filesystem::FilesystemStore;

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata};
use crate::common::ome_metadata::{
    OmeChannel, OmeImage, OmeMetadata, OmePlate, OmeWell, OmeWellSample,
};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;

/// One resolution level (a single Zarr array) of a multiscales image.
#[derive(Debug, Clone)]
struct ZarrLevel {
    /// Array key relative to the store root, e.g. `0` or `A/1/0/0`.
    path: String,
    size_x: u32,
    size_y: u32,
    size_z: u32,
    size_c: u32,
    size_t: u32,
    /// Number of dimensions in the underlying array (2..=5).
    ndim: usize,
    /// For each logical axis (t,c,z,y,x) the index into the array shape, or
    /// `None` if that axis is not present in this array.
    axis_index: [Option<usize>; 5],
    pixel_type: PixelType,
    dimension_order: DimensionOrder,
}

/// One multiscales image == one series.
#[derive(Debug, Clone)]
struct ZarrSeries {
    /// Group key relative to the store root (`""` for the root group).
    group_path: String,
    name: String,
    levels: Vec<ZarrLevel>,
    physical_size_x: Option<f64>,
    physical_size_y: Option<f64>,
    physical_size_z: Option<f64>,
    time_increment: Option<f64>,
    channels: Vec<OmeChannel>,
}

pub struct OmeZarrReader {
    root: Option<PathBuf>,
    store: Option<Arc<FilesystemStore>>,
    series: Vec<ZarrSeries>,
    plate: Option<OmePlate>,
    current_series: usize,
    current_resolution: usize,
    metadata: ImageMetadata,
    /// Lazily-opened array for the current (series, resolution).
    open_array: Option<(String, Array<FilesystemStore>)>,
}

impl Default for OmeZarrReader {
    fn default() -> Self {
        Self::new()
    }
}

impl OmeZarrReader {
    pub fn new() -> Self {
        OmeZarrReader {
            root: None,
            store: None,
            series: Vec::new(),
            plate: None,
            current_series: 0,
            current_resolution: 0,
            metadata: ImageMetadata::default(),
            open_array: None,
        }
    }

    fn current_level(&self) -> Result<&ZarrLevel> {
        self.series
            .get(self.current_series)
            .and_then(|s| s.levels.get(self.current_resolution))
            .ok_or(BioFormatsError::NotInitialized)
    }

    /// Recompute `self.metadata` from the current series + resolution.
    fn refresh_metadata(&mut self) {
        let Some(series) = self.series.get(self.current_series) else {
            return;
        };
        let Some(level) = series.levels.get(self.current_resolution) else {
            return;
        };
        let bps = level.pixel_type.bytes_per_sample() as u8;
        self.metadata = ImageMetadata {
            size_x: level.size_x,
            size_y: level.size_y,
            size_z: level.size_z,
            size_c: level.size_c,
            size_t: level.size_t,
            pixel_type: level.pixel_type,
            bits_per_pixel: bps * 8,
            image_count: level.size_z * level.size_c * level.size_t,
            dimension_order: level.dimension_order,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: true,
            resolution_count: series.levels.len() as u32,
            ..Default::default()
        };
    }

    /// Open (and cache) the Zarr array for the current series + resolution.
    fn ensure_open_array(&mut self) -> Result<()> {
        let path = self.current_level()?.path.clone();
        if self
            .open_array
            .as_ref()
            .map(|(p, _)| p == &path)
            .unwrap_or(false)
        {
            return Ok(());
        }
        let store = self.store.clone().ok_or(BioFormatsError::NotInitialized)?;
        let key = format!("/{}", path.trim_start_matches('/'));
        let array = Array::open(store, &key)
            .map_err(|e| BioFormatsError::Format(format!("zarr array open {key}: {e}")))?;
        self.open_array = Some((path, array));
        Ok(())
    }
}

/// Quick path-based test used by the registry before `peek_header` (which fails
/// on directories). Matches Java `ZarrReader.isThisType`: any path containing
/// `.zarr`, or a directory carrying a Zarr group marker.
pub fn is_zarr_path(path: &Path) -> bool {
    let lower = path.to_string_lossy().to_ascii_lowercase();
    if lower.contains(".zarr") {
        return path.exists();
    }
    if path.is_dir() {
        return path.join(".zgroup").exists()
            || path.join("zarr.json").exists()
            || path.join(".zattrs").exists();
    }
    false
}

/// Resolve the Zarr root directory from an arbitrary path inside or naming the
/// dataset. Mirrors Java's `zarrPath.substring(0, indexOf(".zarr") + 5)`.
fn resolve_root(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if let Some(idx) = s.to_ascii_lowercase().find(".zarr") {
        let end = idx + ".zarr".len();
        return PathBuf::from(&s[..end]);
    }
    path.to_path_buf()
}

/// Read the OME-NGFF attributes of a Zarr group directory, handling both v2
/// (`.zgroup` + `.zattrs`) and v3 (`zarr.json`). Returns `None` if `dir` is not
/// a group.
fn read_group_attributes(dir: &Path) -> Option<serde_json::Map<String, Value>> {
    let zarr_json = dir.join("zarr.json");
    if zarr_json.is_file() {
        let text = std::fs::read_to_string(&zarr_json).ok()?;
        let json: Value = serde_json::from_str(&text).ok()?;
        if json.get("node_type").and_then(Value::as_str) != Some("group") {
            return None;
        }
        return Some(
            json.get("attributes")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default(),
        );
    }
    if dir.join(".zgroup").is_file() {
        let attrs = dir.join(".zattrs");
        if attrs.is_file() {
            let text = std::fs::read_to_string(&attrs).ok()?;
            let json: Value = serde_json::from_str(&text).ok()?;
            return Some(json.as_object().cloned().unwrap_or_default());
        }
        return Some(serde_json::Map::new());
    }
    None
}

/// Recursively collect group directories that carry a `multiscales` attribute,
/// keyed by their path relative to `root`. Skips `labels` subtrees (matching
/// Java's `includeLabels = false` default).
fn collect_multiscales_groups(
    root: &Path,
    dir: &Path,
    rel: &str,
    out: &mut BTreeMap<String, serde_json::Map<String, Value>>,
) {
    if let Some(attrs) = read_group_attributes(dir) {
        if attrs.contains_key("multiscales") {
            out.insert(rel.to_string(), attrs);
            // A multiscales group's children are its arrays, not nested images.
            return;
        }
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if !p.is_dir() {
            continue;
        }
        let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with('.') || name.eq_ignore_ascii_case("labels") || name == "OME" {
            continue;
        }
        let child_rel = if rel.is_empty() {
            name.to_string()
        } else {
            format!("{rel}/{name}")
        };
        collect_multiscales_groups(root, &p, &child_rel, out);
    }
}

/// Natural-order comparator over `/`-separated keys, mirroring Java
/// `ZarrReader.keyComparator` (numeric components sort numerically, A/1/2 before
/// A/1/10).
fn natural_key(key: &str) -> Vec<(bool, u64, String)> {
    key.split('/')
        .map(|part| match part.parse::<u64>() {
            Ok(n) => (true, n, String::new()),
            Err(_) => (false, 0, part.to_string()),
        })
        .collect()
}

/// Map one dtype token to a `PixelType`, accepting both the Zarr v3 canonical
/// name (`uint16`) and the NumPy v2 form (`<u2`, `|i1`, `>f4`, …).
fn pixel_type_from_token(token: &str) -> Option<PixelType> {
    let t = token.trim().to_ascii_lowercase();
    // Canonical Zarr v3 names.
    match t.as_str() {
        "int8" => return Some(PixelType::Int8),
        "uint8" => return Some(PixelType::Uint8),
        "int16" => return Some(PixelType::Int16),
        "uint16" => return Some(PixelType::Uint16),
        "int32" => return Some(PixelType::Int32),
        "uint32" => return Some(PixelType::Uint32),
        "float32" => return Some(PixelType::Float32),
        "float64" => return Some(PixelType::Float64),
        "bool" => return Some(PixelType::Uint8),
        _ => {}
    }
    // NumPy v2 form: optional endian char (< > | =) then kind + byte width.
    let core = t.trim_start_matches(['<', '>', '|', '=']);
    match core {
        "u1" | "b1" => Some(PixelType::Uint8),
        "i1" => Some(PixelType::Int8),
        "u2" => Some(PixelType::Uint16),
        "i2" => Some(PixelType::Int16),
        "u4" => Some(PixelType::Uint32),
        "i4" => Some(PixelType::Int32),
        "f4" => Some(PixelType::Float32),
        "f8" => Some(PixelType::Float64),
        _ => None,
    }
}

/// Map a Zarr dtype name (zarrs `DataType` display string) to our `PixelType`.
/// The display may be `"uint16"`, `"<u2"`, or `"uint16 / <u2"` (v3 / v2 differ).
fn pixel_type_from_dtype(name: &str) -> Result<PixelType> {
    for token in name.split('/') {
        if let Some(pt) = pixel_type_from_token(token) {
            return Ok(pt);
        }
    }
    Err(BioFormatsError::UnsupportedFormat(format!(
        "OME-Zarr: unsupported array dtype {name:?}"
    )))
}

/// Extract the lowercased axis names from a multiscales `axes` value. Supports
/// both the v0.4 object form (`{name,type,unit}`) and the legacy string form.
fn axis_names(axes: &Value) -> Vec<String> {
    let mut names = Vec::new();
    if let Some(arr) = axes.as_array() {
        for a in arr {
            if let Some(s) = a.as_str() {
                names.push(s.to_ascii_lowercase());
            } else if let Some(name) = a.get("name").and_then(Value::as_str) {
                names.push(name.to_ascii_lowercase());
            }
        }
    }
    names
}

/// Compute the dimension order string (e.g. `XYZCT`) from axis names, mirroring
/// Java: reverse the axis list and uppercase. Falls back to `XYZCT`.
fn dimension_order_from_axes(axis_names: &[String]) -> DimensionOrder {
    let mut order = String::new();
    for name in axis_names.iter().rev() {
        if let Some(c) = name.chars().next() {
            order.push(c.to_ascii_uppercase());
        }
    }
    // Keep only the Z/C/T tail order; X and Y are always the last two axes.
    let tail: String = order
        .chars()
        .filter(|c| matches!(c, 'Z' | 'C' | 'T'))
        .collect();
    match tail.as_str() {
        "CZT" => DimensionOrder::XYCZT,
        "CTZ" => DimensionOrder::XYCTZ,
        "ZCT" => DimensionOrder::XYZCT,
        "ZTC" => DimensionOrder::XYZTC,
        "TCZ" => DimensionOrder::XYTCZ,
        "TZC" => DimensionOrder::XYTZC,
        _ => DimensionOrder::XYZCT,
    }
}

fn unit_to_micrometers(value: f64, unit: Option<&str>) -> f64 {
    match unit.unwrap_or("micrometer").to_ascii_lowercase().as_str() {
        "angstrom" => value * 1e-4,
        "nanometer" => value * 1e-3,
        "micrometer" | "micron" | "um" | "µm" | "" => value,
        "millimeter" => value * 1e3,
        "centimeter" => value * 1e4,
        "meter" => value * 1e6,
        _ => value,
    }
}

/// Parse an omero `color` hex string (`RRGGBB`) into a packed RGBA `i32` as used
/// by OME-XML.
fn parse_omero_color(color: &str) -> Option<i32> {
    let hex = color.trim().trim_start_matches('#');
    if hex.len() != 6 && hex.len() != 8 {
        return None;
    }
    let r = u32::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u32::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u32::from_str_radix(&hex[4..6], 16).ok()?;
    let a = if hex.len() == 8 {
        u32::from_str_radix(&hex[6..8], 16).ok()?
    } else {
        0xFF
    };
    Some(((r << 24) | (g << 16) | (b << 8) | a) as i32)
}

fn parse_omero_channels(attrs: &serde_json::Map<String, Value>) -> Vec<OmeChannel> {
    let Some(omero) = attrs.get("omero").and_then(Value::as_object) else {
        return Vec::new();
    };
    let Some(channels) = omero.get("channels").and_then(Value::as_array) else {
        return Vec::new();
    };
    channels
        .iter()
        .map(|ch| OmeChannel {
            name: ch.get("label").and_then(Value::as_str).map(str::to_string),
            samples_per_pixel: 1,
            color: ch
                .get("color")
                .and_then(Value::as_str)
                .and_then(parse_omero_color),
            ..Default::default()
        })
        .collect()
}

impl OmeZarrReader {
    fn parse(&mut self, root: &Path) -> Result<()> {
        let store =
            Arc::new(FilesystemStore::new(root).map_err(|e| {
                BioFormatsError::Format(format!("zarr store {}: {e}", root.display()))
            })?);

        // Discover all multiscales image groups.
        let mut groups: BTreeMap<String, serde_json::Map<String, Value>> = BTreeMap::new();
        collect_multiscales_groups(root, root, "", &mut groups);

        // Root-level attributes (for plate / HCS metadata).
        let root_attrs = read_group_attributes(root).unwrap_or_default();

        if groups.is_empty() {
            let plain = build_plain_zarr_series(&store, root)?;
            if plain.is_empty() {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "OME-Zarr: no multiscales image found under {}",
                    root.display()
                )));
            }
            self.plate = None;
            self.store = Some(store);
            self.series = plain;
            self.current_series = 0;
            self.current_resolution = 0;
            self.refresh_metadata();
            return Ok(());
        }

        // Natural-order the group keys (root "" sorts first).
        let mut keys: Vec<String> = groups.keys().cloned().collect();
        keys.sort_by(|a, b| natural_key(a).cmp(&natural_key(b)));

        let mut series = Vec::new();
        for key in &keys {
            let attrs = &groups[key];
            let built = build_series(&store, key, attrs)?;
            series.extend(built);
        }

        if series.is_empty() {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "OME-Zarr: no readable multiscales datasets under {}",
                root.display()
            )));
        }

        self.plate = parse_plate(&root_attrs, &series);
        self.store = Some(store);
        self.series = series;
        self.current_series = 0;
        self.current_resolution = 0;
        self.refresh_metadata();
        Ok(())
    }
}

/// Build Java-compatible metadata for plain Zarr arrays without NGFF
/// `multiscales`. Java `ZarrReader.get5DShape` right-aligns non-5D shapes into
/// `[t,c,z,y,x]`, so a root 2D array is exposed as singleton T/C/Z.
fn build_plain_zarr_series(store: &Arc<FilesystemStore>, root: &Path) -> Result<Vec<ZarrSeries>> {
    let candidates = collect_plain_array_paths(root);
    let mut out = Vec::new();
    for path in candidates {
        let key = format!("/{}", path.trim_start_matches('/'));
        let array = match Array::open(store.clone(), &key) {
            Ok(array) => array,
            Err(_) => continue,
        };
        let shape = array.shape().to_vec();
        if shape.is_empty() || shape.len() > 5 {
            continue;
        }
        let dtype = format!("{}", array.data_type());
        let pixel_type = pixel_type_from_dtype(&dtype)?;
        let axis_index = java_plain_axis_index(shape.len());
        let dim = |li: usize| -> u32 {
            axis_index[li]
                .and_then(|idx| shape.get(idx).copied())
                .unwrap_or(1) as u32
        };
        let name = if path.is_empty() {
            root.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("Image")
                .trim_end_matches(".zarr")
                .to_string()
        } else {
            path.clone()
        };
        out.push(ZarrSeries {
            group_path: path.clone(),
            name,
            levels: vec![ZarrLevel {
                path,
                size_t: dim(0),
                size_c: dim(1),
                size_z: dim(2),
                size_y: dim(3),
                size_x: dim(4),
                ndim: shape.len(),
                axis_index,
                pixel_type,
                dimension_order: DimensionOrder::XYZCT,
            }],
            physical_size_x: None,
            physical_size_y: None,
            physical_size_z: None,
            time_increment: None,
            channels: Vec::new(),
        });
    }
    Ok(out)
}

fn collect_plain_array_paths(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    collect_plain_array_paths_rec(root, "", &mut out);
    out.sort_by(|a, b| natural_key(a).cmp(&natural_key(b)));
    out
}

fn collect_plain_array_paths_rec(dir: &Path, rel: &str, out: &mut Vec<String>) {
    if dir.join(".zarray").is_file() || zarr_json_is_array(dir) {
        out.push(rel.to_string());
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if !p.is_dir() {
            continue;
        }
        let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with('.') || name == "OME" {
            continue;
        }
        let child_rel = if rel.is_empty() {
            name.to_string()
        } else {
            format!("{rel}/{name}")
        };
        collect_plain_array_paths_rec(&p, &child_rel, out);
    }
}

fn zarr_json_is_array(dir: &Path) -> bool {
    let zarr_json = dir.join("zarr.json");
    let Ok(text) = std::fs::read_to_string(zarr_json) else {
        return false;
    };
    serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|json| {
            json.get("node_type")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .as_deref()
        == Some("array")
}

fn java_plain_axis_index(ndim: usize) -> [Option<usize>; 5] {
    let mut axis_index = [None; 5];
    let start = 5usize.saturating_sub(ndim);
    for idx in 0..ndim {
        axis_index[start + idx] = Some(idx);
    }
    axis_index
}

/// Build the series (usually one) described by a single multiscales group.
fn build_series(
    store: &Arc<FilesystemStore>,
    group_path: &str,
    attrs: &serde_json::Map<String, Value>,
) -> Result<Vec<ZarrSeries>> {
    let multiscales = attrs
        .get("multiscales")
        .and_then(Value::as_array)
        .ok_or_else(|| BioFormatsError::Format("multiscales is not an array".into()))?;

    let channels = parse_omero_channels(attrs);
    let mut out = Vec::new();

    for ms in multiscales {
        let names = ms
            .get("axes")
            .map(axis_names)
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| vec!["t".into(), "c".into(), "z".into(), "y".into(), "x".into()]);
        let dimension_order = dimension_order_from_axes(&names);

        // axis_index[logical] -> position in the array shape.
        let logical = ['t', 'c', 'z', 'y', 'x'];
        let mut axis_index = [None; 5];
        for (li, lc) in logical.iter().enumerate() {
            axis_index[li] = names.iter().position(|n| n.chars().next() == Some(*lc));
        }

        let datasets = ms
            .get("datasets")
            .and_then(Value::as_array)
            .ok_or_else(|| BioFormatsError::Format("multiscales.datasets missing".into()))?;

        let name = ms
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| {
                if group_path.is_empty() {
                    "Image".to_string()
                } else {
                    group_path.to_string()
                }
            });

        let mut levels = Vec::new();
        let mut phys = (None, None, None, None);
        for (li, ds) in datasets.iter().enumerate() {
            let ds_path = ds
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| BioFormatsError::Format("dataset.path missing".into()))?;
            let level_path = if group_path.is_empty() {
                ds_path.to_string()
            } else {
                format!("{group_path}/{ds_path}")
            };
            let key = format!("/{}", level_path.trim_start_matches('/'));
            let array = Array::open(store.clone(), &key)
                .map_err(|e| BioFormatsError::Format(format!("zarr array open {key}: {e}")))?;
            let shape = array.shape().to_vec();
            let dtype = format!("{}", array.data_type());
            let pixel_type = pixel_type_from_dtype(&dtype)?;

            let dim = |li: usize| -> u32 {
                axis_index[li]
                    .and_then(|idx| shape.get(idx).copied())
                    .unwrap_or(1) as u32
            };
            let level = ZarrLevel {
                path: level_path,
                size_t: dim(0),
                size_c: dim(1),
                size_z: dim(2),
                size_y: dim(3),
                size_x: dim(4),
                ndim: shape.len(),
                axis_index,
                pixel_type,
                dimension_order,
            };
            levels.push(level);

            // Physical sizes / time increment from level-0 scale transform.
            if li == 0 {
                if let Some(scale) = dataset_scale(ds) {
                    let get = |logical: usize, unit: Option<&str>| -> Option<f64> {
                        axis_index[logical]
                            .and_then(|idx| scale.get(idx).copied())
                            .map(|v| unit_to_micrometers(v, unit))
                    };
                    let units = axis_units(ms);
                    phys = (
                        get(4, units.get("x").map(String::as_str)), // x
                        get(3, units.get("y").map(String::as_str)), // y
                        get(2, units.get("z").map(String::as_str)), // z
                        // time increment (seconds) — no unit conversion applied.
                        axis_index[0].and_then(|idx| scale.get(idx).copied()),
                    );
                }
            }
        }

        out.push(ZarrSeries {
            group_path: group_path.to_string(),
            name,
            levels,
            physical_size_x: phys.0,
            physical_size_y: phys.1,
            physical_size_z: phys.2,
            time_increment: phys.3,
            channels: channels.clone(),
        });
    }
    Ok(out)
}

/// Extract the `scale` array from a dataset's `coordinateTransformations`.
fn dataset_scale(ds: &Value) -> Option<Vec<f64>> {
    let transforms = ds.get("coordinateTransformations")?.as_array()?;
    for t in transforms {
        if t.get("type").and_then(Value::as_str) == Some("scale") {
            if let Some(scale) = t.get("scale").and_then(Value::as_array) {
                return Some(scale.iter().filter_map(Value::as_f64).collect());
            }
        }
    }
    None
}

/// Map axis name → unit string from a multiscales `axes` list.
fn axis_units(ms: &Value) -> BTreeMap<String, String> {
    let mut units = BTreeMap::new();
    if let Some(arr) = ms.get("axes").and_then(Value::as_array) {
        for a in arr {
            if let (Some(name), Some(unit)) = (
                a.get("name").and_then(Value::as_str),
                a.get("unit").and_then(Value::as_str),
            ) {
                units.insert(name.to_ascii_lowercase(), unit.to_string());
            }
        }
    }
    units
}

/// Parse an OME-NGFF `plate` attribute into an [`OmePlate`], wiring each well
/// sample to the discovered image series whose group path matches the well/field
/// layout. Best-effort; returns `None` if there is no plate metadata.
fn parse_plate(
    root_attrs: &serde_json::Map<String, Value>,
    series: &[ZarrSeries],
) -> Option<OmePlate> {
    let plate = root_attrs.get("plate").and_then(Value::as_object)?;
    let rows = plate.get("rows").and_then(Value::as_array);
    let columns = plate.get("columns").and_then(Value::as_array);
    let wells = plate.get("wells").and_then(Value::as_array)?;
    let name = plate
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_string);

    let row_count = rows.map(|r| r.len()).unwrap_or(0) as u32;
    let col_count = columns.map(|c| c.len()).unwrap_or(0) as u32;

    let mut ome_wells = Vec::new();
    for well in wells {
        let well = match well.as_object() {
            Some(w) => w,
            None => continue,
        };
        let well_path = well.get("path").and_then(Value::as_str).unwrap_or("");
        let row_index = well
            .get("rowIndex")
            .or_else(|| well.get("row_index"))
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32;
        let col_index = well
            .get("columnIndex")
            .or_else(|| well.get("column_index"))
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32;

        // Each field/site under the well is a series whose group_path starts
        // with the well path.
        let mut samples = Vec::new();
        for (si, s) in series.iter().enumerate() {
            if s.group_path == well_path || s.group_path.starts_with(&format!("{well_path}/")) {
                samples.push(OmeWellSample {
                    index: samples.len() as u32,
                    image_ref: Some(si),
                    ..Default::default()
                });
            }
        }
        ome_wells.push(OmeWell {
            row: row_index,
            column: col_index,
            well_samples: samples,
            ..Default::default()
        });
    }

    Some(OmePlate {
        name,
        rows: row_count,
        columns: col_count,
        wells: ome_wells,
        ..Default::default()
    })
}

/// Compute (z, c, t) coordinates for a plane index given the dimension order.
fn zct_coords(order: DimensionOrder, sz: u32, sc: u32, st: u32, no: u32) -> (u32, u32, u32) {
    let chars = match order {
        DimensionOrder::XYCZT => ['C', 'Z', 'T'],
        DimensionOrder::XYCTZ => ['C', 'T', 'Z'],
        DimensionOrder::XYZCT => ['Z', 'C', 'T'],
        DimensionOrder::XYZTC => ['Z', 'T', 'C'],
        DimensionOrder::XYTCZ => ['T', 'C', 'Z'],
        DimensionOrder::XYTZC => ['T', 'Z', 'C'],
    };
    let size = |ch: char| -> u32 {
        match ch {
            'Z' => sz.max(1),
            'C' => sc.max(1),
            'T' => st.max(1),
            _ => 1,
        }
    };
    let s0 = size(chars[0]);
    let s1 = size(chars[1]);
    let idx0 = no % s0;
    let rest = no / s0;
    let idx1 = rest % s1;
    let idx2 = rest / s1;
    let mut z = 0;
    let mut c = 0;
    let mut t = 0;
    for (ch, idx) in chars.iter().zip([idx0, idx1, idx2]) {
        match ch {
            'Z' => z = idx,
            'C' => c = idx,
            'T' => t = idx,
            _ => {}
        }
    }
    (z, c, t)
}

/// Serialise typed elements (host order) to little-endian bytes.
fn elements_to_le_bytes<T, F>(elems: &[T], to_le: F) -> Vec<u8>
where
    F: Fn(&T) -> Vec<u8>,
{
    let mut out = Vec::with_capacity(elems.len() * std::mem::size_of::<T>());
    for e in elems {
        out.extend_from_slice(&to_le(e));
    }
    out
}

impl FormatReader for OmeZarrReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        is_zarr_path(path)
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let root = resolve_root(path);
        if !root.exists() {
            return Err(BioFormatsError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("OME-Zarr root not found: {}", root.display()),
            )));
        }
        self.parse(&root)?;
        self.root = Some(root);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.root = None;
        self.store = None;
        self.series.clear();
        self.plate = None;
        self.current_series = 0;
        self.current_resolution = 0;
        self.metadata = ImageMetadata::default();
        self.open_array = None;
        Ok(())
    }

    fn series_count(&self) -> usize {
        self.series.len()
    }

    fn set_series(&mut self, series: usize) -> Result<()> {
        if series >= self.series.len() {
            return Err(BioFormatsError::SeriesOutOfRange(series));
        }
        self.current_series = series;
        self.current_resolution = 0;
        self.open_array = None;
        self.refresh_metadata();
        Ok(())
    }

    fn series(&self) -> usize {
        self.current_series
    }

    fn metadata(&self) -> &ImageMetadata {
        if self.series.is_empty() {
            return crate::common::reader::uninitialized_metadata();
        }
        &self.metadata
    }

    fn resolution_count(&self) -> usize {
        self.series
            .get(self.current_series)
            .map(|s| s.levels.len())
            .unwrap_or(1)
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        let count = self.resolution_count();
        if level >= count {
            return Err(BioFormatsError::SeriesOutOfRange(level));
        }
        self.current_resolution = level;
        self.open_array = None;
        self.refresh_metadata();
        Ok(())
    }

    fn resolution(&self) -> usize {
        self.current_resolution
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let (w, h) = {
            let level = self.current_level()?;
            (level.size_x, level.size_y)
        };
        self.open_bytes_region(plane_index, 0, 0, w, h)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        self.ensure_open_array()?;
        let level = self.current_level()?.clone();
        let image_count = level.size_z * level.size_c * level.size_t;
        if plane_index >= image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let (z, c, t) = zct_coords(
            level.dimension_order,
            level.size_z,
            level.size_c,
            level.size_t,
            plane_index,
        );

        // Build the array subset in the array's own axis order.
        // logical coord per axis: t,c,z,y,x
        let coord = [t, c, z, y, x];
        let extent = [1u32, 1, 1, h, w];
        let mut start = vec![0u64; level.ndim];
        let mut size = vec![1u64; level.ndim];
        for li in 0..5 {
            if let Some(idx) = level.axis_index[li] {
                if idx < level.ndim {
                    start[idx] = coord[li] as u64;
                    size[idx] = extent[li] as u64;
                }
            }
        }
        let subset = ArraySubset::new_with_start_shape(start, size)
            .map_err(|e| BioFormatsError::Format(format!("zarr subset: {e}")))?;

        let array = &self.open_array.as_ref().unwrap().1;
        let bytes = match level.pixel_type {
            PixelType::Uint8 | PixelType::Bit => array
                .retrieve_array_subset::<Vec<u8>>(&subset)
                .map(|v| v)
                .map_err(read_err)?,
            PixelType::Int8 => array
                .retrieve_array_subset::<Vec<i8>>(&subset)
                .map(|v| v.iter().map(|e| *e as u8).collect())
                .map_err(read_err)?,
            PixelType::Uint16 => array
                .retrieve_array_subset::<Vec<u16>>(&subset)
                .map(|v| elements_to_le_bytes(&v, |e| e.to_le_bytes().to_vec()))
                .map_err(read_err)?,
            PixelType::Int16 => array
                .retrieve_array_subset::<Vec<i16>>(&subset)
                .map(|v| elements_to_le_bytes(&v, |e| e.to_le_bytes().to_vec()))
                .map_err(read_err)?,
            PixelType::Uint32 => array
                .retrieve_array_subset::<Vec<u32>>(&subset)
                .map(|v| elements_to_le_bytes(&v, |e| e.to_le_bytes().to_vec()))
                .map_err(read_err)?,
            PixelType::Int32 => array
                .retrieve_array_subset::<Vec<i32>>(&subset)
                .map(|v| elements_to_le_bytes(&v, |e| e.to_le_bytes().to_vec()))
                .map_err(read_err)?,
            PixelType::Float32 => array
                .retrieve_array_subset::<Vec<f32>>(&subset)
                .map(|v| elements_to_le_bytes(&v, |e| e.to_le_bytes().to_vec()))
                .map_err(read_err)?,
            PixelType::Float64 => array
                .retrieve_array_subset::<Vec<f64>>(&subset)
                .map(|v| elements_to_le_bytes(&v, |e| e.to_le_bytes().to_vec()))
                .map_err(read_err)?,
        };
        Ok(bytes)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        self.open_bytes(plane_index)
    }

    fn ome_metadata(&self) -> Option<OmeMetadata> {
        if self.series.is_empty() {
            return None;
        }
        let images = self
            .series
            .iter()
            .map(|s| {
                // Default a single channel when no omero metadata is present.
                let channels = if s.channels.is_empty() {
                    let c = s.levels.first().map(|l| l.size_c).unwrap_or(1).max(1);
                    (0..c)
                        .map(|_| OmeChannel {
                            samples_per_pixel: 1,
                            ..Default::default()
                        })
                        .collect()
                } else {
                    s.channels.clone()
                };
                OmeImage {
                    name: Some(s.name.clone()),
                    physical_size_x: s.physical_size_x,
                    physical_size_y: s.physical_size_y,
                    physical_size_z: s.physical_size_z,
                    time_increment: s.time_increment,
                    channels,
                    ..Default::default()
                }
            })
            .collect();
        Some(OmeMetadata {
            images,
            plates: self.plate.clone().into_iter().collect(),
            ..Default::default()
        })
    }
}

fn read_err(e: zarrs::array::ArrayError) -> BioFormatsError {
    BioFormatsError::Format(format!("zarr read: {e}"))
}

impl OmeZarrReader {
    /// Expose discovered series metadata for richer callers/tests.
    #[allow(dead_code)]
    pub(crate) fn series_name(&self, index: usize) -> Option<&str> {
        self.series.get(index).map(|s| s.name.as_str())
    }
}
