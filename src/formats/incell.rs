//! InCell GE Healthcare HCS reader (.xdce / .xml).
//!
//! Ported from the upstream Java InCellReader. The .xdce XML describes a plate
//! of wells, each with one or more fields, and per-plane Z/C/T structure plus
//! companion TIFF (or .im) pixel files. Each well/field combination becomes a
//! separate series.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use quick_xml::events::Event;
use quick_xml::Reader as XmlReader;

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata};
use crate::common::ome_metadata::{
    create_lsid, OmeChannel, OmeImage, OmeInstrument, OmeMetadata, OmeObjective, OmePlate, OmeWell,
    OmeWellSample,
};
use crate::common::path::confined_join;
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;

/// A single image plane referenced by the .xdce metadata.
#[derive(Clone, Default)]
struct ImagePlane {
    filename: Option<PathBuf>,
    is_tiff: bool,
}

pub struct InCellReader {
    path: Option<PathBuf>,
    // One ImageMetadata per series (well/field combination).
    series: Vec<ImageMetadata>,
    current_series: usize,
    // imageFiles[series][plane_index] -> ImagePlane.  Indexed in XYZCT order:
    // plane_index = z + c*sizeZ + t*sizeZ*sizeC.
    image_files: Vec<Vec<ImagePlane>>,
    // Number of fields per well (used to map series -> well/field).
    field_count: usize,
    // List of (row,col) wells that actually appear in the plate map, in order.
    plate_wells: Vec<(usize, usize)>,
    // Channels acquired at each timepoint (mirrors Java channelsPerTimepoint).
    channels_per_timepoint: Vec<u32>,
    // True when timepoints differ in channel count: each timepoint becomes a
    // separate series with its own channel count (Java oneTimepointPerSeries).
    one_timepoint_per_series: bool,
    tiff_reader: crate::tiff::TiffReader,
    tiff_loaded: bool,
    // Captured HCS/OME metadata (built into OmeMetadata on demand).
    hcs: HcsMeta,
}

/// Subset of parsed metadata retained for the OME metadata store.
#[derive(Default, Clone)]
struct HcsMeta {
    well_rows: usize,
    well_cols: usize,
    plate_name: String,
    row_name: String,
    col_name: String,
    channel_names: Vec<String>,
    em_waves: Vec<f64>,
    ex_waves: Vec<f64>,
    nominal_magnification: Option<f64>,
    lens_na: Option<f64>,
    physical_size_x: Option<f64>,
    physical_size_y: Option<f64>,
    pos_x: HashMap<usize, f64>,
    pos_y: HashMap<usize, f64>,
}

impl InCellReader {
    pub fn new() -> Self {
        InCellReader {
            path: None,
            series: Vec::new(),
            current_series: 0,
            image_files: Vec::new(),
            field_count: 1,
            plate_wells: Vec::new(),
            channels_per_timepoint: Vec::new(),
            one_timepoint_per_series: false,
            tiff_reader: crate::tiff::TiffReader::new(),
            tiff_loaded: false,
            hcs: HcsMeta::default(),
        }
    }
}

impl Default for InCellReader {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Default)]
struct InCellMeta {
    well_rows: usize,
    well_cols: usize,
    field_count: usize,
    size_z: u32,
    size_c: u32,
    size_t: u32,
    image_width: u32,
    image_height: u32,
    do_z: bool,
    do_t: bool,
    // plateMap[row][col] = a well exists here
    plate_map: Vec<Vec<bool>>,
    // exclude[row][col] = this well is explicitly excluded via <Exclude>
    exclude: Vec<Vec<bool>>,
    // imageFiles[well][field][t][index]
    image_files: Vec<Vec<Vec<Vec<Option<ImagePlane>>>>>,
    total_images: usize,
    // Channels acquired at each timepoint (mirrors Java channelsPerTimepoint).
    // When timepoints differ in channel count, oneTimepointPerSeries kicks in.
    channels_per_timepoint: Vec<u32>,

    // Metadata-store fields (mirrors the Java InCellHandler).
    row_name: String,
    col_name: String,
    plate_name: String,
    channel_names: Vec<String>,
    em_waves: Vec<f64>,
    ex_waves: Vec<f64>,
    nominal_magnification: Option<f64>,
    lens_na: Option<f64>,
    physical_size_x: Option<f64>,
    physical_size_y: Option<f64>,
    // posX/posY keyed by field index (offset_point), in reference-frame units.
    pos_x: HashMap<usize, f64>,
    pos_y: HashMap<usize, f64>,
}

fn attr_val(e: &quick_xml::events::BytesStart, name: &str) -> Option<String> {
    for a in e.attributes().flatten() {
        if a.key.as_ref() == name.as_bytes() {
            return Some(String::from_utf8_lossy(&a.value).to_string());
        }
    }
    None
}

fn attr_int(e: &quick_xml::events::BytesStart, name: &str) -> Option<i64> {
    attr_val(e, name).and_then(|s| s.trim().parse::<i64>().ok())
}

fn attr_nonnegative_u32(e: &quick_xml::events::BytesStart, name: &str) -> Result<u32> {
    let value = attr_int(e, name).unwrap_or(0);
    if value < 0 {
        return Err(BioFormatsError::Format(format!(
            "InCell attribute {name} must be non-negative, got {value}"
        )));
    }
    Ok(value as u32)
}

fn attr_positive_u32(e: &quick_xml::events::BytesStart, name: &str) -> Result<u32> {
    let value = attr_int(e, name).unwrap_or(0);
    if value <= 0 {
        return Err(BioFormatsError::Format(format!(
            "InCell attribute {name} must be positive, got {value}"
        )));
    }
    Ok(value as u32)
}

fn attr_f64(e: &quick_xml::events::BytesStart, name: &str) -> Option<f64> {
    attr_val(e, name).and_then(|s| s.trim().parse::<f64>().ok())
}

/// Parse the .xdce / .xml metadata into a usable structure (mirrors Java
/// MinimalInCellHandler).
fn parse_incell_xml(path: &Path) -> Result<InCellMeta> {
    let content = std::fs::read_to_string(path).map_err(BioFormatsError::Io)?;
    let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();

    let mut m = InCellMeta {
        do_z: true,
        do_t: true,
        row_name: "A".to_string(),
        col_name: "1".to_string(),
        ..Default::default()
    };
    // Plate name = the input file name without directory or extension.
    m.plate_name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();
    // offset_point index counter (when no explicit index is given).
    let mut offset_point_counter: usize = 0;

    // Current parse state.
    let mut current_image_file: Option<PathBuf> = None;
    let mut well_row: usize = 0;
    let mut well_col: usize = 0;
    let mut channels_per_timepoint: Vec<u32> = Vec::new();
    let mut n_channels: u32 = 0;
    let mut allocated = false;

    let mut reader = XmlReader::from_str(&content);
    reader.config_mut().trim_text(false);

    loop {
        let ev = reader
            .read_event()
            .map_err(|e| BioFormatsError::Format(format!("InCell XML parse error: {e}")))?;
        match ev {
            Event::Eof => break,
            Event::Start(ref e) | Event::Empty(ref e) => {
                let qname = e.name();
                let qname = qname.as_ref();
                match qname {
                    b"Plate" => {
                        m.well_rows = attr_positive_u32(e, "rows")? as usize;
                        m.well_cols = attr_positive_u32(e, "columns")? as usize;
                        m.plate_map = vec![vec![false; m.well_cols]; m.well_rows];
                        m.exclude = vec![vec![false; m.well_cols]; m.well_rows];
                    }
                    b"Exclude" => {
                        // Java InCellReader.java:897-901: <Exclude row=… col=…>,
                        // attributes are 1-indexed (subtract 1) and mark a well
                        // to be dropped from the series list.
                        if m.exclude.is_empty() {
                            m.exclude = vec![vec![false; m.well_cols]; m.well_rows];
                        }
                        let row = attr_positive_u32(e, "row")? as usize - 1;
                        let col = attr_positive_u32(e, "col")? as usize - 1;
                        if row < m.well_rows && col < m.well_cols {
                            m.exclude[row][col] = true;
                        } else {
                            return Err(BioFormatsError::Format(format!(
                                "InCell Exclude well ({}, {}) is outside declared plate dimensions {}x{}",
                                row + 1,
                                col + 1,
                                m.well_rows,
                                m.well_cols
                            )));
                        }
                    }
                    b"Images" => {
                        // imagesNumber - not strictly needed for assembly
                    }
                    b"Image" => {
                        m.total_images += 1;
                        current_image_file = None;
                        if let Some(file) = attr_val(e, "filename") {
                            current_image_file =
                                Some(confined_join(&dir, &file).ok_or_else(|| {
                                    BioFormatsError::Format(format!(
                                        "InCell companion filename escapes image directory: {file}"
                                    ))
                                })?);
                        }
                    }
                    b"Identifier" => {
                        let field = attr_nonnegative_u32(e, "field_index")? as usize;
                        let z = attr_nonnegative_u32(e, "z_index")?;
                        let c = attr_nonnegative_u32(e, "wave_index")?;
                        let t = attr_nonnegative_u32(e, "time_index")? as usize;

                        // channels per timepoint is read by Java but the plane
                        // index (with t=0) reduces to z + c*sizeZ regardless.
                        let _channels = channels_per_timepoint
                            .get(t)
                            .copied()
                            .unwrap_or(m.size_c.max(1));
                        let size_z = m.size_z.max(1);
                        // FormatTools.getIndex("XYZCT", ...) with z, c, t=0 => z + c*sizeZ
                        let index = (z + c * size_z) as usize;

                        let filename = current_image_file.clone();
                        let exists = filename.as_ref().map(|p| p.exists()).unwrap_or(false);
                        let is_tiff = filename
                            .as_ref()
                            .and_then(|p| p.extension())
                            .and_then(|e| e.to_str())
                            .map(|e| {
                                e.eq_ignore_ascii_case("tif") || e.eq_ignore_ascii_case("tiff")
                            })
                            .unwrap_or(false);
                        let plane = ImagePlane {
                            filename: if exists { filename } else { None },
                            is_tiff,
                        };

                        if !allocated {
                            allocate_image_files(&mut m, &channels_per_timepoint);
                            allocated = true;
                        }
                        let well = well_row * m.well_cols + well_col;
                        if let Some(w) = m.image_files.get_mut(well) {
                            if let Some(f) = w.get_mut(field) {
                                if let Some(tp) = f.get_mut(t) {
                                    if let Some(slot) = tp.get_mut(index) {
                                        *slot = Some(plane);
                                    }
                                }
                            }
                        }
                    }
                    b"offset_point" => {
                        // MinimalInCellHandler counts fields; InCellHandler also
                        // records per-field stage positions. We do both here.
                        m.field_count += 1;
                        let x = attr_f64(e, "x");
                        let y = attr_f64(e, "y");
                        let index = attr_int(e, "index").map(|v| v as usize).unwrap_or_else(|| {
                            let i = offset_point_counter;
                            offset_point_counter += 1;
                            i
                        });
                        if let Some(x) = x {
                            m.pos_x.insert(index, x);
                        }
                        if let Some(y) = y {
                            // negate Y to flip center-origin -> top-left origin
                            m.pos_y.insert(index, -y);
                        }
                    }
                    b"TimePoint" => {
                        if m.do_t {
                            m.size_t += 1;
                        }
                    }
                    b"Wavelength" => {
                        let fusion = attr_val(e, "fusion_wave").unwrap_or_default();
                        if fusion == "false" {
                            m.size_c += 1;
                        }
                        if let Some(mode) = attr_val(e, "imaging_mode") {
                            let is_3d = mode == "3-D";
                            if m.size_c == 1 || !m.do_z {
                                m.do_z = is_3d;
                            }
                        }
                    }
                    b"AcqWave" => {
                        n_channels += 1;
                    }
                    b"ZDimensionParameters" => {
                        if let Some(nz) = attr_int(e, "number_of_slices") {
                            if nz <= 0 {
                                return Err(BioFormatsError::Format(format!(
                                    "InCell attribute number_of_slices must be positive, got {nz}"
                                )));
                            }
                            if m.do_z {
                                m.size_z = nz as u32;
                            } else {
                                m.size_z = 1;
                            }
                        } else {
                            m.size_z = 1;
                        }
                    }
                    b"Row" => {
                        let row = attr_positive_u32(e, "number")? as usize - 1;
                        if !m.plate_map.is_empty() && row >= m.well_rows {
                            return Err(BioFormatsError::Format(format!(
                                "InCell row {} is outside declared plate rows {}",
                                row + 1,
                                m.well_rows
                            )));
                        }
                        well_row = row;
                    }
                    b"Column" => {
                        let col = attr_positive_u32(e, "number")? as usize - 1;
                        if !m.plate_map.is_empty() && col >= m.well_cols {
                            return Err(BioFormatsError::Format(format!(
                                "InCell column {} is outside declared plate columns {}",
                                col + 1,
                                m.well_cols
                            )));
                        }
                        well_col = col;
                        if well_row < m.plate_map.len() && well_col < m.well_cols {
                            m.plate_map[well_row][well_col] = true;
                        }
                    }
                    b"Size" => {
                        m.image_width = attr_positive_u32(e, "width")?;
                        m.image_height = attr_positive_u32(e, "height")?;
                    }
                    b"NamingRows" => {
                        if let Some(begin) = attr_val(e, "begin") {
                            m.row_name = begin;
                        }
                    }
                    b"NamingColumns" => {
                        if let Some(begin) = attr_val(e, "begin") {
                            m.col_name = begin;
                        }
                    }
                    b"ObjectiveCalibration" => {
                        m.nominal_magnification = attr_f64(e, "magnification");
                        m.lens_na = attr_f64(e, "numerical_aperture");
                        m.physical_size_x = attr_f64(e, "pixel_width");
                        m.physical_size_y = attr_f64(e, "pixel_height");
                    }
                    b"ExcitationFilter" => {
                        if let Some(w) = attr_f64(e, "wavelength") {
                            m.ex_waves.push(w);
                        }
                    }
                    b"EmissionFilter" => {
                        if let Some(w) = attr_f64(e, "wavelength") {
                            m.em_waves.push(w);
                        }
                        if let Some(name) = attr_val(e, "name") {
                            m.channel_names.push(name);
                        }
                    }
                    b"TimeSchedule" => {
                        m.do_t = attr_val(e, "enabled").map(|v| v == "true").unwrap_or(true);
                    }
                    _ => {}
                }
            }
            Event::End(ref e) => {
                let qname = e.name();
                match qname.as_ref() {
                    b"Image" => {
                        current_image_file = None;
                    }
                    b"PlateMap" => {
                        // End of the plate map: allocate the imageFiles array now,
                        // mirroring Java MinimalInCellHandler.endElement.
                        if m.size_t == 0 {
                            m.size_t = 1;
                        }
                        if channels_per_timepoint.is_empty() {
                            channels_per_timepoint.push(m.size_c.max(1));
                        }
                        allocate_image_files(&mut m, &channels_per_timepoint);
                        allocated = true;
                    }
                    b"TimePoint" => {
                        if m.do_t {
                            channels_per_timepoint.push(n_channels);
                            n_channels = 0;
                        }
                    }
                    b"Times" => {
                        if channels_per_timepoint.is_empty() {
                            channels_per_timepoint.push(m.size_c.max(1));
                        }
                        for c in channels_per_timepoint.iter_mut() {
                            if *c == 0 {
                                *c = m.size_c.max(1);
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    if m.size_z == 0 {
        m.size_z = 1;
    }
    if m.size_c == 0 {
        m.size_c = 1;
    }
    if m.size_t == 0 {
        m.size_t = 1;
    }
    if m.field_count == 0 {
        m.field_count = 1;
    }
    if channels_per_timepoint.is_empty() {
        channels_per_timepoint.push(m.size_c.max(1));
    }
    m.channels_per_timepoint = channels_per_timepoint;

    Ok(m)
}

/// Allocate the imageFiles[well][field][t][channels*z] structure.
fn allocate_image_files(m: &mut InCellMeta, channels_per_timepoint: &[u32]) {
    let wells = (m.well_rows * m.well_cols).max(1);
    let fields = m.field_count.max(1);
    let size_t = m.size_t.max(1) as usize;
    let size_z = m.size_z.max(1);
    m.image_files = (0..wells)
        .map(|_| {
            (0..fields)
                .map(|_| {
                    (0..size_t)
                        .map(|t| {
                            let channels = channels_per_timepoint
                                .get(t)
                                .copied()
                                .unwrap_or(m.size_c.max(1));
                            vec![None; (channels * size_z) as usize]
                        })
                        .collect()
                })
                .collect()
        })
        .collect();
}

fn iter_image_planes(m: &InCellMeta) -> impl Iterator<Item = &ImagePlane> {
    m.image_files
        .iter()
        .flat_map(|well| well.iter())
        .flat_map(|field| field.iter())
        .flat_map(|timepoint| timepoint.iter())
        .filter_map(|plane| plane.as_ref())
}

fn validate_incell_companions(m: &InCellMeta) -> Result<()> {
    let mut has_existing_companion = false;
    let mut has_tiff_companion = false;
    let mut has_im_companion = false;
    for plane in iter_image_planes(m) {
        if plane.filename.is_some() {
            has_existing_companion = true;
            has_tiff_companion |= plane.is_tiff;
            has_im_companion |= !plane.is_tiff;
        }
    }
    if !has_existing_companion {
        return Err(BioFormatsError::UnsupportedFormat(
            "InCell XML/XDCE does not reference any existing companion image files".into(),
        ));
    }
    if has_tiff_companion {
        for plane in iter_image_planes(m) {
            if !plane.is_tiff {
                continue;
            }
            let Some(path) = &plane.filename else {
                continue;
            };
            let mut tr = crate::tiff::TiffReader::new();
            tr.set_id(path).map_err(|e| {
                BioFormatsError::Format(format!(
                    "InCell companion TIFF {} could not be initialized: {e}",
                    path.display()
                ))
            })?;
            let meta = tr.metadata();
            if meta.size_x == 0 || meta.size_y == 0 || meta.image_count == 0 {
                return Err(BioFormatsError::Format(format!(
                    "InCell companion TIFF {} has invalid image metadata",
                    path.display()
                )));
            }
            let _ = tr.close();
        }
    }
    if has_im_companion {
        if m.image_width == 0 || m.image_height == 0 {
            return Err(BioFormatsError::Format(
                "InCell .im companion metadata is missing positive image dimensions".into(),
            ));
        }
        let plane_bytes = (m.image_width as u64)
            .checked_mul(m.image_height as u64)
            .and_then(|v| v.checked_mul(2))
            .ok_or_else(|| BioFormatsError::Format("InCell .im plane size overflows".into()))?;
        let required_len = 128u64
            .checked_add(plane_bytes)
            .ok_or_else(|| BioFormatsError::Format("InCell .im payload size overflows".into()))?;
        for plane in iter_image_planes(m) {
            if !plane.is_tiff {
                let Some(path) = &plane.filename else {
                    continue;
                };
                let len = std::fs::metadata(path).map_err(BioFormatsError::Io)?.len();
                if len < required_len {
                    return Err(BioFormatsError::Format(format!(
                        "InCell .im companion {} is shorter than declared payload: need {required_len} bytes, file length {len}",
                        path.display()
                    )));
                }
            }
        }
    }
    if !has_tiff_companion && !has_im_companion {
        return Err(BioFormatsError::UnsupportedFormat(
            "InCell XML/XDCE does not reference any supported companion image files".into(),
        ));
    }
    Ok(())
}

impl InCellReader {
    /// Build the series list and a flat per-series plane lookup from parsed metadata.
    fn build(&mut self, m: InCellMeta) -> Result<()> {
        let size_z = m.size_z.max(1);
        let size_c = m.size_c.max(1);
        let size_t = m.size_t.max(1);

        // Determine the ordered list of populated wells (matches getWellFromSeries).
        // Wells explicitly listed in <Exclude> are dropped, mirroring Java
        // InCellReader.java:477-498 where excluded wells are skipped when
        // computing seriesCount. We remove them from the well list entirely so
        // both the series count and the well->series mapping stay consistent.
        let mut plate_wells: Vec<(usize, usize)> = Vec::new();
        for row in 0..m.well_rows {
            for col in 0..m.well_cols {
                let populated = m
                    .plate_map
                    .get(row)
                    .and_then(|r| r.get(col))
                    .copied()
                    .unwrap_or(false);
                let excluded = m
                    .exclude
                    .get(row)
                    .and_then(|r| r.get(col))
                    .copied()
                    .unwrap_or(false);
                if populated && !excluded {
                    plate_wells.push((row, col));
                }
            }
        }
        if plate_wells.is_empty() {
            // No plate map: assume a single well at (0,0).
            plate_wells.push((0, 0));
        }
        let field_count = m.field_count.max(1);

        // Detect variable channels-per-timepoint (Java InCellReader.java:519-533).
        // When timepoints differ in channel count, each timepoint becomes its
        // own series with a per-series channel count.
        let channels_per_timepoint: Vec<u32> = if m.channels_per_timepoint.is_empty() {
            vec![size_c]
        } else {
            m.channels_per_timepoint.clone()
        };
        let one_timepoint_per_series =
            channels_per_timepoint.windows(2).any(|w| w[0] != w[1]);

        // Number of (well, field) combinations.
        let well_field_count = plate_wells.len() * field_count;
        // sizeT used for the channelsPerTimepoint index space. When timepoints
        // differ, Java indexes by channelsPerTimepoint.size(); otherwise sizeT.
        let cpt_len = channels_per_timepoint.len().max(1);
        let series_count = if one_timepoint_per_series {
            // Java: seriesCount = (totalImages / imageCount) * sizeT, where the
            // (totalImages/imageCount) factor is the number of well/field combos.
            well_field_count * size_t as usize
        } else {
            well_field_count
        };

        // Determine pixel parameters from the first available TIFF plane.
        let mut size_x = m.image_width;
        let mut size_y = m.image_height;
        let mut pixel_type = PixelType::Uint16;
        let mut bits = 16u8;
        let mut little_endian = true;
        let mut is_tiff_first = false;

        'find: for well in &m.image_files {
            for field in well {
                for tp in field {
                    for plane in tp {
                        if let Some(p) = plane {
                            if let Some(fname) = &p.filename {
                                if p.is_tiff {
                                    let mut tr = crate::tiff::TiffReader::new();
                                    if tr.set_id(fname).is_ok() {
                                        let tm = tr.metadata();
                                        size_x = tm.size_x;
                                        size_y = tm.size_y;
                                        pixel_type = tm.pixel_type;
                                        bits = tm.bits_per_pixel;
                                        little_endian = tm.is_little_endian;
                                        is_tiff_first = true;
                                        let _ = tr.close();
                                        break 'find;
                                    }
                                    let _ = tr.close();
                                }
                            }
                        }
                    }
                }
            }
        }
        let _ = is_tiff_first;
        if size_x == 0 || size_y == 0 {
            return Err(BioFormatsError::Format(
                "InCell metadata is missing positive image dimensions".into(),
            ));
        }

        // Build per-series metadata and the flat plane lookup.
        let mut series = Vec::with_capacity(series_count);
        let mut image_files = Vec::with_capacity(series_count);
        for s in 0..series_count {
            // Map the series index to (well, field, timepoint range) following
            // Java getWellFromSeries / getFieldFromSeries / openBytes.
            let (well_idx, field, series_size_c, series_size_t, t_base) = if one_timepoint_per_series
            {
                // Each timepoint is its own series: series order is well-major,
                // then field, then timepoint (fastest). See Java lines 519-552,
                // 807-829: getFieldFromSeries divides by channelsPerTimepoint.size().
                let s2 = s / cpt_len;
                let timepoint = s % cpt_len;
                let (well, fld) =
                    series_to_well_field(s2, &plate_wells, field_count, m.well_cols);
                let c = channels_per_timepoint
                    .get(timepoint)
                    .copied()
                    .unwrap_or(size_c);
                (well, fld, c, 1u32, timepoint as u32)
            } else {
                let (well, fld) =
                    series_to_well_field(s, &plate_wells, field_count, m.well_cols);
                (well, fld, size_c, size_t, 0u32)
            };

            let mut meta_map = HashMap::new();
            meta_map.insert(
                "format".to_string(),
                crate::common::metadata::MetadataValue::String("InCell".into()),
            );
            let meta = ImageMetadata {
                size_x,
                size_y,
                size_z,
                size_c: series_size_c,
                size_t: series_size_t,
                pixel_type,
                bits_per_pixel: bits,
                image_count: size_z * series_size_c * series_size_t,
                dimension_order: DimensionOrder::XYZCT,
                is_rgb: false,
                is_interleaved: false,
                is_indexed: false,
                is_little_endian: little_endian,
                resolution_count: 1,
                series_metadata: meta_map,
                lookup_table: None,
                modulo_z: None,
                modulo_c: None,
                modulo_t: None,
            };
            series.push(meta);

            // Flatten imageFiles[well][field][t][z+c*sizeZ] into XYZCT plane order.
            // For oneTimepointPerSeries the series carries a single timepoint
            // (t_base) and series_size_t == 1.
            let mut planes =
                vec![ImagePlane::default(); (size_z * series_size_c * series_size_t) as usize];
            if let Some(well) = m.image_files.get(well_idx) {
                if let Some(field_planes) = well.get(field) {
                    for t in 0..series_size_t {
                        let src_t = (t_base + t) as usize;
                        let tp = field_planes.get(src_t);
                        for c in 0..series_size_c {
                            for z in 0..size_z {
                                let src_index = (z + c * size_z) as usize;
                                let dst =
                                    (z + c * size_z + t * size_z * series_size_c) as usize;
                                if let Some(Some(p)) =
                                    tp.and_then(|tp| tp.get(src_index)).map(|p| p.as_ref())
                                {
                                    planes[dst] = p.clone();
                                }
                            }
                        }
                    }
                }
            }
            image_files.push(planes);
        }

        self.hcs = HcsMeta {
            well_rows: m.well_rows,
            well_cols: m.well_cols,
            plate_name: m.plate_name.clone(),
            row_name: m.row_name.clone(),
            col_name: m.col_name.clone(),
            channel_names: m.channel_names.clone(),
            em_waves: m.em_waves.clone(),
            ex_waves: m.ex_waves.clone(),
            nominal_magnification: m.nominal_magnification,
            lens_na: m.lens_na,
            physical_size_x: m.physical_size_x,
            physical_size_y: m.physical_size_y,
            pos_x: m.pos_x.clone(),
            pos_y: m.pos_y.clone(),
        };

        self.series = series;
        self.image_files = image_files;
        self.field_count = field_count;
        self.plate_wells = plate_wells;
        self.channels_per_timepoint = channels_per_timepoint;
        self.one_timepoint_per_series = one_timepoint_per_series;
        Ok(())
    }
}

/// Map a flat series index to (well array index, field), matching Java
/// getWellFromSeries / getFieldFromSeries for the uniform case.
fn series_to_well_field(
    series: usize,
    plate_wells: &[(usize, usize)],
    field_count: usize,
    well_cols: usize,
) -> (usize, usize) {
    let well_ordinal = series / field_count;
    let field = series % field_count;
    let (row, col) = plate_wells.get(well_ordinal).copied().unwrap_or((0, 0));
    (row * well_cols.max(1) + col, field)
}

impl FormatReader for InCellReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        if matches!(ext.as_deref(), Some("xdce")) {
            return true;
        }
        if matches!(ext.as_deref(), Some("xml")) {
            if let Ok(data) = std::fs::read(path) {
                let snippet = std::str::from_utf8(&data[..data.len().min(512)]).unwrap_or("");
                return snippet.contains("<InCell") || snippet.contains("xdce");
            }
        }
        false
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        let snippet = std::str::from_utf8(&header[..header.len().min(512)]).unwrap_or("");
        snippet.contains("<InCell") || snippet.contains("xdce")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let m = parse_incell_xml(path)?;
        if m.total_images == 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "InCell XML/XDCE does not reference any companion TIFF image files".into(),
            ));
        }
        validate_incell_companions(&m)?;
        self.build(m)?;
        if self.series.is_empty() {
            return Err(BioFormatsError::UnsupportedFormat(
                "InCell XML/XDCE produced no series".into(),
            ));
        }
        self.path = Some(path.to_path_buf());
        self.current_series = 0;
        self.tiff_loaded = false;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.series.clear();
        self.image_files.clear();
        self.plate_wells.clear();
        self.channels_per_timepoint.clear();
        self.one_timepoint_per_series = false;
        self.field_count = 1;
        self.current_series = 0;
        self.hcs = HcsMeta::default();
        if self.tiff_loaded {
            let _ = self.tiff_reader.close();
            self.tiff_loaded = false;
        }
        Ok(())
    }

    fn series_count(&self) -> usize {
        self.series.len()
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.series.is_empty() {
            Err(BioFormatsError::NotInitialized)
        } else if s >= self.series.len() {
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
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self
            .series
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let plane_bytes =
            meta.size_x as usize * meta.size_y as usize * meta.pixel_type.bytes_per_sample();
        let size_z = meta.size_z.max(1);

        let plane = self
            .image_files
            .get(self.current_series)
            .and_then(|p| p.get(plane_index as usize))
            .cloned()
            .unwrap_or_default();

        // Resolve the plane to read. When the requested plane is missing and it
        // is a Z>0 section, duplicate the corresponding Z=0 plane (Java
        // InCellReader.java:208-213, duplicatePlanes() defaults to true).
        let plane = if plane.filename.is_none() {
            let z = plane_index % size_z;
            if z > 0 {
                let z0_index = plane_index - z;
                self.image_files
                    .get(self.current_series)
                    .and_then(|p| p.get(z0_index as usize))
                    .cloned()
                    .unwrap_or_default()
            } else {
                plane
            }
        } else {
            plane
        };

        let Some(tiff_path) = plane.filename else {
            // Missing plane (and no Z=0 fallback): return zero-filled.
            // Java returns the unmodified (zeroed) buffer.
            return Ok(vec![0u8; plane_bytes]);
        };

        if plane.is_tiff {
            if self.tiff_loaded {
                let _ = self.tiff_reader.close();
            }
            self.tiff_reader.set_id(&tiff_path)?;
            self.tiff_loaded = true;
            return self.tiff_reader.open_bytes(0);
        }

        // .im files: pixel data after a 128-byte header.
        use std::io::{Read, Seek, SeekFrom};
        let mut f = std::fs::File::open(&tiff_path).map_err(BioFormatsError::Io)?;
        let len = f.metadata().map_err(BioFormatsError::Io)?.len();
        let mut buf = vec![0u8; plane_bytes];
        let offset = 128u64;
        let end = offset.checked_add(plane_bytes as u64).ok_or_else(|| {
            BioFormatsError::InvalidData("InCell .im plane offset overflows".into())
        })?;
        if end > len {
            return Err(BioFormatsError::InvalidData(format!(
                "InCell .im plane exceeds file length: need bytes {offset}..{end}, file length {len}"
            )));
        }
        f.seek(SeekFrom::Start(offset))
            .map_err(BioFormatsError::Io)?;
        f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
        Ok(buf)
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
        let meta = self
            .series
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        crop_full_plane("InCell", &full, meta, 1, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self
            .series
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    /// Build the OME HCS metadata: one Plate with Wells/WellSamples mapping each
    /// series (well/field combination) to an Image, plus per-image channel and
    /// physical-size metadata. Mirrors `InCellReader.populateMetadataStore`.
    fn ome_metadata(&self) -> Option<OmeMetadata> {
        if self.series.is_empty() {
            return None;
        }
        let h = &self.hcs;
        let series_count = self.series.len();
        let field_count = self.field_count.max(1);
        let well_cols = h.well_cols.max(1);

        // A single instrument with one objective is shared by all images when
        // the objective calibration was present (mirrors Java InCellReader).
        let has_objective = h.nominal_magnification.is_some() || h.lens_na.is_some();
        let instruments = if has_objective {
            vec![OmeInstrument {
                id: Some(create_lsid("Instrument", &[0])),
                objectives: vec![OmeObjective {
                    id: Some(create_lsid("Objective", &[0, 0])),
                    nominal_magnification: h.nominal_magnification,
                    lens_na: h.lens_na,
                    ..Default::default()
                }],
                ..Default::default()
            }]
        } else {
            Vec::new()
        };

        // Per-series OmeImage (name, physical size, channels).
        let mut images: Vec<OmeImage> = Vec::with_capacity(series_count);
        // Under oneTimepointPerSeries, the series index is divided by the number
        // of timepoints before deriving the well/field (Java getWellFromSeries /
        // getFieldFromSeries, lines 807-829).
        let total_timepoints = if self.one_timepoint_per_series {
            self.channels_per_timepoint.len().max(1)
        } else {
            1
        };
        for s in 0..series_count {
            let well_field = s / total_timepoints;
            let well_ordinal = well_field / field_count;
            let field = well_field % field_count;
            let (well_row, well_col) = h_well_coords(self, well_ordinal);

            // Well label, mirroring the Java row/column naming logic.
            let row_label = format_well_label(&h.row_name, well_row);
            let col_label = format_well_label(&h.col_name, well_col);
            let name = format!("Well {}-{}, Field #{}", row_label, col_label, field + 1);

            // Per-series channel count (varies under oneTimepointPerSeries).
            let size_c = self.series.get(s).map(|m| m.size_c).unwrap_or(1) as usize;
            let mut channels = Vec::with_capacity(size_c);
            for q in 0..size_c {
                channels.push(OmeChannel {
                    name: h.channel_names.get(q).cloned(),
                    samples_per_pixel: 1,
                    color: None,
                    emission_wavelength: h.em_waves.get(q).copied(),
                    excitation_wavelength: h.ex_waves.get(q).copied(),
                });
            }

            images.push(OmeImage {
                name: Some(name),
                physical_size_x: h.physical_size_x.filter(|&v| v > 0.0),
                physical_size_y: h.physical_size_y.filter(|&v| v > 0.0),
                channels,
                instrument_ref: if has_objective { Some(0) } else { None },
                objective_ref: if has_objective { Some(0) } else { None },
                ..Default::default()
            });
        }

        // Build the Plate -> Wells -> WellSamples structure.
        // Wells are indexed by their ordinal in plate_wells (the populated wells).
        let mut wells: Vec<OmeWell> = Vec::with_capacity(self.plate_wells.len());
        for (well_ordinal, &(well_row, well_col)) in self.plate_wells.iter().enumerate() {
            let mut well_samples = Vec::with_capacity(field_count * total_timepoints);
            // Each (field, timepoint) maps to one series under oneTimepointPerSeries;
            // otherwise total_timepoints is 1 and this reduces to one per field.
            let mut sample = 0usize;
            for field in 0..field_count {
                for tp in 0..total_timepoints {
                    let series =
                        (well_ordinal * field_count + field) * total_timepoints + tp;
                    if series >= series_count {
                        continue;
                    }
                    well_samples.push(OmeWellSample {
                        id: Some(create_lsid("WellSample", &[0, well_ordinal, sample])),
                        index: series as u32,
                        image_ref: Some(series),
                        position_x: h.pos_x.get(&field).copied(),
                        position_y: h.pos_y.get(&field).copied(),
                    });
                    sample += 1;
                }
            }
            wells.push(OmeWell {
                id: Some(create_lsid("Well", &[0, well_ordinal])),
                row: well_row as u32,
                column: well_col as u32,
                well_samples,
            });
        }
        let _ = well_cols;

        let plate = OmePlate {
            id: Some(create_lsid("Plate", &[0])),
            name: if h.plate_name.is_empty() {
                None
            } else {
                Some(h.plate_name.clone())
            },
            rows: h.well_rows as u32,
            columns: h.well_cols as u32,
            wells,
        };

        Some(OmeMetadata {
            images,
            instruments,
            plates: vec![plate],
            ..Default::default()
        })
    }
}

/// Coordinates of the n-th populated well (matches `plate_wells` ordering).
fn h_well_coords(reader: &InCellReader, well_ordinal: usize) -> (usize, usize) {
    reader
        .plate_wells
        .get(well_ordinal)
        .copied()
        .unwrap_or((0, 0))
}

/// Format a well row/column label from a naming string such as "A" or "1",
/// offset by `index`. Mirrors the Java InCellReader naming logic: the last
/// character is treated as the base; if it is a digit, add `index`
/// numerically, otherwise advance the character.
fn format_well_label(naming: &str, index: usize) -> String {
    if naming.is_empty() {
        return (index + 1).to_string();
    }
    let Some(last) = naming.chars().last() else {
        return (index + 1).to_string();
    };
    let prefix: String = naming.chars().take(naming.chars().count() - 1).collect();
    if last.is_ascii_digit() {
        let base = last.to_digit(10).unwrap_or(0) as usize;
        format!("{}{}", prefix, index + base)
    } else {
        let ch = (last as u8).wrapping_add(index as u8) as char;
        format!("{}{}", prefix, ch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "bioformats_incell_test_{}_{}_{}",
            std::process::id(),
            nanos,
            name
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn initialized_reader() -> InCellReader {
        let mut reader = InCellReader::new();
        reader.series.push(ImageMetadata {
            size_x: 2,
            size_y: 2,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type: PixelType::Uint8,
            bits_per_pixel: 8,
            image_count: 1,
            ..Default::default()
        });
        reader.image_files.push(vec![ImagePlane::default()]);
        reader
    }

    #[test]
    fn open_bytes_region_rejects_out_of_bounds_without_panicking() {
        let mut reader = initialized_reader();

        let err = reader.open_bytes_region(0, 1, 0, 2, 1).unwrap_err();

        assert!(
            err.to_string().contains("outside image bounds"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn open_bytes_region_crops_missing_plane_zero_buffer() {
        let mut reader = initialized_reader();

        let crop = reader.open_bytes_region(0, 1, 0, 1, 2).unwrap();

        assert_eq!(crop, vec![0, 0]);
    }

    #[test]
    fn incell_rejects_companion_filename_that_escapes_directory() {
        for (name, filename) in [
            ("relative", "../outside.tif".to_string()),
            (
                "absolute",
                std::env::temp_dir()
                    .join("outside.tif")
                    .display()
                    .to_string(),
            ),
        ] {
            let dir = temp_dir(&format!("escape_{name}"));
            let xml = dir.join("plate.xdce");
            std::fs::write(
                &xml,
                format!(
                    r#"<InCell><Image filename="{filename}"><Identifier field_index="0" z_index="0" wave_index="0" time_index="0"/></Image></InCell>"#
                ),
            )
            .unwrap();

            let err = match parse_incell_xml(&xml) {
                Ok(_) => panic!("{name}: escaped InCell companion unexpectedly parsed"),
                Err(err) => err,
            };
            assert!(
                err.to_string().contains("escapes image directory"),
                "{name}: unexpected error: {err}"
            );
        }
    }

    #[test]
    fn incell_accepts_confined_relative_companion_filename() {
        let dir = temp_dir("relative_ok");
        let image_dir = dir.join("Images");
        std::fs::create_dir_all(&image_dir).unwrap();
        let image = image_dir.join("a.tif");
        std::fs::write(&image, []).unwrap();
        let xml = dir.join("plate.xdce");
        std::fs::write(
            &xml,
            r#"<InCell><Image filename="Images/a.tif"><Identifier field_index="0" z_index="0" wave_index="0" time_index="0"/></Image></InCell>"#,
        )
        .unwrap();

        let meta = parse_incell_xml(&xml).unwrap();
        let plane = meta.image_files[0][0][0][0].as_ref().unwrap();

        assert_eq!(plane.filename.as_ref(), Some(&image));
        assert!(plane.is_tiff);
    }

    #[test]
    fn incell_exclude_drops_well_from_series() {
        // 1x2 plate, both wells populated; well (row 1, col 2) is excluded.
        // Exclude attributes are 1-indexed, matching Java InCellReader.
        let xml = r#"<InCell>
            <Plate rows="1" columns="2"/>
            <Exclude row="1" col="2"/>
            <Row number="1"><Column number="1"/></Row>
            <Row number="1"><Column number="2"/></Row>
        </InCell>"#;
        let m = {
            let dir = temp_dir("exclude");
            let path = dir.join("plate.xdce");
            std::fs::write(&path, xml).unwrap();
            parse_incell_xml(&path).unwrap()
        };
        // Both wells are populated in the plate map.
        assert!(m.plate_map[0][0]);
        assert!(m.plate_map[0][1]);
        // Only the second well (1-indexed row=1,col=2) is excluded.
        assert!(!m.exclude[0][0]);
        assert!(m.exclude[0][1]);
    }
}
