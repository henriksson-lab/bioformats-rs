//! BigDataViewer (BDV) HDF5 format reader.
//!
//! Reads `.h5` files produced by the BigDataViewer Fiji plugin for light-sheet
//! microscopy data.  Multi-setup, multi-timepoint, multi-resolution volumes.
//!
//! HDF5 group layout:
//!   t{T:05}/s{S:02}/{level}/cells  — uint16 [z, y, x]
//!   s{S:02}/resolutions            — float64 [n_levels, 3]
//!   s{S:02}/subdivisions           — int32   [n_levels, 3]
//!
//! Companion XML (SpimData) carries the ViewSetups (sizes + voxel sizes) and
//! the Timepoints range.
//!
//! ## Series model (Java parity)
//!
//! Java Bio-Formats' BDVReader flattens every `(ViewSetup × Timepoint ×
//! resolution-level)` combination into a *separate series*. Each such series is
//! a single-channel single-timepoint 3-D volume read from
//! `t{timepoint}/s{setup}/{level}/cells`, with `sizeC = sizeT = 1`,
//! `sizeZ = depth`, `imageCount = sizeZ`. The series iteration order is
//! setup-outer, timepoint-middle, level-inner. Image names follow the pattern
//! `P_t{timepoint:05}, W_s{setup:02}_{level}`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::ome_metadata::{OmeChannel, OmeImage, OmeMetadata};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;

use hdf5_pure_rust::{HyperslabDim, Selection};

/// One BDV series: a single resolution level of one setup at one timepoint.
#[derive(Clone)]
struct SeriesInfo {
    /// Setup index (the `sNN` group number).
    setup: u32,
    /// Timepoint value (the `tNNNNN` group number, e.g. 18).
    timepoint: u32,
    /// Resolution level (the `{level}` group number).
    level: u32,
    /// Core metadata for this series.
    meta: ImageMetadata,
    /// Physical pixel sizes (micrometres) from the setup's voxelSize.
    voxel_size: Option<(f64, f64, f64)>,
}

pub struct BdvReader {
    path: Option<PathBuf>,
    series: Vec<SeriesInfo>,
    current_series: usize,
}

impl BdvReader {
    pub fn new() -> Self {
        BdvReader {
            path: None,
            series: Vec::new(),
            current_series: 0,
        }
    }
}

impl Default for BdvReader {
    fn default() -> Self {
        Self::new()
    }
}

/// One parsed ViewSetup from the companion XML.
struct ViewSetupXml {
    id: u32,
    voxel_size: Option<(f64, f64, f64)>,
}

/// Parse the `<ViewSetup>` blocks from the SpimData XML, extracting each
/// setup id and (optionally) its voxel size.
fn parse_view_setups(xml: &str) -> Vec<ViewSetupXml> {
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(open) = xml[pos..].find("<ViewSetup>") {
        let start = pos + open + "<ViewSetup>".len();
        let end_rel = xml[start..].find("</ViewSetup>").unwrap_or(xml.len() - start);
        let block = &xml[start..start + end_rel];
        pos = start + end_rel;

        // <id>N</id>
        let id = inner_text(block, "id").and_then(|s| s.trim().parse::<u32>().ok());
        // <voxelSize><size>X Y Z</size></voxelSize>
        let voxel_size = inner_text(block, "voxelSize").and_then(|vs| {
            inner_text(&vs, "size").and_then(|s| {
                let parts: Vec<f64> = s.split_whitespace().filter_map(|p| p.parse().ok()).collect();
                if parts.len() >= 3 {
                    Some((parts[0], parts[1], parts[2]))
                } else {
                    None
                }
            })
        });
        if let Some(id) = id {
            out.push(ViewSetupXml { id, voxel_size });
        }
    }
    out
}

/// Find the inner text of the first `<tag>...</tag>` in `xml`.
fn inner_text(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)?;
    Some(xml[start..start + end].to_string())
}

/// Parse the timepoint list from the SpimData `<Timepoints>` block.
///
/// Supports `type="pattern"` with `<integerpattern>` of the forms:
///   * `N`            — a single timepoint
///   * `first-last`   — an inclusive range
///   * `first-last:increment`
/// and `type="range"` with `<first>`/`<last>`. Falls back to `[0]`.
fn parse_timepoints(xml: &str) -> Vec<u32> {
    if let Some(pat) = inner_text(xml, "integerpattern") {
        let pat = pat.trim();
        // first-last:increment
        let (range_part, inc) = match pat.split_once(':') {
            Some((r, i)) => (r, i.trim().parse::<u32>().ok().filter(|&v| v > 0).unwrap_or(1)),
            None => (pat, 1),
        };
        if let Some((first, last)) = range_part.split_once('-') {
            if let (Ok(first), Ok(last)) =
                (first.trim().parse::<u32>(), last.trim().parse::<u32>())
            {
                if last >= first {
                    return (first..=last).step_by(inc as usize).collect();
                }
            }
        } else if let Ok(single) = range_part.parse::<u32>() {
            return vec![single];
        }
    }
    if let (Some(first), Some(last)) = (inner_text(xml, "first"), inner_text(xml, "last")) {
        if let (Ok(first), Ok(last)) = (first.trim().parse::<u32>(), last.trim().parse::<u32>()) {
            if last >= first {
                return (first..=last).collect();
            }
        }
    }
    vec![0]
}

/// Map an HDF5 cells dtype element size to a Bio-Formats pixel type.
/// Java BDVReader maps 1 → UINT8, 2 → UINT16, 4 → INT32 (signed).
fn pixel_type_for_size(size: usize) -> Result<(PixelType, usize)> {
    match size {
        1 => Ok((PixelType::Uint8, 1)),
        2 => Ok((PixelType::Uint16, 2)),
        4 => Ok((PixelType::Int32, 4)),
        other => Err(BioFormatsError::UnsupportedFormat(format!(
            "BDV: unsupported cells dtype size {other}"
        ))),
    }
}

fn parse_bdv(path: &Path) -> Result<Vec<SeriesInfo>> {
    let file = hdf5_pure_rust::File::open(path)
        .map_err(|e| BioFormatsError::Format(format!("HDF5 open error: {e}")))?;

    // ── Enumerate setups and timepoints (companion XML preferred) ────────────
    let xml_path = path.with_extension("xml");
    let xml_str = if xml_path.exists() {
        std::fs::read_to_string(&xml_path).ok()
    } else {
        None
    };

    // Setups: (id, voxelSize). Prefer the XML's ViewSetups; otherwise count the
    // sNN groups at the HDF5 root.
    let setups: Vec<ViewSetupXml> = match xml_str.as_deref().map(parse_view_setups) {
        Some(v) if !v.is_empty() => v,
        _ => {
            let mut members = hdf5_members(&file, "/").unwrap_or_default();
            members.sort();
            members
                .iter()
                .filter(|n| {
                    n.len() == 3
                        && n.starts_with('s')
                        && n[1..].chars().all(|c| c.is_ascii_digit())
                })
                .filter_map(|n| n[1..].parse::<u32>().ok())
                .map(|id| ViewSetupXml {
                    id,
                    voxel_size: None,
                })
                .collect()
        }
    };
    if setups.is_empty() {
        return Err(BioFormatsError::UnsupportedFormat(
            "BDV: no ViewSetups / setup groups found".into(),
        ));
    }

    // Timepoints from the XML, else infer from the tNNNNN root groups.
    let timepoints: Vec<u32> = match xml_str.as_deref().map(parse_timepoints) {
        Some(v) if !v.is_empty() && v != vec![0] => v,
        _ => {
            let mut tps: Vec<u32> = hdf5_members(&file, "/")
                .unwrap_or_default()
                .iter()
                .filter(|n| {
                    n.len() == 6
                        && n.starts_with('t')
                        && n[1..].chars().all(|c| c.is_ascii_digit())
                })
                .filter_map(|n| n[1..].parse::<u32>().ok())
                .collect();
            tps.sort_unstable();
            if tps.is_empty() {
                return Err(BioFormatsError::UnsupportedFormat(
                    "BDV: no timepoint groups found".into(),
                ));
            }
            tps
        }
    };

    // ── Build the flattened series list: setup × timepoint × level ───────────
    let mut series: Vec<SeriesInfo> = Vec::new();
    for setup in &setups {
        for &tp in &timepoints {
            // Number of resolution levels: count integer-named children of the
            // setup group under this timepoint.
            let setup_group = format!("t{tp:05}/s{:02}", setup.id);
            let mut levels: Vec<u32> = match file
                .group(&setup_group)
                .ok()
                .and_then(|g| hdf5_group_members(&g).ok())
            {
                Some(members) => members
                    .iter()
                    .filter_map(|n| n.parse::<u32>().ok())
                    .collect(),
                None => continue, // missing view — skip
            };
            levels.sort_unstable();
            if levels.is_empty() {
                continue;
            }

            for &level in &levels {
                let cells_path = format!("{setup_group}/{level}/cells");
                let ds = match file.dataset(&cells_path) {
                    Ok(d) => d,
                    Err(_) => continue,
                };
                let shape = ds.shape().map_err(|e| {
                    BioFormatsError::Format(format!("BDV: cannot read shape {cells_path}: {e}"))
                })?;
                if shape.len() != 3 || shape.iter().any(|&d| d == 0) {
                    return Err(BioFormatsError::UnsupportedFormat(format!(
                        "BDV: unsupported cells shape {shape:?} for {cells_path}"
                    )));
                }
                let size_z = u32::try_from(shape[0])
                    .map_err(|_| BioFormatsError::Format("BDV Z overflows".into()))?;
                let size_y = u32::try_from(shape[1])
                    .map_err(|_| BioFormatsError::Format("BDV Y overflows".into()))?;
                let size_x = u32::try_from(shape[2])
                    .map_err(|_| BioFormatsError::Format("BDV X overflows".into()))?;

                let dtype_size = ds
                    .dtype()
                    .map(|dt| dt.size())
                    .map_err(|e| BioFormatsError::Format(format!("BDV: dtype {cells_path}: {e}")))?;
                let (pixel_type, bytes_per_sample) = pixel_type_for_size(dtype_size)?;

                let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
                meta_map.insert(
                    "format".into(),
                    MetadataValue::String("BigDataViewer HDF5".into()),
                );
                if let Some(p) = xml_path.to_str() {
                    meta_map.insert("bdv_xml_path".into(), MetadataValue::String(p.into()));
                }
                meta_map.insert("bdv_setup".into(), MetadataValue::Int(setup.id as i64));
                meta_map.insert("bdv_timepoint".into(), MetadataValue::Int(tp as i64));
                meta_map.insert("bdv_level".into(), MetadataValue::Int(level as i64));

                let meta = ImageMetadata {
                    size_x,
                    size_y,
                    size_z,
                    size_c: 1,
                    size_t: 1,
                    pixel_type,
                    bits_per_pixel: (bytes_per_sample * 8) as u8,
                    image_count: size_z,
                    dimension_order: DimensionOrder::XYZTC,
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

                series.push(SeriesInfo {
                    setup: setup.id,
                    timepoint: tp,
                    level,
                    meta,
                    voxel_size: setup.voxel_size,
                });
            }
        }
    }

    if series.is_empty() {
        return Err(BioFormatsError::UnsupportedFormat(
            "BDV: no readable cells datasets found".into(),
        ));
    }

    Ok(series)
}

fn hdf5_group_members(
    group: &hdf5_pure_rust::Group,
) -> std::result::Result<Vec<String>, hdf5_pure_rust::Error> {
    group.member_names()
}

fn hdf5_members(
    file: &hdf5_pure_rust::File,
    path: &str,
) -> std::result::Result<Vec<String>, hdf5_pure_rust::Error> {
    if path == "/" {
        file.member_names()
    } else {
        hdf5_group_members(&file.group(path)?)
    }
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
        self.series = parse_bdv(path)?;
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
        if self.series.is_empty() {
            return Err(BioFormatsError::NotInitialized);
        }
        if s >= self.series.len() {
            return Err(BioFormatsError::SeriesOutOfRange(s));
        }
        self.current_series = s;
        Ok(())
    }

    fn series(&self) -> usize {
        self.current_series
    }

    fn metadata(&self) -> &ImageMetadata {
        self.series
            .get(self.current_series)
            .map(|s| &s.meta)
            .unwrap_or_else(|| crate::common::reader::uninitialized_metadata())
    }

    fn resolution_count(&self) -> usize {
        // Each resolution level is exposed as its own series (Java parity), so
        // every series is a single-resolution image. Returns 0 before set_id.
        if self.series.is_empty() {
            0
        } else {
            1
        }
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if self.series.is_empty() {
            return Err(BioFormatsError::NotInitialized);
        }
        if level != 0 {
            return Err(BioFormatsError::Format(format!(
                "resolution {level} out of range (max 0)"
            )));
        }
        Ok(())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let si = self
            .series
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        let meta = &si.meta;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }

        // sizeC = sizeT = 1, so the plane index is the Z slice directly.
        let z = plane_index as usize;
        let ds_path = format!(
            "t{:05}/s{:02}/{}/cells",
            si.timepoint, si.setup, si.level
        );
        let bps = meta.pixel_type.bytes_per_sample() as usize;
        let plane_pixels = meta.size_x as usize * meta.size_y as usize;

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

        // Select only plane z of the [Z, Y, X] cells dataset.
        let sel = Selection::Hyperslab(vec![
            HyperslabDim::new(z as u64, 1, 1, 1),
            HyperslabDim::new(0, 1, meta.size_y as u64, 1),
            HyperslabDim::new(0, 1, meta.size_x as u64, 1),
        ]);

        let raw: Vec<u8> = match bps {
            1 => ds
                .read_slice::<u8, _>(sel)
                .map_err(|e| BioFormatsError::Format(format!("HDF5 read: {e}")))?,
            2 => {
                let words: Vec<u16> = ds
                    .read_slice::<u16, _>(sel)
                    .map_err(|e| BioFormatsError::Format(format!("HDF5 read: {e}")))?;
                words.iter().flat_map(|w| w.to_le_bytes()).collect()
            }
            4 => {
                let words: Vec<u32> = ds
                    .read_slice::<u32, _>(sel)
                    .map_err(|e| BioFormatsError::Format(format!("HDF5 read: {e}")))?;
                words.iter().flat_map(|w| w.to_le_bytes()).collect()
            }
            other => {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "BDV unsupported bytes-per-sample {other}"
                )))
            }
        };

        let plane_bytes = plane_pixels * bps;
        if raw.len() == plane_bytes {
            Ok(raw)
        } else {
            Err(BioFormatsError::UnsupportedFormat(format!(
                "BDV dataset {ds_path} is shorter than declared plane {plane_index} \
                 (need {plane_bytes} bytes, have {})",
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
        let si = self
            .series
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        crop_full_plane("BDV", &full, &si.meta, 1, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let si = self
            .series
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        let (sx, sy) = (si.meta.size_x, si.meta.size_y);
        let tw = sx.min(256);
        let th = sy.min(256);
        let tx = (sx - tw) / 2;
        let ty = (sy - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn ome_metadata(&self) -> Option<OmeMetadata> {
        if self.series.is_empty() {
            return None;
        }
        let mut ome = OmeMetadata::default();
        for si in &self.series {
            let (psx, psy, psz) = match si.voxel_size {
                Some((x, y, z)) => (Some(x), Some(y), Some(z)),
                None => (None, None, None),
            };
            ome.images.push(OmeImage {
                // Java: "P_t{timepoint:05}, W_s{setup:02}_{level}".
                name: Some(format!(
                    "P_t{:05}, W_s{:02}_{}",
                    si.timepoint, si.setup, si.level
                )),
                physical_size_x: psx,
                physical_size_y: psy,
                physical_size_z: psz,
                channels: vec![OmeChannel {
                    samples_per_pixel: 1,
                    ..Default::default()
                }],
                ..Default::default()
            });
        }
        Some(ome)
    }
}
