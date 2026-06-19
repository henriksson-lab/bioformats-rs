//! BigDataViewer (BDV) HDF5 format reader.
//!
//! Reads BigDataViewer `.h5`/`.xml` datasets (SpimData) used for light-sheet
//! microscopy. A companion `SpimData` XML file carries the ViewSetups (sizes,
//! voxel sizes, channel/illumination/angle attributes) and the Timepoints
//! range; the pixel data lives in the HDF5 file.
//!
//! HDF5 group layout:
//!   t{T:05}/s{S:02}/{level}/cells  — [z, y, x] integer volume
//!
//! ## Series model (Java parity)
//!
//! This is a faithful port of `loci.formats.in.BDVReader`. A *series* is
//! created per `(ViewSetup × resolution-level)`, NOT per timepoint:
//!   * `sizeT` = number of timepoints (all timepoints share one series),
//!   * `sizeC` = number of distinct `channel` attributes in the XML (multiple
//!     channel ViewSetups collapse into a single multi-channel series),
//!   * `sizeZ` = depth of the `cells` volume,
//!   * `imageCount = sizeC * sizeT * sizeZ`,
//!   * `dimensionOrder = XYZTC`.
//!
//! As in upstream Bio-Formats' default `ImageReader` configuration,
//! resolutions are *flattened*: every resolution level of a setup is exposed
//! as its own series. Image names follow `P_t{first:05}, W_s{setup:02}_{level}`.
//!
//! ### Known intentional divergence
//!
//! Two `s08` planes of the SPIM test file are decoded differently from Java —
//! that is an off-by-one bug in the libhdf5 build Bio-Formats bundles
//! (full-precision scaleoffset chunks), fixed in libhdf5 2.x which our
//! pure-Rust HDF5 backend tracks. Our decode is the correct one. See
//! `bioformats_bug.txt`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::ome_metadata::{OmeChannel, OmeImage, OmeMetadata};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;

use hdf5_pure_rust::{HyperslabDim, Selection};

/// One core series: a single resolution level of one logical setup across all
/// timepoints. Resolutions are flattened, but timepoints stay in `sizeT`.
#[derive(Clone)]
struct SeriesInfo {
    /// The first-channel/base `sNN` setup id for this logical series.
    setup: u32,
    /// Setup id for each channel index in this logical series.
    channel_setups: Vec<u32>,
    /// Timepoints exposed through this series' T axis.
    timepoints: Vec<u32>,
    /// Resolution level (the `{level}` group number).
    level: u32,
    /// Core metadata for this series.
    meta: ImageMetadata,
    /// Physical pixel sizes from the owning setup's voxelSize.
    voxel_size: Option<(f64, f64, f64)>,
}

/// One parsed `<ViewSetup>` from the companion XML, mirroring an entry of
/// Java's `setupAttributeList` plus the associated `setupVoxelSizes`.
#[derive(Clone)]
struct SetupXml {
    id: u32,
    name: Option<String>,
    voxel_size: Option<(f64, f64, f64)>,
    voxel_unit: Option<String>,
    /// Custom attributes (channel, illumination, angle, …) as (key, value).
    attributes: Vec<(String, String)>,
}

impl SetupXml {
    fn attribute(&self, key: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

pub struct BdvReader {
    /// Resolved HDF5 file path.
    path: Option<PathBuf>,
    /// Companion XML path (if any).
    xml_path: Option<PathBuf>,
    file: Option<hdf5_pure_rust::File>,

    // -- parsed XML state (mirrors the Java fields) --
    /// All ViewSetups, in XML order (Java `setupAttributeList`).
    setup_attribute_list: Vec<SetupXml>,
    /// Distinct channel attribute values, in encounter order (Java `channelIndexes`).
    channel_indexes: Vec<u32>,
    size_c: u32,
    first_timepoint: u32,
    last_timepoint: u32,
    timepoint_increment: u32,
    timepoint_use_pattern: bool,

    /// Flattened series list (one per logical setup/resolution level).
    series: Vec<SeriesInfo>,
    current_series: usize,
}

impl BdvReader {
    pub fn new() -> Self {
        BdvReader {
            path: None,
            xml_path: None,
            file: None,
            setup_attribute_list: Vec::new(),
            channel_indexes: Vec::new(),
            size_c: 0,
            first_timepoint: 0,
            last_timepoint: 0,
            timepoint_increment: 1,
            timepoint_use_pattern: false,
            series: Vec::new(),
            current_series: 0,
        }
    }

    /// The `cells` dataset path for a plane in the current series.
    fn image_data_path(&self, no: u32) -> Result<String> {
        let si = self
            .series
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        let (_z, c, t) = get_zct_coords(
            si.meta.dimension_order,
            si.meta.size_z,
            si.meta.size_c,
            si.meta.size_t,
            no,
        );
        let timepoint =
            si.timepoints.get(t as usize).copied().ok_or_else(|| {
                BioFormatsError::Format("BDV timepoint index out of range".into())
            })?;
        let setup = si
            .channel_setups
            .get(c as usize)
            .copied()
            .unwrap_or(si.setup);
        Ok(format!(
            "t{:05}/s{:02}/{}/cells",
            timepoint, setup, si.level
        ))
    }
}

impl Default for BdvReader {
    fn default() -> Self {
        Self::new()
    }
}

// ── Companion-XML parsing (mirrors the Java BDVXMLHandler) ──────────────────

/// Parse the `<ViewSetup>` blocks from the SpimData XML, extracting each
/// setup id, name, voxel size/unit and custom attributes. Java accumulates
/// these in `setupAttributeList`, `setupVoxelSizes` and `channelIndexes`.
fn parse_view_setups(xml: &str) -> Vec<SetupXml> {
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some((_open, start)) = find_start_tag_end_ci(xml, "viewsetup", pos) {
        let end_rel = find_end_tag_ci(&xml[start..], "viewsetup").unwrap_or(xml.len() - start);
        let block = &xml[start..start + end_rel];
        pos = start + end_rel;

        let id = inner_text(block, "id").and_then(|s| s.trim().parse::<u32>().ok());
        let name = inner_text(block, "name").map(|s| s.trim().to_string());
        let voxel_block = inner_text(block, "voxelSize");
        let voxel_size = voxel_block.as_deref().and_then(|vs| {
            inner_text(vs, "size").and_then(|s| {
                let parts: Vec<f64> = s
                    .split_whitespace()
                    .filter_map(|p| p.parse().ok())
                    .collect();
                if parts.len() >= 3 {
                    Some((parts[0], parts[1], parts[2]))
                } else {
                    None
                }
            })
        });
        let voxel_unit = voxel_block
            .as_deref()
            .and_then(|vs| inner_text(vs, "unit"))
            .map(|s| s.trim().to_string());
        let attributes = parse_view_setup_attributes(block);
        if let Some(id) = id {
            out.push(SetupXml {
                id,
                name,
                voxel_size,
                voxel_unit,
                attributes,
            });
        }
    }
    out
}

/// Extract the leaf attributes inside a `<attributes>...</attributes>` block.
/// Java records each non-empty leaf element under `<attributes>` (keyed by its
/// lower-cased tag name) into the setup's attribute map; nested non-leaf
/// elements are skipped. The `channel` attribute additionally feeds
/// `channelIndexes`/`sizeC` (handled by the caller).
fn parse_view_setup_attributes(block: &str) -> Vec<(String, String)> {
    let Some(attributes_block) = inner_text(block, "attributes") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let bytes = attributes_block.as_bytes();
    let mut pos = 0usize;
    while out.len() < 64 {
        let Some(open_rel) = attributes_block[pos..].find('<') else {
            break;
        };
        let open = pos + open_rel;
        if bytes.get(open + 1).is_some_and(|b| *b == b'/') {
            pos = open + 2;
            continue;
        }
        let Some(gt_rel) = attributes_block[open..].find('>') else {
            break;
        };
        let gt = open + gt_rel;
        let raw_tag = attributes_block[open + 1..gt].trim();
        let tag = raw_tag
            .split_whitespace()
            .next()
            .unwrap_or("")
            .trim_end_matches('/');
        if tag.is_empty() || raw_tag.ends_with('/') {
            pos = gt + 1;
            continue;
        }
        let Some(close_rel) = find_end_tag_ci(&attributes_block[gt + 1..], tag) else {
            pos = gt + 1;
            continue;
        };
        let value = crate::common::xml::decode_xml_escaped_str(
            attributes_block[gt + 1..gt + 1 + close_rel].trim(),
        );
        if !value.is_empty()
            && !value.contains('<')
            && tag
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
        {
            out.push((bdv_metadata_key(tag), value));
        }
        pos = gt + 1 + close_rel + tag.len() + 3;
    }
    out
}

fn bdv_metadata_key(name: &str) -> String {
    let mut key = String::new();
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            key.push(ch.to_ascii_lowercase());
        } else if !key.ends_with('_') {
            key.push('_');
        }
    }
    key.trim_matches('_').to_string()
}

/// Find the inner text of the first `<tag>...</tag>` in `xml`.
fn inner_text(xml: &str, tag: &str) -> Option<String> {
    let (_, start) = find_start_tag_end_ci(xml, tag, 0)?;
    let end = find_end_tag_ci(&xml[start..], tag)?;
    Some(crate::common::xml::decode_xml_escaped_str(
        &xml[start..start + end],
    ))
}

/// Find inner text of `<tag ...>...</tag>`, allowing attributes on the opening tag.
fn inner_text_with_attrs(xml: &str, tag: &str) -> Option<String> {
    inner_text(xml, tag)
}

fn find_start_tag_end_ci(xml: &str, tag: &str, from: usize) -> Option<(usize, usize)> {
    let lower = xml[from..].to_ascii_lowercase();
    let needle = format!("<{}", tag.to_ascii_lowercase());
    let mut search = 0usize;
    while let Some(rel) = lower[search..].find(&needle) {
        let open = from + search + rel;
        let after = open + needle.len();
        let boundary = xml.as_bytes().get(after).copied();
        if matches!(
            boundary,
            Some(b'>') | Some(b'/') | Some(b' ') | Some(b'\t') | Some(b'\n') | Some(b'\r')
        ) {
            let end = xml[open..].find('>')? + open + 1;
            return Some((open, end));
        }
        search += rel + needle.len();
    }
    None
}

fn find_end_tag_ci(xml: &str, tag: &str) -> Option<usize> {
    let lower = xml.to_ascii_lowercase();
    lower.find(&format!("</{}>", tag.to_ascii_lowercase()))
}

/// Parse the timepoint range/pattern from the SpimData `<Timepoints>` block.
///
/// Mirrors Java's handler: `type="pattern"` uses `<integerpattern>` of the
/// form `first` / `first-last`; Java intends to support `first-last:increment`
/// too, but gates that branch behind `DataTools.parseInteger(parts[1])`, so
/// the colon form leaves `last`/`increment` unchanged. Otherwise `<first>` and
/// `<last>` bound an inclusive range. Returns `(first, last, increment,
/// use_pattern)`. Falls back to `(0, 0, 1, false)`.
fn parse_timepoints(xml: &str) -> (u32, u32, u32, bool) {
    let mut first = 0u32;
    let mut last = 0u32;
    let mut increment = 1u32;

    let Some((tp_open, body_start)) = find_start_tag_end_ci(xml, "timepoints", 0) else {
        return (first, last, increment, false);
    };
    let tag = &xml[tp_open..body_start];
    let Some(close_rel) = find_end_tag_ci(&xml[body_start..], "timepoints") else {
        return (first, last, increment, false);
    };
    let body = &xml[body_start..body_start + close_rel];

    // Determine whether the <Timepoints type="..."> is a pattern.
    let mut use_pattern = false;
    if let Some(tidx) = tag.find("type=") {
        let rest = &tag[tidx + 5..];
        let quote = rest.chars().next();
        if matches!(quote, Some('"') | Some('\'')) {
            let q = quote.unwrap();
            if let Some(end) = rest[1..].find(q) {
                let val = &rest[1..1 + end];
                use_pattern = val.eq_ignore_ascii_case("pattern");
            }
        }
    }

    if use_pattern {
        if let Some(pat) = inner_text(body, "integerpattern") {
            parse_integer_string(pat.trim(), &mut first, &mut last, &mut increment);
        }
    }
    if let Some(f) = inner_text(body, "first").and_then(|s| s.trim().parse::<u32>().ok()) {
        first = f;
    }
    if let Some(l) = inner_text(body, "last").and_then(|s| s.trim().parse::<u32>().ok()) {
        last = l;
    }

    (first, last, increment, use_pattern)
}

/// Parse a timepoint pattern (Java `parseIntegerString`).
fn parse_integer_string(pattern: &str, first: &mut u32, last: &mut u32, increment: &mut u32) {
    let parts: Vec<&str> = pattern.split('-').collect();
    if let Ok(f) = parts[0].trim().parse::<u32>() {
        *first = f;
    }
    // Java checks DataTools.parseInteger(parts[1]) before splitting on ':'.
    // Because that helper is Integer.valueOf, values like "10:2" do not enter
    // this branch; preserve the actual reader behavior.
    if parts.len() > 1 && parts[1].trim().parse::<u32>().is_ok() {
        let parts2: Vec<&str> = parts[1].split(':').collect();
        if let Ok(l) = parts2[0].trim().parse::<u32>() {
            *last = l;
        }
        if parts2.len() > 1 {
            if let Ok(i) = parts2[1].trim().parse::<u32>() {
                *increment = i;
            }
        }
    }
}

/// Java BDVReader computes patterned timepoint count as integer division of
/// `(last - first + 1) / increment`, even though the path iteration still walks
/// `first, first + increment, ... <= last`.
fn java_timepoint_count(first: u32, last: u32, increment: u32, use_pattern: bool) -> u32 {
    let last = if !use_pattern && last == 0 {
        first
    } else {
        last
    };
    if use_pattern && last > 0 {
        let span = last.saturating_sub(first).saturating_add(1);
        if increment > 0 {
            (span / increment).max(1)
        } else {
            span.max(1)
        }
    } else {
        last.saturating_sub(first).saturating_add(1).max(1)
    }
}

/// Map an HDF5 cells dtype element size to a Bio-Formats pixel type.
/// Java BDVReader maps 1 → UINT8, 2 → UINT16, 4 → INT32 (signed).
fn pixel_type_for_size(size: usize) -> Result<(PixelType, usize)> {
    match size {
        1 => Ok((PixelType::Uint8, 1)),
        2 => Ok((PixelType::Uint16, 2)),
        4 => Ok((PixelType::Int32, 4)),
        other => Err(BioFormatsError::Format(format!(
            "Pixel type not understood. Only 8, 16 and 32 bit images supported (size {other})"
        ))),
    }
}

/// Compute (z, c, t) for plane `no` in the given dimension order. Mirrors
/// `loci.formats.FormatTools.getZCTCoords`.
fn get_zct_coords(
    order: DimensionOrder,
    size_z: u32,
    size_c: u32,
    size_t: u32,
    index: u32,
) -> (u32, u32, u32) {
    let (s0, s1) = match order {
        DimensionOrder::XYZCT => (size_z, size_c),
        DimensionOrder::XYZTC => (size_z, size_t),
        DimensionOrder::XYCZT => (size_c, size_z),
        DimensionOrder::XYCTZ => (size_c, size_t),
        DimensionOrder::XYTZC => (size_t, size_z),
        DimensionOrder::XYTCZ => (size_t, size_c),
    };
    let s0 = s0.max(1);
    let s1 = s1.max(1);
    let v0 = index % s0;
    let v1 = (index / s0) % s1;
    let v2 = index / (s0 * s1);
    match order {
        DimensionOrder::XYZCT => (v0, v1, v2),
        DimensionOrder::XYZTC => (v0, v2, v1),
        DimensionOrder::XYCZT => (v1, v0, v2),
        DimensionOrder::XYCTZ => (v2, v0, v1),
        DimensionOrder::XYTZC => (v1, v2, v0),
        DimensionOrder::XYTCZ => (v2, v1, v0),
    }
}

// ── Path resolution (mirrors Java fetchXMLId + initFile) ────────────────────

/// Resolve `(h5_path, xml_path, xml_string)` from the file the user opened.
///
/// Two entry points, mirroring Java `initFile`/`fetchXMLId` but more lenient so
/// a bare `.h5` (no companion XML, or an XML lacking `<hdf5>`) stays usable —
/// the pure-Rust reader infers structure from the HDF5 groups in that case:
///
///   * Opening the `.h5`: that file *is* the HDF5 pixel store. The companion
///     `basename.xml` (Java `fetchXMLId`) is read for metadata when present but
///     is optional, as is its `<hdf5>` element.
///   * Opening the `.xml`: the HDF5 path comes from the `<hdf5>` element
///     (resolved relative to the XML's directory, mirroring the Java handler
///     which builds `parent + File.separator + hdf5Contents`). Here the XML and
///     its `<hdf5>` element are required.
fn resolve_bdv_paths(path: &Path) -> Result<(PathBuf, Option<PathBuf>, Option<String>)> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());

    if matches!(ext.as_deref(), Some("h5")) {
        // The opened .h5 is the pixel store; look for the optional companion
        // XML (basename up to the first '.', plus ".xml", same dir).
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let base = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| match n.find('.') {
                Some(i) => &n[..i],
                None => n,
            })
            .unwrap_or("");
        let candidate = parent.join(format!("{base}.xml"));
        let (xml_path, xml_str) = if candidate.exists() {
            let xml = std::fs::read_to_string(&candidate).ok();
            (Some(candidate), xml)
        } else {
            (None, None)
        };
        return Ok((path.to_path_buf(), xml_path, xml_str));
    }

    // Opening the .xml: the HDF5 location is taken from <hdf5>.
    let xml_path = path.to_path_buf();
    let xml = std::fs::read_to_string(&xml_path).map_err(BioFormatsError::Io)?;

    // The Java handler reads <hdf5> and resolves it relative to the XML dir.
    let hdf5 = inner_text_with_attrs(&xml, "hdf5")
        .map(|s| s.trim().to_string())
        .filter(|s| s.to_ascii_lowercase().ends_with(".h5"))
        .ok_or_else(|| BioFormatsError::Format("Could not find H5 file location in XML".into()))?;

    let xml_parent = xml_path.parent().unwrap_or_else(|| Path::new("."));
    let h5 = PathBuf::from(&hdf5);
    let h5_path = if h5.is_absolute() {
        h5
    } else {
        xml_parent.join(h5)
    };

    Ok((h5_path, Some(xml_path), Some(xml)))
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
        matches!(ext.as_deref(), Some("h5") | Some("xml"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        // Intentionally false — avoid conflict with ImarisHdfReader which uses HDF5
        // magic bytes; rely on extension/XML detection only.
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;

        // initFile: resolve XML, parse it (BDVXMLHandler), then HDF5 structure.
        let (h5_path, xml_path, xml_str) = resolve_bdv_paths(path)?;

        // Verify the XML is a SpimData document (Java relies on isThisType, but
        // we guard here so unrelated XML/H5 pairs are rejected cleanly).
        if let Some(xml) = xml_str.as_deref() {
            if !xml.contains("SpimData") {
                return Err(BioFormatsError::Format(
                    "BDV: companion XML is not a SpimData document".into(),
                ));
            }
        }

        self.parse_xml(xml_str.as_deref());

        let file = hdf5_pure_rust::File::open(&h5_path)
            .map_err(|e| BioFormatsError::Format(format!("HDF5 open error: {e}")))?;

        self.path = Some(h5_path);
        self.xml_path = xml_path;
        self.file = Some(file);

        self.parse_structure()?;

        self.current_series = 0;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.xml_path = None;
        self.file = None;
        self.setup_attribute_list.clear();
        self.channel_indexes.clear();
        self.size_c = 0;
        self.first_timepoint = 0;
        self.last_timepoint = 0;
        self.timepoint_increment = 1;
        self.timepoint_use_pattern = false;
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
        self.series
            .get(self.current_series)
            .map(|s| s.meta.resolution_count as usize)
            .unwrap_or(0)
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if self.series.is_empty() {
            return Err(BioFormatsError::NotInitialized);
        }
        if level >= self.resolution_count().max(1) {
            return Err(BioFormatsError::Format(format!(
                "resolution {level} out of range (max {})",
                self.resolution_count().saturating_sub(1)
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
        let (z, _c, _t) = get_zct_coords(
            meta.dimension_order,
            meta.size_z,
            meta.size_c,
            meta.size_t,
            plane_index,
        );
        let (sx, sy) = (meta.size_x, meta.size_y);
        self.read_block(plane_index, z, 0, 0, sx, sy)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let si = self
            .series
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        let meta = &si.meta;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let x2 = x
            .checked_add(w)
            .ok_or_else(|| BioFormatsError::Format("BDV region width overflows".into()))?;
        let y2 = y
            .checked_add(h)
            .ok_or_else(|| BioFormatsError::Format("BDV region height overflows".into()))?;
        if x2 > meta.size_x || y2 > meta.size_y {
            return Err(BioFormatsError::Format(
                "BDV region is outside image bounds".into(),
            ));
        }
        let (z, _c, _t) = get_zct_coords(
            meta.dimension_order,
            meta.size_z,
            meta.size_c,
            meta.size_t,
            plane_index,
        );
        self.read_block(plane_index, z, x, y, w, h)
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
            let setup = self.setup_attribute_list.iter().find(|s| s.id == si.setup);
            let first_timepoint = si
                .timepoints
                .first()
                .copied()
                .unwrap_or(self.first_timepoint);
            let name = match setup {
                Some(s) => format!("P_t{first_timepoint:05}, W_s{:02}_{}", s.id, si.level),
                None => format!("P_t{first_timepoint:05}, W_s{:02}_{}", si.setup, si.level),
            };
            let (psx, psy, psz) = match si.voxel_size {
                Some((x, y, z)) => (Some(x), Some(y), Some(z)),
                None => (None, None, None),
            };
            let channels: Vec<OmeChannel> = (0..si.meta.size_c.max(1))
                .map(|_| OmeChannel {
                    samples_per_pixel: 1,
                    ..Default::default()
                })
                .collect();
            ome.images.push(OmeImage {
                name: Some(name),
                physical_size_x: psx,
                physical_size_y: psy,
                physical_size_z: psz,
                channels,
                ..Default::default()
            });
            let image_index = ome.images.len() - 1;
            let _ = ome.add_original_metadata_annotations(&si.meta, image_index);
        }
        Some(ome)
    }
}

impl BdvReader {
    /// Mirrors the BDVXMLHandler: populate setups, channel indexes, sizeC and
    /// the timepoint range from the companion SpimData XML. When no XML is
    /// available the fields stay at their defaults and `parse_structure` falls
    /// back to enumerating the HDF5 groups.
    fn parse_xml(&mut self, xml: Option<&str>) {
        let Some(xml) = xml else { return };

        // ViewSetups (setupAttributeList + setupVoxelSizes).
        self.setup_attribute_list = parse_view_setups(xml);

        // channelIndexes / sizeC: each distinct "channel" attribute value, in
        // XML encounter order.
        self.channel_indexes.clear();
        for setup in &self.setup_attribute_list {
            if let Some(value) = setup.attribute("channel") {
                if let Ok(idx) = value.trim().parse::<u32>() {
                    if !self.channel_indexes.contains(&idx) {
                        self.channel_indexes.push(idx);
                    }
                }
            }
        }
        self.size_c = self.channel_indexes.len() as u32;

        // Timepoints.
        let (first, last, increment, use_pattern) = parse_timepoints(xml);
        self.first_timepoint = first;
        self.last_timepoint = last;
        self.timepoint_increment = increment.max(1);
        self.timepoint_use_pattern = use_pattern;
    }

    /// Walk the HDF5 group tree (`tNNNNN/sNN/{level}/cells`) and build one core
    /// series per logical setup/resolution level. Timepoints stay in `sizeT`;
    /// channel ViewSetups with the same non-channel position collapse into
    /// `sizeC`, matching Java BDVReader.
    fn parse_structure(&mut self) -> Result<()> {
        // If no XML setups were parsed, fall back to enumerating root groups.
        if self.setup_attribute_list.is_empty() {
            self.infer_setups_and_timepoints_from_hdf5()?;
        }

        let file = self.file.as_ref().ok_or(BioFormatsError::NotInitialized)?;

        // Enumerate the timepoints (XML range/pattern) to visit, in order.
        // Bio-Formats' pattern branch computes the count with integer division
        // of `(last - first + 1) / increment`, rather than including every
        // stepped endpoint. Preserve that behavior for metadata/plane parity.
        let mut timepoints: Vec<u32> = Vec::new();
        let last = self.last_timepoint.max(self.first_timepoint);
        let increment = self.timepoint_increment.max(1);
        if self.timepoint_use_pattern && last > 0 {
            let count = java_timepoint_count(
                self.first_timepoint,
                self.last_timepoint,
                self.timepoint_increment,
                self.timepoint_use_pattern,
            );
            for i in 0..count {
                timepoints.push(
                    self.first_timepoint
                        .saturating_add(i.saturating_mul(increment)),
                );
            }
        } else {
            let mut tp = self.first_timepoint;
            while tp <= last {
                timepoints.push(tp);
                let next = tp.saturating_add(increment);
                if next == tp {
                    break;
                }
                tp = next;
            }
        }

        let size_c = self.size_c.max(1);

        // Build the flattened series list: logical setup × level. Only first
        // timepoint coordinates that actually carry a `cells` dataset become
        // series; other timepoints are resolved at plane-read time through T.
        let mut series: Vec<SeriesInfo> = Vec::new();
        let first_channel = self.channel_indexes.first().copied();
        let mut logical_setup_index = 0usize;
        for setup in &self.setup_attribute_list {
            let participates_as_base = if size_c == 1 {
                true
            } else {
                setup
                    .attribute("channel")
                    .and_then(|v| v.trim().parse::<u32>().ok())
                    == first_channel
            };
            if !participates_as_base {
                continue;
            }

            let channel_setups =
                self.channel_setups_for_logical_index(logical_setup_index, setup.id);
            logical_setup_index += 1;

            let setup_group = format!("t{:05}/s{:02}", timepoints[0], setup.id);
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

            for &level in &levels {
                let cells_path = format!("{setup_group}/{level}/cells");
                if !dataset_exists(file, &cells_path) {
                    continue;
                }

                // Shape: HDF5 cells dataset is [z, y, x].
                let ds = file
                    .dataset(&cells_path)
                    .map_err(|e| BioFormatsError::Format(format!("dataset {cells_path}: {e}")))?;
                let shape = ds.shape().map_err(|e| {
                    BioFormatsError::Format(format!("BDV: cannot read shape {cells_path}: {e}"))
                })?;
                if shape.len() != 3 || shape.iter().any(|&d| d == 0) {
                    return Err(BioFormatsError::Format(format!(
                        "BDV: unsupported cells shape {shape:?} for {cells_path}"
                    )));
                }
                let size_z = u32::try_from(shape[0])
                    .map_err(|_| BioFormatsError::Format("BDV Z overflows".into()))?;
                let size_y = u32::try_from(shape[1])
                    .map_err(|_| BioFormatsError::Format("BDV Y overflows".into()))?;
                let size_x = u32::try_from(shape[2])
                    .map_err(|_| BioFormatsError::Format("BDV X overflows".into()))?;

                let dtype_size = ds.dtype().map(|dt| dt.size()).map_err(|e| {
                    BioFormatsError::Format(format!("BDV: dtype {cells_path}: {e}"))
                })?;
                let (pixel_type, bytes_per_sample) = pixel_type_for_size(dtype_size)?;

                let voxel_size = setup.voxel_size;
                let meta_map =
                    self.build_series_metadata(Some(setup), setup.id, timepoints[0], level);
                let size_t = java_timepoint_count(
                    self.first_timepoint,
                    self.last_timepoint,
                    self.timepoint_increment,
                    self.timepoint_use_pattern,
                );
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
                    is_indexed: true,
                    is_little_endian: true,
                    // This reader exposes each BDV mipmap level as its own
                    // flattened series, matching Java's core view here; each
                    // visible series therefore has a single active resolution.
                    resolution_count: 1,
                    thumbnail: false,
                    series_metadata: meta_map,
                    lookup_table: None,
                    modulo_z: None,
                    modulo_c: None,
                    modulo_t: None,
                };

                series.push(SeriesInfo {
                    setup: setup.id,
                    channel_setups: channel_setups.clone(),
                    timepoints: timepoints.clone(),
                    level,
                    meta,
                    voxel_size,
                });
            }
        }

        if series.is_empty() {
            return Err(BioFormatsError::Format("No image data found...".into()));
        }

        self.series = series;
        Ok(())
    }

    fn channel_setups_for_logical_index(&self, logical_index: usize, base_setup: u32) -> Vec<u32> {
        if self.size_c <= 1 || self.channel_indexes.is_empty() {
            return vec![base_setup];
        }

        self.channel_indexes
            .iter()
            .map(|channel| {
                self.setup_attribute_list
                    .iter()
                    .filter(|setup| {
                        setup
                            .attribute("channel")
                            .and_then(|v| v.trim().parse::<u32>().ok())
                            == Some(*channel)
                    })
                    .nth(logical_index)
                    .map(|setup| setup.id)
                    .unwrap_or(base_setup)
            })
            .collect()
    }

    /// Build the per-series original-metadata map (format tag, paths, setup
    /// attributes, voxel size). Not a Java method — Java records these in the
    /// MetadataStore as annotations; we surface them via `series_metadata`.
    fn build_series_metadata(
        &self,
        setup: Option<&SetupXml>,
        setup_id: u32,
        timepoint: u32,
        level: u32,
    ) -> HashMap<String, MetadataValue> {
        let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
        meta_map.insert(
            "format".into(),
            MetadataValue::String("BigDataViewer HDF5".into()),
        );
        if let Some(p) = self.xml_path.as_ref().and_then(|p| p.to_str()) {
            meta_map.insert("bdv_xml_path".into(), MetadataValue::String(p.into()));
        }
        meta_map.insert("bdv_setup".into(), MetadataValue::Int(setup_id as i64));
        meta_map.insert("bdv_timepoint".into(), MetadataValue::Int(timepoint as i64));
        meta_map.insert("bdv_level".into(), MetadataValue::Int(level as i64));
        if let Some(setup) = setup {
            if let Some(name) = setup.name.as_ref().filter(|s| !s.is_empty()) {
                meta_map.insert(
                    "bdv_view_setup_name".into(),
                    MetadataValue::String(name.clone()),
                );
            }
            if let Some(unit) = setup.voxel_unit.as_ref().filter(|s| !s.is_empty()) {
                meta_map.insert("bdv_voxel_unit".into(), MetadataValue::String(unit.clone()));
            }
            if !setup.attributes.is_empty() {
                meta_map.insert(
                    "bdv_view_setup_attribute_count".into(),
                    MetadataValue::Int(setup.attributes.len() as i64),
                );
                for (key, value) in &setup.attributes {
                    meta_map.insert(
                        format!("bdv_view_setup_attribute.{key}"),
                        MetadataValue::String(value.clone()),
                    );
                }
            }
            if let Some((x, y, z)) = setup.voxel_size {
                meta_map.insert("bdv_voxel_size_x".into(), MetadataValue::Float(x));
                meta_map.insert("bdv_voxel_size_y".into(), MetadataValue::Float(y));
                meta_map.insert("bdv_voxel_size_z".into(), MetadataValue::Float(z));
            }
        }
        meta_map
    }

    /// Fallback when there is no companion XML: enumerate the HDF5 root for
    /// `tNNNNN` timepoint groups and (via the first timepoint) `sNN` setups.
    /// Not present in Java (which requires the XML) but keeps the reader usable
    /// for bare `.h5` inputs lacking an XML.
    fn infer_setups_and_timepoints_from_hdf5(&mut self) -> Result<()> {
        let file = self.file.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut root = hdf5_members(file, "/").unwrap_or_default();
        root.sort();

        let mut timepoints: Vec<u32> = root
            .iter()
            .filter(|n| {
                n.len() == 6 && n.starts_with('t') && n[1..].chars().all(|c| c.is_ascii_digit())
            })
            .filter_map(|n| n[1..].parse::<u32>().ok())
            .collect();
        timepoints.sort_unstable();
        if timepoints.is_empty() {
            // No timepoint groups → there are no `sNN` setup groups to enumerate
            // either, so report the same "no setups" condition the XML-less path
            // and the empty-`<ViewSetup>` path use.
            return Err(BioFormatsError::Format(
                "BDV: no ViewSetups / setup groups found".into(),
            ));
        }
        self.first_timepoint = timepoints[0];
        self.last_timepoint = *timepoints.last().unwrap();
        self.timepoint_increment = if timepoints.len() > 1 {
            (timepoints[1] - timepoints[0]).max(1)
        } else {
            1
        };
        self.timepoint_use_pattern = false;

        // Setups: sNN groups under the first timepoint.
        let first_tp_group = format!("t{:05}", self.first_timepoint);
        let mut setups: Vec<u32> = hdf5_members(file, &first_tp_group)
            .unwrap_or_default()
            .iter()
            .filter(|n| {
                n.len() == 3 && n.starts_with('s') && n[1..].chars().all(|c| c.is_ascii_digit())
            })
            .filter_map(|n| n[1..].parse::<u32>().ok())
            .collect();
        setups.sort_unstable();
        if setups.is_empty() {
            return Err(BioFormatsError::Format(
                "BDV: no ViewSetups / setup groups found".into(),
            ));
        }
        self.setup_attribute_list = setups
            .into_iter()
            .map(|id| SetupXml {
                id,
                name: None,
                voxel_size: None,
                voxel_unit: None,
                attributes: Vec::new(),
            })
            .collect();
        self.channel_indexes.clear();
        self.size_c = 0;
        Ok(())
    }

    /// Read a (z, x, y, w, h) sub-block of the current series' plane, resolving
    /// the actual `cells` dataset via the channel/timepoint/resolution mapping
    /// (Java `getImageData`). Returns little-endian packed bytes.
    fn read_block(&self, no: u32, z: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let ds_path = self.image_data_path(no)?;
        let bps = self
            .series
            .get(self.current_series)
            .map(|s| s.meta.pixel_type.bytes_per_sample() as usize)
            .ok_or(BioFormatsError::NotInitialized)?;

        let file = self.file.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let ds = file
            .dataset(&ds_path)
            .map_err(|e| BioFormatsError::Format(format!("dataset {ds_path}: {e}")))?;

        let sel = Selection::Hyperslab(vec![
            HyperslabDim::new(z as u64, 1, 1, 1),
            HyperslabDim::new(y as u64, 1, h as u64, 1),
            HyperslabDim::new(x as u64, 1, w as u64, 1),
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
                return Err(BioFormatsError::Format(format!(
                    "BDV unsupported bytes-per-sample {other}"
                )))
            }
        };

        let expected = (w as usize)
            .checked_mul(h as usize)
            .and_then(|v| v.checked_mul(bps))
            .ok_or_else(|| BioFormatsError::Format("BDV byte count overflows".into()))?;
        if raw.len() == expected {
            Ok(raw)
        } else {
            Err(BioFormatsError::Format(format!(
                "BDV dataset {ds_path} region is shorter than declared plane {no} \
                 (need {expected} bytes, have {})",
                raw.len()
            )))
        }
    }
}

/// Whether a dataset exists at `path` in `file`.
fn dataset_exists(file: &hdf5_pure_rust::File, path: &str) -> bool {
    file.dataset(path).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bdv_view_setup_attributes_are_preserved_as_bounded_scalars() {
        let xml = r#"<SpimData>
  <SequenceDescription>
    <ViewSetups>
      <ViewSetup>
        <id>2</id>
        <name>view A</name>
        <voxelSize><unit>micrometer</unit><size>0.3 0.4 1.5</size></voxelSize>
        <attributes>
          <illumination>0</illumination>
          <channel-name>DAPI</channel-name>
          <angle>45</angle>
          <nested><ignored>yes</ignored></nested>
        </attributes>
      </ViewSetup>
    </ViewSetups>
  </SequenceDescription>
</SpimData>"#;

        let setups = parse_view_setups(xml);

        assert_eq!(setups.len(), 1);
        assert_eq!(setups[0].id, 2);
        assert_eq!(setups[0].name.as_deref(), Some("view A"));
        assert_eq!(setups[0].voxel_size, Some((0.3_f64, 0.4_f64, 1.5_f64)));
        assert_eq!(
            setups[0].attributes,
            vec![
                ("illumination".into(), "0".into()),
                ("channel_name".into(), "DAPI".into()),
                ("angle".into(), "45".into())
            ]
        );
    }

    #[test]
    fn bdv_xml_tags_match_java_case_insensitively() {
        let xml = r#"<SpimData>
  <SequenceDescription>
    <ViewSetups>
      <VIEWSETUP type="relative">
        <ID>3</ID>
        <NAME>A&amp;B</NAME>
        <VOXELSIZE><UNIT>&#181;m</UNIT><SIZE>1 2 3</SIZE></VOXELSIZE>
        <ATTRIBUTES><CHANNEL-NAME>DAPI &amp; FITC</CHANNEL-NAME></ATTRIBUTES>
      </VIEWSETUP>
    </ViewSetups>
  </SequenceDescription>
  <TIMEPOINTS type="pattern"><INTEGERPATTERN>1-5:2</INTEGERPATTERN></TIMEPOINTS>
</SpimData>"#;

        let setups = parse_view_setups(xml);
        assert_eq!(setups.len(), 1);
        assert_eq!(setups[0].id, 3);
        assert_eq!(setups[0].name.as_deref(), Some("A&B"));
        assert_eq!(setups[0].voxel_unit.as_deref(), Some("\u{b5}m"));
        assert_eq!(
            setups[0].attributes,
            vec![("channel_name".into(), "DAPI & FITC".into())]
        );
        assert_eq!(parse_timepoints(xml), (1, 0, 1, true));
    }

    #[test]
    fn bdv_metadata_key_normalizes_attribute_names() {
        assert_eq!(bdv_metadata_key("channel-name"), "channel_name");
        assert_eq!(bdv_metadata_key("ViewSetup.Id"), "viewsetup_id");
        assert_eq!(bdv_metadata_key(" angle "), "angle");
    }

    #[test]
    fn bdv_timepoint_pattern_first_last_increment() {
        let xml = r#"<SpimData><Timepoints type="pattern">
          <integerpattern>0-10:2</integerpattern>
        </Timepoints></SpimData>"#;
        let (first, last, inc, pat) = parse_timepoints(xml);
        assert_eq!((first, last, inc, pat), (0, 0, 1, true));
    }

    #[test]
    fn bdv_timepoint_pattern_first_last_without_increment() {
        let xml = r#"<SpimData><Timepoints type="pattern">
          <integerpattern>0-10</integerpattern>
        </Timepoints></SpimData>"#;
        let (first, last, inc, pat) = parse_timepoints(xml);
        assert_eq!((first, last, inc, pat), (0, 10, 1, true));
    }

    #[test]
    fn bdv_pattern_size_t_matches_java_colon_increment_behavior() {
        let first = 0;
        let last = 0;
        let increment = 1;

        assert_eq!(java_timepoint_count(first, last, increment, true), 1);
    }

    #[test]
    fn bdv_timepoint_range_first_last() {
        let xml = r#"<SpimData><Timepoints type="range">
          <first>3</first><last>7</last>
        </Timepoints></SpimData>"#;
        let (first, last, inc, pat) = parse_timepoints(xml);
        assert_eq!((first, last, inc, pat), (3, 7, 1, false));
    }

    #[test]
    fn bdv_timepoint_range_ignores_first_last_outside_timepoints() {
        let xml = r#"<SpimData>
          <ViewRegistration><first>99</first><last>101</last></ViewRegistration>
          <Timepoints type="range"><first>3</first><last>7</last></Timepoints>
        </SpimData>"#;
        let (first, last, inc, pat) = parse_timepoints(xml);
        assert_eq!((first, last, inc, pat), (3, 7, 1, false));
    }

    #[test]
    fn bdv_timepoint_pattern_ignores_first_last_outside_timepoints() {
        let xml = r#"<SpimData>
          <ViewRegistration><first>99</first><last>101</last></ViewRegistration>
          <Timepoints type="pattern"><integerpattern>0-10:2</integerpattern></Timepoints>
        </SpimData>"#;
        let (first, last, inc, pat) = parse_timepoints(xml);
        assert_eq!((first, last, inc, pat), (0, 0, 1, true));
    }

    #[test]
    fn bdv_channels_collapse_into_size_c() {
        let xml = r#"<SpimData>
          <ViewSetup><id>0</id><attributes><channel>0</channel></attributes></ViewSetup>
          <ViewSetup><id>1</id><attributes><channel>1</channel></attributes></ViewSetup>
        </SpimData>"#;
        let mut r = BdvReader::new();
        r.parse_xml(Some(xml));
        assert_eq!(r.size_c, 2);
        assert_eq!(r.channel_indexes, vec![0, 1]);
        assert_eq!(r.setup_attribute_list.len(), 2);
    }

    fn bdv_test_meta() -> ImageMetadata {
        ImageMetadata {
            size_x: 4,
            size_y: 3,
            size_z: 2,
            size_c: 2,
            size_t: 2,
            pixel_type: PixelType::Uint8,
            bits_per_pixel: 8,
            image_count: 8,
            dimension_order: DimensionOrder::XYZTC,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: true,
            is_little_endian: true,
            resolution_count: 1,
            thumbnail: false,
            series_metadata: HashMap::new(),
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        }
    }

    #[test]
    fn bdv_plane_index_maps_xyz_tc_to_timepoint_and_channel_setup() {
        let mut r = BdvReader::new();
        r.series.push(SeriesInfo {
            setup: 10,
            channel_setups: vec![10, 11],
            timepoints: vec![3, 5],
            level: 2,
            meta: bdv_test_meta(),
            voxel_size: None,
        });

        assert_eq!(r.image_data_path(0).unwrap(), "t00003/s10/2/cells");
        assert_eq!(r.image_data_path(1).unwrap(), "t00003/s10/2/cells");
        assert_eq!(r.image_data_path(2).unwrap(), "t00005/s10/2/cells");
        assert_eq!(r.image_data_path(4).unwrap(), "t00003/s11/2/cells");
        assert_eq!(r.image_data_path(6).unwrap(), "t00005/s11/2/cells");
    }

    #[test]
    fn bdv_channel_setup_lookup_uses_same_logical_setup_ordinal() {
        let xml = r#"<SpimData>
          <ViewSetup><id>0</id><attributes><channel>0</channel><angle>0</angle></attributes></ViewSetup>
          <ViewSetup><id>1</id><attributes><channel>1</channel><angle>0</angle></attributes></ViewSetup>
          <ViewSetup><id>2</id><attributes><channel>0</channel><angle>1</angle></attributes></ViewSetup>
          <ViewSetup><id>3</id><attributes><channel>1</channel><angle>1</angle></attributes></ViewSetup>
        </SpimData>"#;
        let mut r = BdvReader::new();
        r.parse_xml(Some(xml));

        assert_eq!(r.channel_setups_for_logical_index(0, 0), vec![0, 1]);
        assert_eq!(r.channel_setups_for_logical_index(1, 2), vec![2, 3]);
    }

    #[test]
    fn bdv_fixture_keeps_timepoints_in_size_t_not_series() {
        let path = Path::new("testdata/bdv/HisYFP-SPIM.xml");
        if !path.exists() {
            return;
        }

        let mut r = BdvReader::new();
        r.set_id(path).unwrap();

        assert_eq!(r.series_count(), 34);
        assert_eq!(r.metadata().size_t, 1);
        assert_eq!(r.metadata().size_c, 1);
        assert_eq!(r.metadata().resolution_count, 1);
        assert_eq!(r.metadata().image_count, r.metadata().size_z);
    }
}
