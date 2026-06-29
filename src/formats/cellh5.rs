//! CellH5 (.ch5) format reader.
//!
//! CellH5 is an HDF5-based format for cell biology HCS data, developed alongside
//! CellProfiler and used in the Sommer et al. cell tracking / segmentation pipeline.
//!
//! Common HDF5 layout:
//!   sample/0/position/{well}/image/channel/{ch}   — uint16 [n_frames, y, x] or [y, x]
//!   plate/{plate}/experiment/{well}/image/channel/{ch}
//!
//! Detection: extension `.ch5` only (HDF5 magic-byte detection disabled to avoid
//! conflicts with other HDF5-based readers).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, LookupTable, MetadataValue};
use crate::common::ome_metadata::{
    create_lsid, OmeMetadata, OmePlate, OmeROI, OmeShape, OmeWell, OmeWellSample,
};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;

use hdf5_pure_rust::format::messages::datatype::DatatypeClass;
use hdf5_pure_rust::{HyperslabDim, Selection};

const CELLH5_METADATA_NODE_LIMIT: usize = 512;

/// One CellH5 series: its image-data dataset path and parsed dimensions.
struct CellH5Series {
    /// HDF5 path to the 5D `[c, t, z, y, x]` dataset for this series.
    dataset_path: String,
    meta: ImageMetadata,
    image_name: String,
    plate: String,
    well: String,
    site: String,
}

pub struct CellH5Reader {
    path: Option<PathBuf>,
    /// One entry per series (multi-position / multi-well, plus segmentation
    /// label-image series), matching CellH5Reader.java parseStructure().
    series: Vec<CellH5Series>,
    current_series: usize,
}

impl CellH5Reader {
    pub fn new() -> Self {
        CellH5Reader {
            path: None,
            series: Vec::new(),
            current_series: 0,
        }
    }

    fn read_region(&self, plane_index: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let series = self
            .series
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        let meta = &series.meta;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let x_end = x
            .checked_add(w)
            .ok_or_else(|| BioFormatsError::Format("CellH5 region X overflows".into()))?;
        let y_end = y
            .checked_add(h)
            .ok_or_else(|| BioFormatsError::Format("CellH5 region Y overflows".into()))?;
        if x_end > meta.size_x || y_end > meta.size_y {
            return Err(BioFormatsError::Format(
                "CellH5 region is outside image bounds".into(),
            ));
        }

        let bytes_per_sample = (meta.bits_per_pixel / 8) as usize;
        let region_pixels = w as usize * h as usize;
        let region_bytes = region_pixels
            .checked_mul(bytes_per_sample)
            .ok_or_else(|| BioFormatsError::Format("CellH5 region byte count overflows".into()))?;

        // dimension_order XYZTC: z varies fastest, then t, then c.
        let sz = meta.size_z as usize;
        let st = meta.size_t as usize;
        let z = (plane_index as usize) % sz;
        let t = (plane_index as usize / sz) % st;
        let c = (plane_index as usize) / (sz * st);

        // Single dataset `[c, t, z, y, x]`; lower-rank datasets drop leading
        // dimensions in the same order dims_from_shape() assumes.
        let ds_path = series.dataset_path.clone();
        let path = self
            .path
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?
            .clone();
        let file = hdf5_pure_rust::File::open(&path)
            .map_err(|e| BioFormatsError::Format(format!("HDF5: {e}")))?;
        let ds = file
            .dataset(&ds_path)
            .map_err(|e| BioFormatsError::Format(format!("dataset {ds_path}: {e}")))?;

        let shape = ds.shape().unwrap_or_default();
        let y0 = y as u64;
        let x0 = x as u64;
        let yh = h as u64;
        let xw = w as u64;
        let dims: Vec<HyperslabDim> = match shape.len() {
            5 => vec![
                HyperslabDim::new(c as u64, 1, 1, 1),
                HyperslabDim::new(t as u64, 1, 1, 1),
                HyperslabDim::new(z as u64, 1, 1, 1),
                HyperslabDim::new(y0, 1, yh, 1),
                HyperslabDim::new(x0, 1, xw, 1),
            ],
            4 => vec![
                HyperslabDim::new(c as u64, 1, 1, 1),
                HyperslabDim::new(t as u64, 1, 1, 1),
                HyperslabDim::new(y0, 1, yh, 1),
                HyperslabDim::new(x0, 1, xw, 1),
            ],
            3 => vec![
                HyperslabDim::new(t as u64, 1, 1, 1),
                HyperslabDim::new(y0, 1, yh, 1),
                HyperslabDim::new(x0, 1, xw, 1),
            ],
            2 => vec![
                HyperslabDim::new(y0, 1, yh, 1),
                HyperslabDim::new(x0, 1, xw, 1),
            ],
            other => {
                return Err(BioFormatsError::Format(format!(
                    "CellH5: dataset {ds_path} has unsupported rank {other}"
                )));
            }
        };
        let selection = Selection::Hyperslab(dims);

        let raw: Vec<u8> = match bytes_per_sample {
            1 => ds
                .read_slice::<u8, _>(selection)
                .map_err(|e| BioFormatsError::Format(format!("HDF5 read: {e}")))?,
            2 => {
                let words: Vec<u16> = ds
                    .read_slice::<u16, _>(selection)
                    .map_err(|e| BioFormatsError::Format(format!("HDF5 read: {e}")))?;
                let mut out = Vec::with_capacity(words.len() * 2);
                for word in words {
                    out.extend_from_slice(&word.to_le_bytes());
                }
                out
            }
            4 => {
                let dwords: Vec<i32> = ds
                    .read_slice::<i32, _>(selection)
                    .map_err(|e| BioFormatsError::Format(format!("HDF5 read: {e}")))?;
                let mut out = Vec::with_capacity(dwords.len() * 4);
                for dword in dwords {
                    out.extend_from_slice(&dword.to_le_bytes());
                }
                out
            }
            _ => ds
                .read_slice::<u8, _>(selection)
                .map_err(|e| BioFormatsError::Format(format!("HDF5 read: {e}")))?,
        };

        if raw.len() == region_bytes {
            Ok(raw)
        } else if raw.len() > region_bytes {
            Ok(raw[..region_bytes].to_vec())
        } else {
            Err(BioFormatsError::Format(format!(
                "CellH5: dataset {ds_path} is too short for plane {plane_index}"
            )))
        }
    }
}

impl Default for CellH5Reader {
    fn default() -> Self {
        Self::new()
    }
}

/// Walk the CellH5 experiment structure and collect the image-data (and
/// segmentation label-image) dataset paths for every position, in the order
/// CellH5Reader.java#parseStructure() produces them.
///
/// Layout (Java `CellH5Constants`):
///   `/sample/0/plate/{plate}/experiment/{well}/position/{site}/image/channel`
///   `/sample/0/plate/{plate}/experiment/{well}/position/{site}/image/region`
///
/// `image/channel` (and `image/region`) is itself the 5D dataset
/// `[channel, time, zslice, y, x]`, NOT a group of per-channel leaves.
/// Returns image-data paths first (one series per position), then the
/// segmentation paths, matching the Java two-pass ordering.
fn find_image_datasets(file: &hdf5_pure_rust::File) -> Vec<String> {
    // Resolve the `/sample/0/plate` prefix; fall back to a looser scan so
    // synthetic fixtures that omit the fixed `0` level still work.
    let mut positions: Vec<String> = Vec::new();

    let plate_roots = ["sample/0/plate", "sample/plate", "plate"];
    for plate_root in &plate_roots {
        let plate_g = match file.group(plate_root) {
            Ok(g) => g,
            Err(_) => continue,
        };
        let plates = hdf5_group_members(&plate_g).unwrap_or_default();
        for plate in &plates {
            // experiment/{well}/position/{site}
            let well_root = format!("{plate_root}/{plate}/experiment");
            let well_g = match file.group(&well_root) {
                Ok(g) => g,
                Err(_) => continue,
            };
            for well in hdf5_group_members(&well_g).unwrap_or_default() {
                let site_root = format!("{well_root}/{well}/position");
                let site_g = match file.group(&site_root) {
                    Ok(g) => g,
                    Err(_) => continue,
                };
                for site in hdf5_group_members(&site_g).unwrap_or_default() {
                    positions.push(format!("{site_root}/{site}"));
                }
            }
        }
        if !positions.is_empty() {
            break;
        }
    }

    // Pass 1: image/channel datasets. Pass 2: image/region (segmentation).
    let mut paths = Vec::new();
    for pos in &positions {
        let ch_path = format!("{pos}/image/channel");
        if file.dataset(&ch_path).is_ok() {
            paths.push(ch_path);
        }
    }
    for pos in &positions {
        let seg_path = format!("{pos}/image/region");
        if file.dataset(&seg_path).is_ok() {
            paths.push(seg_path);
        }
    }
    paths
}

fn hdf5_attr_value(attr: &hdf5_pure_rust::Attribute) -> MetadataValue {
    // New API: no AttrValue enum. Inspect the datatype class and read accordingly.
    let dtype = attr.dtype();
    // Number of elements (scalar attrs have empty/[1] shape).
    let n_elems: u64 = attr.shape().iter().product::<u64>().max(1);
    match dtype.class() {
        DatatypeClass::FloatingPoint => {
            if n_elems > 1 {
                match attr.read::<f64>() {
                    Ok(v) => MetadataValue::String(format!("{v:?}")),
                    Err(_) => MetadataValue::String(String::new()),
                }
            } else if let Some(v) = attr.read_scalar_f64() {
                MetadataValue::Float(v)
            } else {
                MetadataValue::String(attr.read_string())
            }
        }
        DatatypeClass::FixedPoint => {
            if n_elems > 1 {
                match attr.read::<i64>() {
                    Ok(v) => MetadataValue::String(format!("{v:?}")),
                    Err(_) => MetadataValue::String(String::new()),
                }
            } else if let Some(v) = attr.read_scalar_i64() {
                MetadataValue::Int(v)
            } else {
                MetadataValue::String(attr.read_string())
            }
        }
        DatatypeClass::String | DatatypeClass::VarLen => {
            if n_elems > 1 {
                match attr.read_strings() {
                    Ok(v) => MetadataValue::String(v.join(",")),
                    Err(_) => MetadataValue::String(attr.read_string()),
                }
            } else {
                let s = attr.read_string();
                MetadataValue::String(s.trim_matches('\0').trim().to_string())
            }
        }
        _ => MetadataValue::String(attr.read_string()),
    }
}

fn collect_group_attrs(
    group: &hdf5_pure_rust::Group,
    path: &str,
    meta_map: &mut HashMap<String, MetadataValue>,
) {
    if let Ok(names) = group.attr_names() {
        for name in names {
            if let Ok(attr) = group.attr(&name) {
                meta_map.insert(format!("cellh5_attr:{path}@{name}"), hdf5_attr_value(&attr));
            }
        }
    }
}

fn collect_dataset_metadata(
    dataset: &hdf5_pure_rust::Dataset,
    path: &str,
    meta_map: &mut HashMap<String, MetadataValue>,
) {
    let dtype_size = dataset.dtype().map(|dt| hdf5_dtype_size(&dt)).unwrap_or(0);
    let shape = dataset.shape().unwrap_or_default();
    meta_map.insert(
        format!("cellh5_dataset:{path}"),
        MetadataValue::String(format!("shape={:?}; dtype_size={dtype_size}", shape)),
    );

    if let Ok(names) = dataset.attr_names() {
        for name in names {
            if let Ok(attr) = dataset.attr(&name) {
                meta_map.insert(format!("cellh5_attr:{path}@{name}"), hdf5_attr_value(&attr));
            }
        }
    }
}

fn collect_file_attrs(file: &hdf5_pure_rust::File, meta_map: &mut HashMap<String, MetadataValue>) {
    // New API: no file.root(); attributes live directly on the File.
    if let Ok(names) = file.attr_names() {
        for name in names {
            if let Ok(attr) = file.attr(&name) {
                meta_map.insert(format!("cellh5_attr:/@{name}"), hdf5_attr_value(&attr));
            }
        }
    }
}

fn collect_hdf5_metadata(
    file: &hdf5_pure_rust::File,
    path: &str,
    meta_map: &mut HashMap<String, MetadataValue>,
    visited: &mut usize,
) {
    if *visited >= CELLH5_METADATA_NODE_LIMIT {
        return;
    }
    *visited += 1;

    let group = match file.group(path) {
        Ok(group) => group,
        Err(_) => return,
    };
    collect_group_attrs(&group, path, meta_map);

    let members = match hdf5_group_members(&group) {
        Ok(members) => members,
        Err(_) => return,
    };
    for member in members {
        if *visited >= CELLH5_METADATA_NODE_LIMIT {
            return;
        }
        let child_path = if path == "/" {
            format!("/{member}")
        } else {
            format!("{path}/{member}")
        };
        if let Ok(dataset) = file.dataset(&child_path) {
            *visited += 1;
            collect_dataset_metadata(&dataset, &child_path, meta_map);
        } else if file.group(&child_path).is_ok() {
            collect_hdf5_metadata(file, &child_path, meta_map, visited);
        }
    }
}

/// Derive (sizeX, sizeY, sizeZ, sizeC, sizeT) from a CellH5 dataset shape.
/// The canonical layout is 5D `[c, t, z, y, x]` (CellH5Reader.java#getShape:
/// `ctzyx`); lower-rank fixtures are accepted for robustness.
fn shape_dim(value: u64, label: &str) -> Result<u32> {
    if value == 0 || value > u32::MAX as u64 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "CellH5: invalid {label} dimension {value}"
        )));
    }
    Ok(value as u32)
}

fn dims_from_shape(shape: &[u64]) -> Result<(u32, u32, u32, u32, u32)> {
    match shape.len() {
        5 => Ok((
            shape_dim(shape[4], "X")?,
            shape_dim(shape[3], "Y")?,
            shape_dim(shape[2], "Z")?,
            shape_dim(shape[0], "C")?,
            shape_dim(shape[1], "T")?,
        )),
        4 => Ok((
            shape_dim(shape[3], "X")?,
            shape_dim(shape[2], "Y")?,
            1,
            shape_dim(shape[0], "C")?,
            shape_dim(shape[1], "T")?,
        )),
        3 => Ok((
            shape_dim(shape[2], "X")?,
            shape_dim(shape[1], "Y")?,
            1,
            1,
            shape_dim(shape[0], "T")?,
        )),
        2 => Ok((
            shape_dim(shape[1], "X")?,
            shape_dim(shape[0], "Y")?,
            1,
            1,
            1,
        )),
        _ => Err(BioFormatsError::UnsupportedFormat(format!(
            "CellH5: unsupported dataset rank {}",
            shape.len()
        ))),
    }
}

fn cellh5_series_coord(ds_path: &str) -> Option<(String, String, String)> {
    let parts: Vec<&str> = ds_path.split('/').collect();
    let plate_i = parts.iter().position(|part| *part == "plate")?;
    let experiment_i = parts.iter().position(|part| *part == "experiment")?;
    let position_i = parts.iter().position(|part| *part == "position")?;
    Some((
        parts.get(plate_i + 1)?.to_string(),
        parts.get(experiment_i + 1)?.to_string(),
        parts.get(position_i + 1)?.to_string(),
    ))
}

fn cellh5_well_row_column(well: &str) -> (u32, u32) {
    let mut chars = well.chars();
    let Some(row_char) = chars.next() else {
        return (0, 0);
    };
    let row = "ABCDEFGHIJKLMNOP"
        .chars()
        .position(|ch| ch == row_char.to_ascii_uppercase());
    let column =
        chars.as_str().parse::<u32>().ok().and_then(
            |value| {
                if value > 0 {
                    Some(value - 1)
                } else {
                    None
                }
            },
        );
    match (row, column) {
        (Some(row), Some(column)) => (row as u32, column),
        _ => (0, 0),
    }
}

fn cellh5_roi_count(
    file: &hdf5_pure_rust::File,
    first_dataset_path: &str,
    series_count: usize,
) -> usize {
    if series_count > 2 {
        return 0;
    }
    let Some(position_path) = first_dataset_path
        .strip_suffix("/image/channel")
        .or_else(|| first_dataset_path.strip_suffix("/image/region"))
    else {
        return 0;
    };
    let feature_path = format!("{position_path}/feature");
    let feature_group = match file.group(&feature_path) {
        Ok(group) => group,
        Err(_) => return 0,
    };
    let mut count = 0usize;
    for object_name in hdf5_group_members(&feature_group).unwrap_or_default() {
        let bbox_path = format!("{feature_path}/{object_name}/bounding_box");
        if let Ok(dataset) = file.dataset(&bbox_path) {
            if let Some(rows) = dataset.shape().unwrap_or_default().first() {
                count = count.saturating_add(*rows as usize);
            }
        }
    }
    count
}

fn parse_cellh5(path: &Path) -> Result<Vec<CellH5Series>> {
    let file = hdf5_pure_rust::File::open(path)
        .map_err(|e| BioFormatsError::Format(format!("HDF5 open error: {e}")))?;

    let dataset_paths = find_image_datasets(&file);

    // Global metadata, attached to every series (cheap clone of small map).
    let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
    meta_map.insert("format".into(), MetadataValue::String("CellH5".into()));
    collect_file_attrs(&file, &mut meta_map);
    collect_hdf5_metadata(&file, "/", &mut meta_map, &mut 0);

    if dataset_paths.is_empty() {
        return Err(BioFormatsError::UnsupportedFormat(
            "CellH5: no image datasets found in supported sample/plate channel layouts".into(),
        ));
    }
    let roi_count = cellh5_roi_count(&file, &dataset_paths[0], dataset_paths.len());

    let mut series_list = Vec::with_capacity(dataset_paths.len());
    for ds_path in dataset_paths {
        let (plate, well, site) = cellh5_series_coord(&ds_path).unwrap_or_else(|| {
            (
                "PLATE_00".to_string(),
                "WELL_00".to_string(),
                "0".to_string(),
            )
        });
        let is_segmentation = ds_path.ends_with("/image/region");
        let image_name = if is_segmentation {
            format!("P_{plate}, W_{well}_{site} label image")
        } else {
            format!("P_{plate}, W_{well}_{site}")
        };
        let ds = file
            .dataset(&ds_path)
            .map_err(|e| BioFormatsError::Format(format!("dataset {ds_path}: {e}")))?;
        let shape = ds.shape().unwrap_or_default();
        let (size_x, size_y, size_z, size_c, size_t) =
            dims_from_shape(&shape).map_err(|err| match err {
                BioFormatsError::UnsupportedFormat(message) => {
                    BioFormatsError::UnsupportedFormat(format!("{message} for {ds_path}"))
                }
                other => other,
            })?;

        let dtype_size = ds.dtype().map(|dt| hdf5_dtype_size(&dt)).map_err(|e| {
            BioFormatsError::Format(format!("CellH5: cannot read dtype for {ds_path}: {e}"))
        })?;
        // Java CellH5Reader.java:445-455 maps element size to pixel type:
        // 1 → UINT8, 2 → UINT16, 4 → INT32 (signed).
        let pixel_type = match dtype_size {
            1 => PixelType::Uint8,
            2 => PixelType::Uint16,
            4 => PixelType::Int32,
            other => {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "CellH5: unsupported dtype size {other} for {ds_path}"
                )));
            }
        };
        let bytes_per_sample: usize = match pixel_type {
            PixelType::Uint8 => 1,
            PixelType::Int32 => 4,
            _ => 2,
        };

        let mut sm = meta_map.clone();
        sm.insert(
            "cellh5_series_path".into(),
            MetadataValue::String(ds_path.clone()),
        );
        sm.insert(
            "cellh5_roi_count".into(),
            MetadataValue::Int(roi_count as i64),
        );

        let meta = ImageMetadata {
            size_x,
            size_y,
            size_z,
            size_c,
            size_t,
            pixel_type,
            bits_per_pixel: (bytes_per_sample * 8) as u8,
            image_count: size_z
                .checked_mul(size_c)
                .and_then(|v| v.checked_mul(size_t))
                .ok_or_else(|| {
                    BioFormatsError::Format(format!("CellH5: image count overflows for {ds_path}"))
                })?,
            // Storage is [c, t, z, y, x]: x,y fastest, then z, then t, then c.
            dimension_order: DimensionOrder::XYZTC,
            is_rgb: false,
            is_interleaved: false,
            // Java CellH5Reader marks both image and segmentation series indexed.
            is_indexed: true,
            is_little_endian: true,
            resolution_count: 1,
            thumbnail: false,
            series_metadata: sm,
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        };

        series_list.push(CellH5Series {
            dataset_path: ds_path,
            meta,
            image_name,
            plate,
            well,
            site,
        });
    }

    Ok(series_list)
}

fn hdf5_group_members(
    group: &hdf5_pure_rust::Group,
) -> std::result::Result<Vec<String>, hdf5_pure_rust::Error> {
    // New API: member_names() returns all child links (groups + datasets).
    group.member_names()
}

fn hdf5_dtype_size(dtype: &hdf5_pure_rust::Datatype) -> usize {
    dtype.size()
}

/// Java CellH5Reader.get8BitLookupTable(): select a simple 8-bit ramp by the
/// channel last read. The crate stores LUT entries as u16, so the byte ramp is
/// widened while preserving the 0..255 values.
fn cellh5_8bit_lookup_table(channel: usize) -> LookupTable {
    let ramp: Vec<u16> = (0..256).map(|v| v as u16).collect();
    let zero = vec![0u16; 256];
    match channel {
        0 => LookupTable {
            red: ramp,
            green: zero.clone(),
            blue: zero,
        },
        1 => LookupTable {
            red: zero.clone(),
            green: ramp,
            blue: zero,
        },
        2 => LookupTable {
            red: zero.clone(),
            green: zero,
            blue: ramp,
        },
        3 => LookupTable {
            red: zero,
            green: ramp.clone(),
            blue: ramp,
        },
        4 => LookupTable {
            red: ramp.clone(),
            green: zero,
            blue: ramp,
        },
        5 => LookupTable {
            red: ramp.clone(),
            green: ramp,
            blue: zero,
        },
        _ => LookupTable {
            red: ramp.clone(),
            green: ramp.clone(),
            blue: ramp,
        },
    }
}

// =====================================================================
// Writer
// =====================================================================

use crate::common::metadata::DimensionOrder as MetaDimensionOrder;
use crate::common::writer::FormatWriter;
use hdf5_pure_rust::WritableFile;

/// CellH5 (.ch5) HDF5 writer.
///
/// Faithful port of Java `loci.formats.out.CellH5Writer`. It lays out the
/// CellH5 HDF5 structure:
///
///   `/definition/object`, `/definition/feature`, `/definition/image`  (groups)
///   `/sample/0/plate/<plate>/experiment/<well>/position/<site>/image/channel`
///
/// where `image/channel` is the 5D `[sizeC, sizeT, sizeZ, sizeY, sizeX]`
/// pixel dataset. Java's `JHDFService` writes plane-by-plane into a
/// pre-initialised dataset via `writeArraySlice`; the `hdf5_pure_rust` writer
/// has no in-place slice-write API, so we buffer all planes and emit the full
/// dataset in one `write()` call on `close()`. The on-disk layout is identical.
///
/// Supported pixel types mirror Java `getPixelTypes`: UINT8, UINT16, INT32.
pub struct CellH5Writer {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    /// `bpp` (bytes per pixel): 1, 2, or 4, as in the Java field.
    bpp: usize,
    /// Buffered plane bytes indexed by the incoming `plane_index`.
    planes: Vec<Option<Vec<u8>>>,
    /// Default CellH5 plate / well / site coordinates (Java defaults).
    plate: String,
    well: String,
    site: i32,
}

impl CellH5Writer {
    pub fn new() -> Self {
        CellH5Writer {
            path: None,
            meta: None,
            bpp: 0,
            planes: Vec::new(),
            plate: "PLATE_00".to_string(),
            well: "WELL_00".to_string(),
            site: 1,
        }
    }

    /// The image-data dataset path, mirroring Java's `outputPath`:
    /// `/sample/0/plate/<plate>/experiment/<well>/position/<site>/image/channel`.
    fn output_path(&self) -> String {
        format!(
            "/sample/0/plate/{}/experiment/{}/position/{}/image/channel",
            self.plate, self.well, self.site
        )
    }
}

impl Default for CellH5Writer {
    fn default() -> Self {
        Self::new()
    }
}

/// Decompose a plane index into (c, t, z), porting the dimension-order logic of
/// Java `CellH5Writer.saveBytes()`. The Java implementation handles XYCZT,
/// XYCTZ, and XYZTC explicitly; other orders are an error there too.
fn cellh5_ctz_coords(
    order: MetaDimensionOrder,
    size_c: u32,
    size_z: u32,
    size_t: u32,
    no: u32,
) -> Result<(u32, u32, u32)> {
    let size_c = size_c.max(1);
    let size_z = size_z.max(1);
    let size_t = size_t.max(1);
    match order {
        MetaDimensionOrder::XYCZT => {
            let c = no % size_c;
            let z = ((no - c) / size_c) % size_z;
            let t = ((no - c) / size_c) / size_z;
            Ok((c, t, z))
        }
        MetaDimensionOrder::XYCTZ => {
            let c = no % size_c;
            let t = ((no - c) / size_c) % size_t;
            let z = ((no - c) / size_c) / size_t;
            Ok((c, t, z))
        }
        MetaDimensionOrder::XYZTC => {
            let z = no % size_z;
            let t = ((no - z) / size_z) % size_t;
            let c = ((no - z) / size_z) / size_t;
            Ok((c, t, z))
        }
        other => Err(BioFormatsError::Format(format!(
            "CellH5Writer: dimension order not understood: {other:?}"
        ))),
    }
}

impl FormatWriter for CellH5Writer {
    fn is_this_type(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("ch5"))
            .unwrap_or(false)
    }

    fn set_metadata(&mut self, meta: &ImageMetadata) -> Result<()> {
        // Java getPixelTypes: UINT8, UINT16, INT32. bpp derived from type.
        let bpp = match meta.pixel_type {
            PixelType::Uint8 => 1,
            PixelType::Uint16 => 2,
            PixelType::Int32 => 4,
            other => {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "CellH5Writer: pixel type {other:?} not supported (UINT8/UINT16/INT32 only)"
                )));
            }
        };
        self.bpp = bpp;
        self.meta = Some(meta.clone());
        Ok(())
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let meta = self
            .meta
            .as_ref()
            .ok_or_else(|| BioFormatsError::Format("CellH5Writer: set_metadata first".into()))?;
        // Plane count = sizeC * sizeT * sizeZ (one sample per plane).
        let count = meta
            .size_c
            .max(1)
            .checked_mul(meta.size_t.max(1))
            .and_then(|v| v.checked_mul(meta.size_z.max(1)))
            .ok_or_else(|| BioFormatsError::Format("CellH5Writer: plane count overflows".into()))?;
        self.path = Some(path.to_path_buf());
        self.planes = (0..count as usize).map(|_| None).collect();
        Ok(())
    }

    fn save_bytes(&mut self, plane_index: u32, data: &[u8]) -> Result<()> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index as usize >= self.planes.len() {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let expected = meta.size_x as usize * meta.size_y as usize * self.bpp;
        if data.len() != expected {
            return Err(BioFormatsError::Format(format!(
                "CellH5Writer: plane {plane_index} has {} bytes, expected {expected}",
                data.len()
            )));
        }
        self.planes[plane_index as usize] = Some(data.to_vec());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        let path = match self.path.take() {
            Some(p) => p,
            None => {
                self.meta = None;
                self.planes.clear();
                return Ok(());
            }
        };
        let meta = self.meta.take().ok_or(BioFormatsError::NotInitialized)?;

        let size_x = meta.size_x.max(1) as usize;
        let size_y = meta.size_y.max(1) as usize;
        let size_z = meta.size_z.max(1) as usize;
        let size_c = meta.size_c.max(1) as usize;
        let size_t = meta.size_t.max(1) as usize;
        let plane_pixels = size_x * size_y;

        // Assemble the dense [C, T, Z, Y, X] volume from the buffered planes,
        // placing each incoming plane at the (c, t, z) slice the Java
        // saveBytes() would have written it to.
        let total_pixels = size_c * size_t * size_z * plane_pixels;
        // Strides for [C, T, Z, Y, X].
        let stride_c = size_t * size_z * plane_pixels;
        let stride_t = size_z * plane_pixels;
        let stride_z = plane_pixels;

        let output_path = self.output_path();

        // Build per-type volumes and write. Java writes byte/short/int arrays.
        macro_rules! assemble {
            ($ty:ty, $from_le:expr) => {{
                let mut vol = vec![<$ty>::default(); total_pixels];
                for (no, slot) in self.planes.iter().enumerate() {
                    let buf = slot.as_ref().ok_or_else(|| {
                        BioFormatsError::Format(format!("CellH5Writer: missing plane {no}"))
                    })?;
                    let (c, t, z) = cellh5_ctz_coords(
                        meta.dimension_order,
                        meta.size_c,
                        meta.size_z,
                        meta.size_t,
                        no as u32,
                    )?;
                    let base =
                        c as usize * stride_c + t as usize * stride_t + z as usize * stride_z;
                    let conv = $from_le;
                    for p in 0..plane_pixels {
                        vol[base + p] = conv(buf, p);
                    }
                }
                vol
            }};
        }

        let mut file = WritableFile::create(&path)
            .map_err(|e| BioFormatsError::Format(format!("CellH5Writer: create {path:?}: {e}")))?;

        // /definition/{object,feature,image} groups (Java setId()).
        {
            let mut def = file.create_group("definition").map_err(|e| {
                BioFormatsError::Format(format!("CellH5Writer: create /definition: {e}"))
            })?;
            for sub in ["object", "feature", "image"] {
                def.create_group(sub).map_err(|e| {
                    BioFormatsError::Format(format!("CellH5Writer: create /definition/{sub}: {e}"))
                })?;
            }
        }

        // Build the nested path down to .../position/<site>/image, then write
        // the `channel` dataset into it.
        let shape = [
            size_c as u64,
            size_t as u64,
            size_z as u64,
            size_y as u64,
            size_x as u64,
        ];
        let bpp = self.bpp;

        // Walk/create: sample -> 0 -> plate -> <plate> -> experiment -> <well>
        //   -> position -> <site> -> image, then build the dataset there.
        let mut sample = file
            .create_group("sample")
            .map_err(|e| BioFormatsError::Format(format!("CellH5Writer: create /sample: {e}")))?;
        let mut g0 = sample
            .create_group("0")
            .map_err(|e| BioFormatsError::Format(format!("CellH5Writer: create sample/0: {e}")))?;
        let mut plate_g = g0
            .create_group("plate")
            .map_err(|e| BioFormatsError::Format(format!("CellH5Writer: create plate: {e}")))?;
        let mut plate_named = plate_g.create_group(&self.plate).map_err(|e| {
            BioFormatsError::Format(format!("CellH5Writer: create plate name: {e}"))
        })?;
        let mut experiment = plate_named.create_group("experiment").map_err(|e| {
            BioFormatsError::Format(format!("CellH5Writer: create experiment: {e}"))
        })?;
        let mut well_g = experiment
            .create_group(&self.well)
            .map_err(|e| BioFormatsError::Format(format!("CellH5Writer: create well: {e}")))?;
        let mut position = well_g
            .create_group("position")
            .map_err(|e| BioFormatsError::Format(format!("CellH5Writer: create position: {e}")))?;
        let mut site_g = position
            .create_group(&self.site.to_string())
            .map_err(|e| BioFormatsError::Format(format!("CellH5Writer: create site: {e}")))?;
        let mut image_g = site_g
            .create_group("image")
            .map_err(|e| BioFormatsError::Format(format!("CellH5Writer: create image: {e}")))?;

        let write_res = match bpp {
            1 => {
                let vol = assemble!(u8, |buf: &[u8], p: usize| buf[p]);
                image_g
                    .new_dataset_builder("channel")
                    .shape(&shape)
                    .write::<u8>(&vol)
            }
            2 => {
                // Java uses ByteBuffer.wrap(buf).asShortBuffer(), whose byte
                // order is big-endian unless explicitly changed.
                let vol = assemble!(u16, |buf: &[u8], p: usize| u16::from_be_bytes([
                    buf[p * 2],
                    buf[p * 2 + 1]
                ]));
                image_g
                    .new_dataset_builder("channel")
                    .shape(&shape)
                    .write::<u16>(&vol)
            }
            // bpp is validated to 1/2/4 in set_metadata; 4 is the only remaining case.
            _ => {
                // Java uses ByteBuffer.wrap(buf).asIntBuffer(), also
                // big-endian by default.
                let vol = assemble!(i32, |buf: &[u8], p: usize| i32::from_be_bytes([
                    buf[p * 4],
                    buf[p * 4 + 1],
                    buf[p * 4 + 2],
                    buf[p * 4 + 3]
                ]));
                image_g
                    .new_dataset_builder("channel")
                    .shape(&shape)
                    .write::<i32>(&vol)
            }
        };
        write_res.map_err(|e| {
            BioFormatsError::Format(format!("CellH5Writer: write {output_path}: {e}"))
        })?;

        file.close().map_err(|e| {
            BioFormatsError::Format(format!("CellH5Writer: finalize {path:?}: {e}"))
        })?;

        self.planes.clear();
        Ok(())
    }

    fn can_do_stacks(&self) -> bool {
        true
    }
}

impl FormatReader for CellH5Reader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("ch5"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        // Disabled — rely on extension only to avoid conflicts with other HDF5 readers.
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let series = parse_cellh5(path)?;
        self.series = series;
        self.path = Some(path.to_path_buf());
        self.current_series = 0;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.series.clear();
        self.current_series = 0;
        Ok(())
    }

    fn series_count(&self) -> usize {
        self.series.len()
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if s >= self.series.len() {
            Err(BioFormatsError::SeriesOutOfRange(s))
        } else {
            self.current_series = s;
            Ok(())
        }
    }

    fn series(&self) -> usize {
        self.current_series
    }

    fn metadata(&self) -> &ImageMetadata {
        self.series
            .get(self.current_series)
            .map(|series| &series.meta)
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn resolution_count(&self) -> usize {
        1
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if level != 0 {
            Err(BioFormatsError::Format(format!(
                "CellH5: resolution {level} out of range"
            )))
        } else {
            Ok(())
        }
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let (size_x, size_y) = {
            let meta = &self
                .series
                .get(self.current_series)
                .ok_or(BioFormatsError::NotInitialized)?
                .meta;
            (meta.size_x, meta.size_y)
        };
        self.read_region(plane_index, 0, 0, size_x, size_y)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        self.read_region(plane_index, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = &self
            .series
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?
            .meta;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn ome_metadata(&self) -> Option<OmeMetadata> {
        if self.series.is_empty() {
            return None;
        }

        let mut ome = OmeMetadata::default();
        for series in &self.series {
            let mut image_ome = OmeMetadata::from_image_metadata(&series.meta);
            if let Some(image) = image_ome.images.get_mut(0) {
                image.name = Some(series.image_name.clone());
            }
            ome.images.extend(image_ome.images);
        }

        let first = &self.series[0];
        let (row, column) = cellh5_well_row_column(&first.well);
        let site_index = first.site.parse::<u32>().unwrap_or(0);
        ome.plates.push(OmePlate {
            id: Some(create_lsid("Plate", &[0])),
            name: Some(first.plate.clone()),
            rows: 0,
            columns: 0,
            wells: vec![OmeWell {
                id: Some(create_lsid("Well", &[0])),
                row,
                column,
                well_samples: vec![OmeWellSample {
                    id: Some(create_lsid("WellSample", &[0])),
                    index: site_index,
                    image_ref: Some(0),
                    position_x: None,
                    position_y: None,
                }],
            }],
        });

        let roi_count = match first.meta.series_metadata.get("cellh5_roi_count") {
            Some(MetadataValue::Int(count)) if *count > 0 => *count as usize,
            _ => 0,
        };
        ome.rois.reserve(roi_count);
        for index in 0..roi_count {
            ome.rois.push(OmeROI {
                id: Some(create_lsid("ROI", &[index])),
                name: None,
                shapes: vec![OmeShape::Rectangle {
                    x: 0.0,
                    y: 0.0,
                    width: 0.0,
                    height: 0.0,
                    the_z: Some(0),
                    the_t: Some(0),
                    the_c: Some(0),
                }],
            });
        }

        Some(ome)
    }

    /// Java CellH5Reader.get8BitLookupTable() returns a synthetic colour ramp
    /// for the channel last read by openBytes(). Decode that channel directly
    /// from the requested plane index (XYZTC: Z fastest, then T, then C).
    fn lookup_table(&mut self, plane_index: u32) -> Result<Option<LookupTable>> {
        let meta = &self
            .series
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?
            .meta;
        if !meta.is_indexed || meta.pixel_type != PixelType::Uint8 {
            return Ok(None);
        }
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let sz = meta.size_z.max(1) as usize;
        let st = meta.size_t.max(1) as usize;
        let channel = plane_index as usize / (sz * st);
        Ok(Some(cellh5_8bit_lookup_table(channel)))
    }
}

#[cfg(test)]
mod writer_tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_path(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("bioformats_rs_cellh5_{name}_{nonce}.ch5"))
    }

    fn base_meta() -> ImageMetadata {
        ImageMetadata {
            size_x: 4,
            size_y: 3,
            size_z: 2,
            size_c: 2,
            size_t: 1,
            pixel_type: PixelType::Uint16,
            bits_per_pixel: 16,
            image_count: 4,
            dimension_order: DimensionOrder::XYZTC,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: true,
            resolution_count: 1,
            thumbnail: false,
            series_metadata: Default::default(),
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        }
    }

    #[test]
    fn cellh5_writer_round_trip_uint16() {
        let meta = base_meta();
        let path = temp_path("rt16");

        // Tag each plane's first pixel with the plane index so we can verify
        // placement after the read-back. dimension_order XYZTC: z fastest, then
        // t, then c -> the reader's open_bytes uses the same ordering.
        let plane_pixels = (meta.size_x * meta.size_y) as usize;
        let mut planes: Vec<Vec<u8>> = Vec::new();
        let mut expected_readback: Vec<Vec<u8>> = Vec::new();
        for i in 0..meta.image_count {
            let mut p = Vec::with_capacity(plane_pixels * 2);
            let mut rb = Vec::with_capacity(plane_pixels * 2);
            for px in 0..plane_pixels {
                let v = (i as u16) * 1000 + px as u16;
                p.extend_from_slice(&v.to_be_bytes());
                rb.extend_from_slice(&v.to_le_bytes());
            }
            planes.push(p);
            expected_readback.push(rb);
        }

        let mut w = CellH5Writer::new();
        w.set_metadata(&meta).unwrap();
        w.set_id(&path).unwrap();
        for (i, p) in planes.iter().enumerate() {
            w.save_bytes(i as u32, p).unwrap();
        }
        w.close().unwrap();

        let h5 = hdf5_pure_rust::File::open(&path).unwrap();
        let ds = h5
            .dataset("/sample/0/plate/PLATE_00/experiment/WELL_00/position/1/image/channel")
            .unwrap();
        let stored: Vec<u16> = ds.read::<u16>().unwrap();
        assert_eq!(stored[0], 0);
        assert_eq!(stored[1], 1);
        assert_eq!(stored[plane_pixels], 1000);
        drop(ds);
        drop(h5);

        // Read it back with the CellH5Reader.
        let mut r = CellH5Reader::new();
        r.set_id(&path).unwrap();
        assert_eq!(r.series_count(), 1);
        let m = r.metadata();
        assert_eq!(m.size_x, 4);
        assert_eq!(m.size_y, 3);
        assert_eq!(m.size_z, 2);
        assert_eq!(m.size_c, 2);
        assert_eq!(m.size_t, 1);
        assert_eq!(m.pixel_type, PixelType::Uint16);
        assert_eq!(m.image_count, 4);

        for i in 0..4u32 {
            let got = r.open_bytes(i).unwrap();
            assert_eq!(
                got, expected_readback[i as usize],
                "plane {i} mismatch after round trip"
            );
        }
        r.close().unwrap();
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn cellh5_writer_interprets_int32_input_big_endian_like_java() {
        let mut meta = base_meta();
        meta.pixel_type = PixelType::Int32;
        meta.bits_per_pixel = 32;
        meta.size_x = 2;
        meta.size_y = 1;
        meta.size_z = 1;
        meta.size_c = 1;
        meta.size_t = 1;
        meta.image_count = 1;
        let path = temp_path("writer_i32_be");

        let mut plane = Vec::new();
        plane.extend_from_slice(&(-2i32).to_be_bytes());
        plane.extend_from_slice(&0x0102_0304i32.to_be_bytes());

        let mut w = CellH5Writer::new();
        w.set_metadata(&meta).unwrap();
        w.set_id(&path).unwrap();
        w.save_bytes(0, &plane).unwrap();
        w.close().unwrap();

        let h5 = hdf5_pure_rust::File::open(&path).unwrap();
        let ds = h5
            .dataset("/sample/0/plate/PLATE_00/experiment/WELL_00/position/1/image/channel")
            .unwrap();
        assert_eq!(ds.read::<i32>().unwrap(), vec![-2, 0x0102_0304]);
        drop(ds);
        drop(h5);

        let mut r = CellH5Reader::new();
        r.set_id(&path).unwrap();
        assert_eq!(
            r.open_bytes(0).unwrap(),
            [
                0xfe, 0xff, 0xff, 0xff, //
                0x04, 0x03, 0x02, 0x01,
            ]
        );
        r.close().unwrap();
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn cellh5_writer_round_trip_uint8() {
        let mut meta = base_meta();
        meta.pixel_type = PixelType::Uint8;
        meta.bits_per_pixel = 8;
        meta.size_z = 1;
        meta.size_c = 1;
        meta.size_t = 1;
        meta.image_count = 1;
        let path = temp_path("rt8");

        let plane: Vec<u8> = (0u8..(meta.size_x * meta.size_y) as u8).collect();

        let mut w = CellH5Writer::new();
        w.set_metadata(&meta).unwrap();
        w.set_id(&path).unwrap();
        w.save_bytes(0, &plane).unwrap();
        w.close().unwrap();

        let mut r = CellH5Reader::new();
        r.set_id(&path).unwrap();
        assert_eq!(r.metadata().pixel_type, PixelType::Uint8);
        assert_eq!(r.open_bytes(0).unwrap(), plane);
        r.close().unwrap();
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn cellh5_reader_preserves_signed_int32_pixels_like_java() {
        let path = temp_path("signed_i32");
        let mut file = hdf5_pure_rust::WritableFile::create(&path).unwrap();
        {
            let mut sample = file.create_group("sample").unwrap();
            let mut zero = sample.create_group("0").unwrap();
            let mut plate = zero.create_group("plate").unwrap();
            let mut plate0 = plate.create_group("Plate0").unwrap();
            let mut experiment = plate0.create_group("experiment").unwrap();
            let mut well = experiment.create_group("A01").unwrap();
            let mut positions = well.create_group("position").unwrap();
            let mut site = positions.create_group("1").unwrap();
            let mut image = site.create_group("image").unwrap();
            image
                .new_dataset_builder("channel")
                .shape(&[1, 1, 1, 1, 2])
                .write::<i32>(&[-1i32, 0x0102_0304])
                .unwrap();
        }
        file.flush().unwrap();

        let mut reader = CellH5Reader::new();
        reader.set_id(&path).unwrap();
        assert_eq!(reader.metadata().pixel_type, PixelType::Int32);
        assert_eq!(
            reader.open_bytes(0).unwrap(),
            [
                0xff, 0xff, 0xff, 0xff, //
                0x04, 0x03, 0x02, 0x01,
            ]
        );
        reader.close().unwrap();
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn cellh5_reader_returns_java_uint8_channel_luts() {
        let mut meta = base_meta();
        meta.pixel_type = PixelType::Uint8;
        meta.bits_per_pixel = 8;
        meta.size_z = 1;
        meta.size_c = 7;
        meta.size_t = 1;
        meta.image_count = 7;
        let path = temp_path("uint8_lut");

        let plane = vec![0u8; (meta.size_x * meta.size_y) as usize];
        let mut w = CellH5Writer::new();
        w.set_metadata(&meta).unwrap();
        w.set_id(&path).unwrap();
        for no in 0..meta.image_count {
            w.save_bytes(no, &plane).unwrap();
        }
        w.close().unwrap();

        let mut reader = CellH5Reader::new();
        reader.set_id(&path).unwrap();
        assert!(reader.metadata().is_indexed);
        assert_eq!(reader.metadata().pixel_type, PixelType::Uint8);

        let red = reader.lookup_table(0).unwrap().expect("channel 0 LUT");
        assert_eq!(red.red[255], 255);
        assert_eq!(red.green[255], 0);
        assert_eq!(red.blue[255], 0);

        let green = reader.lookup_table(1).unwrap().expect("channel 1 LUT");
        assert_eq!(green.red[255], 0);
        assert_eq!(green.green[255], 255);
        assert_eq!(green.blue[255], 0);

        let gray = reader.lookup_table(6).unwrap().expect("default LUT");
        assert_eq!(gray.red[255], 255);
        assert_eq!(gray.green[255], 255);
        assert_eq!(gray.blue[255], 255);

        assert!(matches!(
            reader.lookup_table(meta.image_count),
            Err(BioFormatsError::PlaneOutOfRange(_))
        ));
        reader.close().unwrap();
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn cellh5_reader_uint16_indexed_data_has_no_java_lut() {
        let mut meta = base_meta();
        meta.size_z = 1;
        meta.size_c = 1;
        meta.size_t = 1;
        meta.image_count = 1;
        let path = temp_path("uint16_no_lut");

        let plane = vec![0u8; (meta.size_x * meta.size_y * 2) as usize];
        let mut w = CellH5Writer::new();
        w.set_metadata(&meta).unwrap();
        w.set_id(&path).unwrap();
        w.save_bytes(0, &plane).unwrap();
        w.close().unwrap();

        let mut reader = CellH5Reader::new();
        reader.set_id(&path).unwrap();
        assert!(reader.metadata().is_indexed);
        assert_eq!(reader.metadata().pixel_type, PixelType::Uint16);
        assert!(reader.lookup_table(0).unwrap().is_none());
        reader.close().unwrap();
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn cellh5_writer_rejects_unsupported_type() {
        let mut meta = base_meta();
        meta.pixel_type = PixelType::Float32;
        let mut w = CellH5Writer::new();
        assert!(w.set_metadata(&meta).is_err());
    }

    #[test]
    fn cellh5_writer_is_this_type() {
        let w = CellH5Writer::new();
        assert!(w.is_this_type(Path::new("foo.ch5")));
        assert!(w.is_this_type(Path::new("foo.CH5")));
        assert!(!w.is_this_type(Path::new("foo.h5")));
    }
}
