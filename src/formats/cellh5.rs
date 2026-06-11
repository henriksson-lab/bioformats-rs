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
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;

use hdf5_pure_rust::format::messages::datatype::DatatypeClass;
use hdf5_pure_rust::{HyperslabDim, Selection};

const CELLH5_METADATA_NODE_LIMIT: usize = 512;

/// One CellH5 series: its image-data dataset path and parsed dimensions.
struct CellH5Series {
    /// HDF5 path to the 5D `[c, t, z, y, x]` dataset for this series.
    dataset_path: String,
    meta: ImageMetadata,
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

    let mut series_list = Vec::with_capacity(dataset_paths.len());
    for ds_path in dataset_paths {
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
            is_indexed: false,
            is_little_endian: true,
            resolution_count: 1,
            series_metadata: sm,
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        };

        series_list.push(CellH5Series {
            dataset_path: ds_path,
            meta,
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
        let series = self
            .series
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        let meta = &series.meta;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }

        let plane_pixels = meta.size_x as usize * meta.size_y as usize;
        let bytes_per_sample = (meta.bits_per_pixel / 8) as usize;
        let plane_bytes = plane_pixels * bytes_per_sample;

        // dimension_order XYZTC: z varies fastest, then t, then c.
        let sz = meta.size_z as usize;
        let st = meta.size_t as usize;
        let z = (plane_index as usize) % sz;
        let t = (plane_index as usize / sz) % st;
        let c = (plane_index as usize) / (sz * st);

        // Single 5D dataset `[c, t, z, y, x]` for this series/well/position.
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

        // Per-plane partial read via a hyperslab selection: fix the leading
        // dims (c, t, z) to the requested indices and select all of y,x.
        // Storage is `[c, t, z, y, x]` for 5D; lower-rank datasets drop the
        // leading dims in the same order dims_from_shape() assumes.
        let shape = ds.shape().unwrap_or_default();
        let y = meta.size_y as u64;
        let xw = meta.size_x as u64;
        let dims: Vec<HyperslabDim> = match shape.len() {
            5 => vec![
                HyperslabDim::new(c as u64, 1, 1, 1),
                HyperslabDim::new(t as u64, 1, 1, 1),
                HyperslabDim::new(z as u64, 1, 1, 1),
                HyperslabDim::new(0, 1, y, 1),
                HyperslabDim::new(0, 1, xw, 1),
            ],
            4 => vec![
                HyperslabDim::new(c as u64, 1, 1, 1),
                HyperslabDim::new(t as u64, 1, 1, 1),
                HyperslabDim::new(0, 1, y, 1),
                HyperslabDim::new(0, 1, xw, 1),
            ],
            3 => vec![
                HyperslabDim::new(t as u64, 1, 1, 1),
                HyperslabDim::new(0, 1, y, 1),
                HyperslabDim::new(0, 1, xw, 1),
            ],
            2 => vec![
                HyperslabDim::new(0, 1, y, 1),
                HyperslabDim::new(0, 1, xw, 1),
            ],
            other => {
                return Err(BioFormatsError::Format(format!(
                    "CellH5: dataset {ds_path} has unsupported rank {other}"
                )));
            }
        };
        let selection = Selection::Hyperslab(dims);

        // After read_slice the returned vec IS this plane; index from 0.
        let raw: Vec<u8> = match bytes_per_sample {
            1 => ds
                .read_slice::<u8, _>(selection)
                .map_err(|e| BioFormatsError::Format(format!("HDF5 read: {e}")))?,
            2 => {
                let words: Vec<u16> = ds
                    .read_slice::<u16, _>(selection)
                    .map_err(|e| BioFormatsError::Format(format!("HDF5 read: {e}")))?;
                words.iter().flat_map(|w| w.to_le_bytes()).collect()
            }
            4 => {
                let dwords: Vec<u32> = ds
                    .read_slice::<u32, _>(selection)
                    .map_err(|e| BioFormatsError::Format(format!("HDF5 read: {e}")))?;
                dwords.iter().flat_map(|d| d.to_le_bytes()).collect()
            }
            _ => ds
                .read_slice::<u8, _>(selection)
                .map_err(|e| BioFormatsError::Format(format!("HDF5 read: {e}")))?,
        };

        // The hyperslab read already isolates the requested plane, so the
        // buffer should be exactly one plane long; index from offset 0.
        let offset = 0usize;

        if offset + plane_bytes <= raw.len() {
            Ok(raw[offset..offset + plane_bytes].to_vec())
        } else {
            Err(BioFormatsError::Format(format!(
                "CellH5: dataset {ds_path} is too short for plane {plane_index}"
            )))
        }
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
        let meta = &self
            .series
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?
            .meta;
        crop_full_plane("CellH5", &full, meta, 1, x, y, w, h)
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
}
