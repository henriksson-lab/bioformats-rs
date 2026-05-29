//! BigDataViewer (BDV) HDF5 format reader.
//!
//! Reads `.h5` files produced by the BigDataViewer Fiji plugin for light-sheet
//! microscopy data.  Multi-setup, multi-timepoint, multi-resolution volumes.
//!
//! HDF5 group layout:
//!   t{T:05}/s{C:02}/{level}/cells  — uint16 [z, y, x]
//!   s{C:02}/resolutions            — float64 [n_levels, 3]
//!   s{C:02}/subdivisions           — int32   [n_levels, 3]
//!
//! Optional companion XML carries size and timepoint-range metadata.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;

pub struct BdvReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    n_resolutions: usize,
    current_resolution: usize,
    size_t: u32,
    size_c: u32,
}

impl BdvReader {
    pub fn new() -> Self {
        BdvReader {
            path: None,
            meta: None,
            n_resolutions: 0,
            current_resolution: 0,
            size_t: 1,
            size_c: 1,
        }
    }
}

impl Default for BdvReader {
    fn default() -> Self {
        Self::new()
    }
}

/// Minimal tag-search helper — no full XML parse needed.
fn xml_find(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{}>", tag);
    let close = format!("</{}>", tag);
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)?;
    Some(xml[start..start + end].trim().to_string())
}

/// Count occurrences of an opening tag in the XML string.
fn xml_count(xml: &str, tag: &str) -> usize {
    let open = format!("<{}>", tag);
    let mut count = 0;
    let mut pos = 0;
    while let Some(idx) = xml[pos..].find(&open) {
        count += 1;
        pos += idx + open.len();
    }
    count
}

fn parse_bdv(path: &Path) -> Result<(ImageMetadata, usize, u32, u32)> {
    let file = hdf5_pure::File::open(path)
        .map_err(|e| BioFormatsError::Format(format!("HDF5 open error: {e}")))?;

    // ── Try companion XML for authoritative dimensions ───────────────────────
    let xml_path = path.with_extension("xml");
    let mut size_x: u32 = 0;
    let mut size_y: u32 = 0;
    let mut size_z: u32 = 0;
    let mut size_t: u32 = 0;
    let mut size_c: u32 = 0;
    let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
    meta_map.insert(
        "format".into(),
        MetadataValue::String("BigDataViewer HDF5".into()),
    );

    if xml_path.exists() {
        if let Ok(xml_str) = std::fs::read_to_string(&xml_path) {
            meta_map.insert(
                "bdv_xml_path".into(),
                MetadataValue::String(xml_path.display().to_string()),
            );
            meta_map.insert("bdv_xml".into(), MetadataValue::String(xml_str.clone()));
            // Parse <size>X Y Z</size>
            if let Some(size_str) = xml_find(&xml_str, "size") {
                let parts: Vec<u32> = size_str
                    .split_whitespace()
                    .filter_map(|s| s.parse().ok())
                    .collect();
                if parts.len() >= 3 {
                    if parts[0] == 0 || parts[1] == 0 || parts[2] == 0 {
                        return Err(BioFormatsError::UnsupportedFormat(
                            "BDV XML has non-positive size axis".into(),
                        ));
                    }
                    size_x = parts[0];
                    size_y = parts[1];
                    size_z = parts[2];
                    meta_map.insert("bdv_size".into(), MetadataValue::String(size_str));
                }
            }
            // Parse timepoint range: <first>N</first> ... <last>M</last>
            if let (Some(first_str), Some(last_str)) =
                (xml_find(&xml_str, "first"), xml_find(&xml_str, "last"))
            {
                let first: u32 = first_str.parse().map_err(|_| {
                    BioFormatsError::UnsupportedFormat(format!(
                        "BDV XML has invalid first timepoint {first_str:?}"
                    ))
                })?;
                let last: u32 = last_str.parse().map_err(|_| {
                    BioFormatsError::UnsupportedFormat(format!(
                        "BDV XML has invalid last timepoint {last_str:?}"
                    ))
                })?;
                if last < first {
                    return Err(BioFormatsError::UnsupportedFormat(format!(
                        "BDV XML last timepoint {last} precedes first {first}"
                    )));
                }
                size_t = last - first + 1;
                meta_map.insert(
                    "bdv_timepoint_first".into(),
                    MetadataValue::Int(first as i64),
                );
                meta_map.insert("bdv_timepoint_last".into(), MetadataValue::Int(last as i64));
            }
            // Count ViewSetup elements
            let vc = xml_count(&xml_str, "ViewSetup");
            if vc > 0 {
                size_c = vc as u32;
                meta_map.insert("bdv_view_setup_count".into(), MetadataValue::Int(vc as i64));
            }
        }
    }

    // ── Fall back to HDF5 introspection if XML didn't provide everything ─────
    if size_t == 0 {
        // Count top-level groups matching t\d{5}
        if let Ok(root_members) = hdf5_members(&file, "/") {
            size_t = root_members
                .iter()
                .filter(|n| {
                    n.len() == 6 && n.starts_with('t') && n[1..].chars().all(|c| c.is_ascii_digit())
                })
                .count() as u32;
        }
        if size_t == 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "BDV: no timepoint groups found".into(),
            ));
        }
    }

    if size_c == 0 {
        // Count setup groups under t00000
        if let Ok(t0) = file.group("t00000") {
            if let Ok(members) = hdf5_group_members(&t0) {
                size_c = members
                    .iter()
                    .filter(|n| {
                        n.len() == 3
                            && n.starts_with('s')
                            && n[1..].chars().all(|c| c.is_ascii_digit())
                    })
                    .count() as u32;
            }
        }
        if size_c == 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "BDV: no setup groups found under t00000".into(),
            ));
        }
    }

    if size_x == 0 || size_y == 0 || size_z == 0 {
        // Infer from shape of t00000/s00/0/cells
        let ds = file.dataset("t00000/s00/0/cells").map_err(|e| {
            BioFormatsError::UnsupportedFormat(format!(
                "BDV: missing t00000/s00/0/cells for dimension inference: {e}"
            ))
        })?;
        let shape = ds.shape().map_err(|e| {
            BioFormatsError::Format(format!("BDV: cannot read cells dataset shape: {e}"))
        })?;
        if shape.len() != 3 || shape[0] == 0 || shape[1] == 0 || shape[2] == 0 {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "BDV: unsupported cells shape {shape:?}"
            )));
        }
        size_z = u32::try_from(shape[0])
            .map_err(|_| BioFormatsError::Format("BDV Z dimension overflows".into()))?;
        size_y = u32::try_from(shape[1])
            .map_err(|_| BioFormatsError::Format("BDV Y dimension overflows".into()))?;
        size_x = u32::try_from(shape[2])
            .map_err(|_| BioFormatsError::Format("BDV X dimension overflows".into()))?;
    }
    validate_bdv_cells_dataset(&file, "t00000/s00/0/cells", size_x, size_y, size_z)?;

    // ── Count resolution levels from s00/resolutions ────────────────────────
    let n_resolutions: usize = if let Ok(ds) = file.dataset("s00/resolutions") {
        let shape = ds.shape().unwrap_or_default();
        if !shape.is_empty() && shape[0] > 0 {
            shape[0] as usize
        } else {
            1
        }
    } else {
        // Fall back: count integer-named children of t00000/s00
        if let Ok(g) = file.group("t00000/s00") {
            if let Ok(members) = hdf5_group_members(&g) {
                let n = members
                    .iter()
                    .filter(|n| n.parse::<usize>().is_ok())
                    .count();
                n
            } else {
                0
            }
        } else {
            0
        }
    };
    if n_resolutions == 0 {
        return Err(BioFormatsError::UnsupportedFormat(
            "BDV: no resolution levels found".into(),
        ));
    }

    let image_count = size_z
        .checked_mul(size_c)
        .and_then(|v| v.checked_mul(size_t))
        .ok_or_else(|| BioFormatsError::Format("BDV image count overflows".into()))?;
    let meta = ImageMetadata {
        size_x,
        size_y,
        size_z,
        size_c,
        size_t,
        pixel_type: PixelType::Uint16,
        bits_per_pixel: 16,
        image_count,
        dimension_order: DimensionOrder::XYZCT,
        is_rgb: false,
        is_interleaved: false,
        is_indexed: false,
        is_little_endian: true,
        resolution_count: n_resolutions as u32,
        series_metadata: meta_map,
        lookup_table: None,
        modulo_z: None,
        modulo_c: None,
        modulo_t: None,
    };

    Ok((meta, n_resolutions, size_t, size_c))
}

fn hdf5_group_members(
    group: &hdf5_pure::Group<'_>,
) -> std::result::Result<Vec<String>, hdf5_pure::Error> {
    let mut members = group.groups()?;
    members.extend(group.datasets()?);
    Ok(members)
}

fn hdf5_members(
    file: &hdf5_pure::File,
    path: &str,
) -> std::result::Result<Vec<String>, hdf5_pure::Error> {
    if path == "/" {
        hdf5_group_members(&file.root())
    } else {
        hdf5_group_members(&file.group(path)?)
    }
}

fn hdf5_dtype_size(dtype: hdf5_pure::DType) -> usize {
    match dtype {
        hdf5_pure::DType::I16 | hdf5_pure::DType::U16 => 2,
        hdf5_pure::DType::I8 | hdf5_pure::DType::U8 => 1,
        hdf5_pure::DType::F32
        | hdf5_pure::DType::I32
        | hdf5_pure::DType::U32
        | hdf5_pure::DType::Enum(_) => 4,
        hdf5_pure::DType::F64
        | hdf5_pure::DType::I64
        | hdf5_pure::DType::U64
        | hdf5_pure::DType::ObjectReference => 8,
        hdf5_pure::DType::Array(base, dims) => {
            hdf5_dtype_size(*base) * dims.iter().copied().product::<u32>() as usize
        }
        _ => 0,
    }
}

fn validate_bdv_cells_dataset(
    file: &hdf5_pure::File,
    path: &str,
    size_x: u32,
    size_y: u32,
    size_z: u32,
) -> Result<()> {
    let ds = file
        .dataset(path)
        .map_err(|e| BioFormatsError::UnsupportedFormat(format!("BDV: missing {path}: {e}")))?;
    let dtype_size = ds
        .dtype()
        .map(hdf5_dtype_size)
        .map_err(|e| BioFormatsError::Format(format!("BDV: cannot read dtype for {path}: {e}")))?;
    if dtype_size != 2 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "BDV: unsupported cells dtype size {dtype_size} for {path}"
        )));
    }
    let shape = ds
        .shape()
        .map_err(|e| BioFormatsError::Format(format!("BDV: cannot read shape for {path}: {e}")))?;
    let declared = [size_z as u64, size_y as u64, size_x as u64];
    if shape != declared {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "BDV: {path} shape {shape:?} does not match declared {declared:?}"
        )));
    }
    Ok(())
}

impl FormatReader for BdvReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("h5"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        // Intentionally false — avoid conflict with ImarisReader which uses HDF5
        // magic bytes; rely on extension detection only.
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let (meta, n_res, size_t, size_c) = parse_bdv(path)?;
        self.meta = Some(meta);
        self.path = Some(path.to_path_buf());
        self.n_resolutions = n_res;
        self.current_resolution = 0;
        self.size_t = size_t;
        self.size_c = size_c;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.n_resolutions = 0;
        self.current_resolution = 0;
        Ok(())
    }

    fn series_count(&self) -> usize {
        usize::from(self.meta.is_some())
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.meta.is_none() {
            return Err(BioFormatsError::NotInitialized);
        }
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
        self.meta
            .as_ref()
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn resolution_count(&self) -> usize {
        self.n_resolutions
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if self.meta.is_none() {
            return Err(BioFormatsError::NotInitialized);
        }
        if level >= self.n_resolutions {
            return Err(BioFormatsError::Format(format!(
                "resolution {level} out of range (max {})",
                self.n_resolutions - 1
            )));
        }
        self.current_resolution = level;
        Ok(())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }

        let sz = meta.size_z as usize;
        let sc = meta.size_c as usize;
        let z = (plane_index as usize) % sz;
        let c = (plane_index as usize / sz) % sc;
        let t = (plane_index as usize) / (sz * sc);

        let res = self.current_resolution;
        let ds_path = format!("t{t:05}/s{c:02}/{res}/cells");

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

        let plane_pixels = meta.size_x as usize * meta.size_y as usize;
        let plane_bytes = plane_pixels * 2; // uint16

        let words: Vec<u16> = ds
            .read_u16()
            .map_err(|e| BioFormatsError::Format(format!("HDF5 read: {e}")))?;
        let raw: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();

        let offset = z * plane_bytes;
        if offset + plane_bytes <= raw.len() {
            Ok(raw[offset..offset + plane_bytes].to_vec())
        } else {
            Err(BioFormatsError::UnsupportedFormat(format!(
                "BDV dataset {ds_path} is shorter than declared plane {plane_index} \
                 (need {} bytes, have {})",
                offset + plane_bytes,
                raw.len()
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
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        crop_full_plane("BDV", &full, meta, 1, x, y, w, h)
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
