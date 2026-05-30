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
    first_timepoint: u32,
    timepoint_increment: u32,
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
            first_timepoint: 0,
            timepoint_increment: 1,
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

fn parse_bdv(path: &Path) -> Result<(ImageMetadata, usize, u32, u32, u32, u32)> {
    let file = hdf5_pure::File::open(path)
        .map_err(|e| BioFormatsError::Format(format!("HDF5 open error: {e}")))?;

    // ── Try companion XML for authoritative dimensions ───────────────────────
    let xml_path = path.with_extension("xml");
    let mut size_x: u32 = 0;
    let mut size_y: u32 = 0;
    let mut size_z: u32 = 0;
    let mut size_t: u32 = 0;
    let mut size_c: u32 = 0;
    // Timepoint group naming: Java defaults firstTimepoint=0, increment=1, then
    // builds paths as t{firstTimepoint + increment*time}.
    let mut first_timepoint: u32 = 0;
    let mut timepoint_increment: u32 = 1;
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
                first_timepoint = first;
                meta_map.insert(
                    "bdv_timepoint_first".into(),
                    MetadataValue::Int(first as i64),
                );
                meta_map.insert("bdv_timepoint_last".into(), MetadataValue::Int(last as i64));
            }
            // Parse <integerpattern>first-last:increment</integerpattern>.
            // Java parses parts[0] as firstTimepoint and the part after ':' as
            // the timepoint increment (defaulting to 1 when absent).
            if let Some(pat) = xml_find(&xml_str, "integerpattern") {
                let dash: Vec<&str> = pat.splitn(2, '-').collect();
                if let Ok(first) = dash[0].trim().parse::<u32>() {
                    first_timepoint = first;
                    meta_map.insert(
                        "bdv_timepoint_first".into(),
                        MetadataValue::Int(first as i64),
                    );
                }
                if dash.len() > 1 {
                    let colon: Vec<&str> = dash[1].splitn(2, ':').collect();
                    if colon.len() > 1 {
                        if let Ok(inc) = colon[1].trim().parse::<u32>() {
                            if inc > 0 {
                                timepoint_increment = inc;
                                meta_map.insert(
                                    "bdv_timepoint_increment".into(),
                                    MetadataValue::Int(inc as i64),
                                );
                            }
                        }
                    }
                }
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

    // The first timepoint's HDF5 group is named for firstTimepoint (BDV uses
    // t{firstTimepoint + increment*time}), so init probes must use it — not a
    // literal t00000 — to stay consistent with the open_bytes read path.
    let first_t_group = format!("t{first_timepoint:05}");
    let first_cells_path = format!("{first_t_group}/s00/0/cells");

    if size_c == 0 {
        // Count setup groups under the first timepoint group
        if let Ok(t0) = file.group(&first_t_group) {
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
        // Infer from shape of the first timepoint's cells dataset
        let ds = file.dataset(&first_cells_path).map_err(|e| {
            BioFormatsError::UnsupportedFormat(format!(
                "BDV: missing {first_cells_path} for dimension inference: {e}"
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
    let (pixel_type, bytes_per_sample) =
        validate_bdv_cells_dataset(&file, &first_cells_path, size_x, size_y, size_z)?;

    // ── Count resolution levels from s00/resolutions ────────────────────────
    let n_resolutions: usize = if let Ok(ds) = file.dataset("s00/resolutions") {
        let shape = ds.shape().unwrap_or_default();
        if !shape.is_empty() && shape[0] > 0 {
            shape[0] as usize
        } else {
            1
        }
    } else {
        // Fall back: count integer-named children of <first timepoint>/s00
        if let Ok(g) = file.group(&format!("{first_t_group}/s00")) {
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
        pixel_type,
        bits_per_pixel: (bytes_per_sample * 8) as u8,
        image_count,
        dimension_order: DimensionOrder::XYZTC,
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

    Ok((
        meta,
        n_resolutions,
        size_t,
        size_c,
        first_timepoint,
        timepoint_increment,
    ))
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

/// Validates the cells dataset shape and derives the pixel type from its HDF5
/// dtype size. Java BDVReader.java:571-579 maps element size to pixel type:
/// 1 → UINT8, 2 → UINT16, 4 → INT32 (signed). Returns (pixel_type, bytes_per_sample).
fn validate_bdv_cells_dataset(
    file: &hdf5_pure::File,
    path: &str,
    size_x: u32,
    size_y: u32,
    size_z: u32,
) -> Result<(PixelType, usize)> {
    let ds = file
        .dataset(path)
        .map_err(|e| BioFormatsError::UnsupportedFormat(format!("BDV: missing {path}: {e}")))?;
    let dtype_size = ds
        .dtype()
        .map(hdf5_dtype_size)
        .map_err(|e| BioFormatsError::Format(format!("BDV: cannot read dtype for {path}: {e}")))?;
    let (pixel_type, bytes_per_sample) = match dtype_size {
        1 => (PixelType::Uint8, 1usize),
        2 => (PixelType::Uint16, 2usize),
        4 => (PixelType::Int32, 4usize),
        other => {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "BDV: unsupported cells dtype size {other} for {path}"
            )));
        }
    };
    let shape = ds
        .shape()
        .map_err(|e| BioFormatsError::Format(format!("BDV: cannot read shape for {path}: {e}")))?;
    let declared = [size_z as u64, size_y as u64, size_x as u64];
    if shape != declared {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "BDV: {path} shape {shape:?} does not match declared {declared:?}"
        )));
    }
    Ok((pixel_type, bytes_per_sample))
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
        let (meta, n_res, size_t, size_c, first_timepoint, timepoint_increment) =
            parse_bdv(path)?;
        self.meta = Some(meta);
        self.path = Some(path.to_path_buf());
        self.n_resolutions = n_res;
        self.current_resolution = 0;
        self.size_t = size_t;
        self.size_c = size_c;
        self.first_timepoint = first_timepoint;
        self.timepoint_increment = timepoint_increment;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.n_resolutions = 0;
        self.current_resolution = 0;
        self.first_timepoint = 0;
        self.timepoint_increment = 1;
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

        // Dimension order is XYZTC: Z varies fastest, then T, then C.
        let sz = meta.size_z as usize;
        let st = meta.size_t as usize;
        let z = (plane_index as usize) % sz;
        let t = (plane_index as usize / sz) % st;
        let c = (plane_index as usize) / (sz * st);

        // Map the 0-based timepoint index onto the HDF5 group index using the
        // companion XML's first/increment (Java: firstTimepoint + increment*time).
        let group_t = self.first_timepoint as usize + self.timepoint_increment as usize * t;

        let res = self.current_resolution;
        let ds_path = format!("t{group_t:05}/s{c:02}/{res}/cells");

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

        // Read width-aware: the cells dtype may be 1/2/4 bytes (Uint8/Uint16/
        // Int32), so dispatch on the resolved pixel type rather than assuming
        // uint16. Each branch yields the raw little-endian sample bytes.
        let bps = meta.pixel_type.bytes_per_sample() as usize;
        let plane_pixels = meta.size_x as usize * meta.size_y as usize;
        let plane_bytes = plane_pixels * bps;

        let raw: Vec<u8> = match bps {
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
                let words: Vec<u32> = ds
                    .read_u32()
                    .map_err(|e| BioFormatsError::Format(format!("HDF5 read: {e}")))?;
                words.iter().flat_map(|w| w.to_le_bytes()).collect()
            }
            other => {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "BDV unsupported bytes-per-sample {other}"
                )))
            }
        };

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
