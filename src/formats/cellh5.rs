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

const CELLH5_METADATA_NODE_LIMIT: usize = 512;

pub struct CellH5Reader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    /// HDF5 dataset paths to per-channel image data.
    channel_paths: Vec<String>,
}

impl CellH5Reader {
    pub fn new() -> Self {
        CellH5Reader {
            path: None,
            meta: None,
            channel_paths: Vec::new(),
        }
    }
}

impl Default for CellH5Reader {
    fn default() -> Self {
        Self::new()
    }
}

/// Walk known CellH5 layout patterns and collect leaf channel dataset paths.
fn find_image_datasets(file: &hdf5_pure::File) -> Vec<String> {
    let mut paths = Vec::new();

    for root in &["sample", "plate"] {
        let root_g = match file.group(root) {
            Ok(g) => g,
            Err(_) => continue,
        };
        let plates = match hdf5_group_members(&root_g) {
            Ok(m) => m,
            Err(_) => continue,
        };
        for plate in &plates {
            for mid in &["position", "experiment"] {
                let mid_path = format!("{root}/{plate}/{mid}");
                let mid_g = match file.group(&mid_path) {
                    Ok(g) => g,
                    Err(_) => continue,
                };
                let wells = match hdf5_group_members(&mid_g) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                for well in &wells {
                    let ch_path = format!("{mid_path}/{well}/image/channel");
                    let ch_g = match file.group(&ch_path) {
                        Ok(g) => g,
                        Err(_) => continue,
                    };
                    let chs = match hdf5_group_members(&ch_g) {
                        Ok(m) => m,
                        Err(_) => continue,
                    };
                    for ch in &chs {
                        paths.push(format!("{ch_path}/{ch}"));
                    }
                }
            }
        }
    }

    paths
}

fn hdf5_attr_value(attr: &hdf5_pure::AttrValue) -> MetadataValue {
    match attr {
        hdf5_pure::AttrValue::F64(v) => MetadataValue::Float(*v),
        hdf5_pure::AttrValue::F64Array(v) => MetadataValue::String(format!("{v:?}")),
        hdf5_pure::AttrValue::I32(v) => MetadataValue::Int(*v as i64),
        hdf5_pure::AttrValue::I64(v) => MetadataValue::Int(*v),
        hdf5_pure::AttrValue::I64Array(v) => MetadataValue::String(format!("{v:?}")),
        hdf5_pure::AttrValue::U32(v) => MetadataValue::Int(*v as i64),
        hdf5_pure::AttrValue::U64(v) => MetadataValue::Int((*v).min(i64::MAX as u64) as i64),
        hdf5_pure::AttrValue::String(s) | hdf5_pure::AttrValue::AsciiString(s) => {
            MetadataValue::String(s.trim_matches('\0').trim().to_string())
        }
        hdf5_pure::AttrValue::StringArray(v)
        | hdf5_pure::AttrValue::AsciiStringArray(v)
        | hdf5_pure::AttrValue::VarLenAsciiArray(v) => MetadataValue::String(v.join(",")),
    }
}

fn collect_group_attrs(
    group: &hdf5_pure::Group<'_>,
    path: &str,
    meta_map: &mut HashMap<String, MetadataValue>,
) {
    if let Ok(attrs) = group.attrs() {
        for (name, attr) in attrs {
            meta_map.insert(format!("cellh5_attr:{path}@{name}"), hdf5_attr_value(&attr));
        }
    }
}

fn collect_dataset_metadata(
    dataset: &hdf5_pure::Dataset<'_>,
    path: &str,
    meta_map: &mut HashMap<String, MetadataValue>,
) {
    let dtype_size = dataset.dtype().map(hdf5_dtype_size).unwrap_or(0);
    let shape = dataset.shape().unwrap_or_default();
    meta_map.insert(
        format!("cellh5_dataset:{path}"),
        MetadataValue::String(format!("shape={:?}; dtype_size={dtype_size}", shape)),
    );

    if let Ok(attrs) = dataset.attrs() {
        for (name, attr) in attrs {
            meta_map.insert(format!("cellh5_attr:{path}@{name}"), hdf5_attr_value(&attr));
        }
    }
}

fn collect_file_attrs(file: &hdf5_pure::File, meta_map: &mut HashMap<String, MetadataValue>) {
    if let Ok(attrs) = file.root().attrs() {
        for (name, attr) in attrs {
            meta_map.insert(format!("cellh5_attr:/@{name}"), hdf5_attr_value(&attr));
        }
    }
}

fn collect_hdf5_metadata(
    file: &hdf5_pure::File,
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

fn parse_cellh5(path: &Path) -> Result<(ImageMetadata, Vec<String>)> {
    let file = hdf5_pure::File::open(path)
        .map_err(|e| BioFormatsError::Format(format!("HDF5 open error: {e}")))?;

    let channel_paths = find_image_datasets(&file);

    let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
    meta_map.insert("format".into(), MetadataValue::String("CellH5".into()));
    collect_file_attrs(&file, &mut meta_map);
    collect_hdf5_metadata(&file, "/", &mut meta_map, &mut 0);

    if channel_paths.is_empty() {
        return Err(BioFormatsError::UnsupportedFormat(
            "CellH5: no image datasets found in supported sample/plate channel layouts".into(),
        ));
    }

    // Inspect the first channel dataset to get dimensions
    let ds = file
        .dataset(&channel_paths[0])
        .map_err(|e| BioFormatsError::Format(format!("dataset {}: {e}", channel_paths[0])))?;

    let shape = ds.shape().unwrap_or_default();
    let (size_x, size_y, size_z, size_t) = match shape.len() {
        3 => {
            // [n_frames, y, x]
            let nt = shape[0] as u32;
            let sy = shape[1] as u32;
            let sx = shape[2] as u32;
            (sx, sy, 1u32, nt)
        }
        2 => {
            // [y, x]
            let sy = shape[0] as u32;
            let sx = shape[1] as u32;
            (sx, sy, 1u32, 1u32)
        }
        _ => {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "CellH5: unsupported dataset rank {} for {}",
                shape.len(),
                channel_paths[0]
            )));
        }
    };

    // Determine pixel type from dataset dtype size
    let pixel_type = match ds.dtype().map(hdf5_dtype_size).unwrap_or(2) {
        1 => PixelType::Uint8,
        4 => PixelType::Uint32,
        _ => PixelType::Uint16,
    };
    let bytes_per_sample: usize = match pixel_type {
        PixelType::Uint8 => 1,
        PixelType::Uint32 => 4,
        _ => 2,
    };

    let size_c = channel_paths.len() as u32;
    let image_count = size_z * size_c * size_t;

    let meta = ImageMetadata {
        size_x,
        size_y,
        size_z,
        size_c,
        size_t,
        pixel_type,
        bits_per_pixel: (bytes_per_sample * 8) as u8,
        image_count,
        dimension_order: DimensionOrder::XYZCT,
        is_rgb: false,
        is_interleaved: false,
        is_indexed: false,
        is_little_endian: true,
        resolution_count: 1,
        series_metadata: meta_map,
        lookup_table: None,
        modulo_z: None,
        modulo_c: None,
        modulo_t: None,
    };

    Ok((meta, channel_paths))
}

fn hdf5_group_members(
    group: &hdf5_pure::Group<'_>,
) -> std::result::Result<Vec<String>, hdf5_pure::Error> {
    let mut members = group.groups()?;
    members.extend(group.datasets()?);
    Ok(members)
}

fn hdf5_dtype_size(dtype: hdf5_pure::DType) -> usize {
    match dtype {
        hdf5_pure::DType::F32
        | hdf5_pure::DType::I32
        | hdf5_pure::DType::U32
        | hdf5_pure::DType::Enum(_) => 4,
        hdf5_pure::DType::F64
        | hdf5_pure::DType::I64
        | hdf5_pure::DType::U64
        | hdf5_pure::DType::ObjectReference => 8,
        hdf5_pure::DType::I16 | hdf5_pure::DType::U16 => 2,
        hdf5_pure::DType::I8 | hdf5_pure::DType::U8 => 1,
        hdf5_pure::DType::Array(base, dims) => {
            hdf5_dtype_size(*base) * dims.iter().copied().product::<u32>() as usize
        }
        _ => 0,
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
        let (meta, channel_paths) = parse_cellh5(path)?;
        self.meta = Some(meta);
        self.path = Some(path.to_path_buf());
        self.channel_paths = channel_paths;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.channel_paths.clear();
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
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }

        let plane_pixels = meta.size_x as usize * meta.size_y as usize;
        let bytes_per_sample = (meta.bits_per_pixel / 8) as usize;
        let plane_bytes = plane_pixels * bytes_per_sample;

        let sz = meta.size_z as usize;
        let sc = meta.size_c as usize;
        let z = (plane_index as usize) % sz;
        let c = (plane_index as usize / sz) % sc;
        let t = (plane_index as usize) / (sz * sc);

        let ds_path = self.channel_paths[c].clone();
        let path = self
            .path
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?
            .clone();
        let file = hdf5_pure::File::open(&path)
            .map_err(|e| BioFormatsError::Format(format!("HDF5: {e}")))?;
        let ds = file
            .dataset(&ds_path)
            .map_err(|e| BioFormatsError::Format(format!("dataset {ds_path}: {e}")))?;

        let raw: Vec<u8> = match bytes_per_sample {
            1 => ds
                .read_u8()
                .map_err(|e| BioFormatsError::Format(format!("HDF5 read: {e}")))?,
            2 => {
                let words: Vec<u16> = ds
                    .read_u16()
                    .map_err(|e| BioFormatsError::Format(format!("HDF5 read: {e}")))?;
                words.iter().flat_map(|w| w.to_le_bytes()).collect()
            }
            4 => {
                let dwords: Vec<u32> = ds
                    .read_u32()
                    .map_err(|e| BioFormatsError::Format(format!("HDF5 read: {e}")))?;
                dwords.iter().flat_map(|d| d.to_le_bytes()).collect()
            }
            _ => ds
                .read_u8()
                .map_err(|e| BioFormatsError::Format(format!("HDF5 read: {e}")))?,
        };

        // raw layout: [t, z, y, x] → [t, y, x] when sz==1, or [y, x]
        // Offset for frame t, z-plane z:
        let frame_bytes = meta.size_z as usize * plane_bytes;
        let offset = t * frame_bytes + z * plane_bytes;

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
        let meta = self.meta.as_ref().unwrap();
        let bps = (meta.bits_per_pixel / 8) as usize;
        let row_bytes = meta.size_x as usize * bps;
        let out_row = w as usize * bps;
        let mut out = Vec::with_capacity(h as usize * out_row);
        for r in 0..h as usize {
            let src_start = (y as usize + r) * row_bytes + x as usize * bps;
            if src_start + out_row <= full.len() {
                out.extend_from_slice(&full[src_start..src_start + out_row]);
            } else {
                out.extend(std::iter::repeat(0u8).take(out_row));
            }
        }
        Ok(out)
    }

    fn open_thumb_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(0, tx, ty, tw, th)
    }
}
