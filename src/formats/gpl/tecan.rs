//! Tecan Spark Cyto reader.
//!
//! Line-by-line port of `loci.formats.in.TecanReader`
//! (`components/formats-gpl/src/loci/formats/in/TecanReader.java`). Methods
//! appear in the same order as the Java source so the two files can be audited
//! side by side.
//!
//! A dataset is a SQLite `.db` file in a `Database` directory, with sibling
//! `Images` (TIFF planes) and optional `Export` (analysis output) directories.
//!
//! Java library adapters (the only non-Java helpers, per the translation
//! parity rule): `rusqlite` replaces JDBC/`SQLiteConfig`, `crate::tiff::TiffReader`
//! replaces `MinimalTiffReader`, `leica_lms::parse_dom` replaces
//! `XMLTools.parseDOM`, the `zip` crate replaces `ZipInputStream`/`ZipHandle`,
//! and `get_elements_by_tag_name`, `date_tools_format_date`,
//! `get_physical_size_nm`, `get_zct_coords`, `add_meta` and
//! `update_metadata_lists` mirror the Java DOM / `DateTools` / `FormatTools` /
//! `FormatReader` calls of the same names.
//!
//! The SQLite-backed body is compiled only with the `tecan` cargo feature
//! (default on). Without it, `set_id` reports `UnsupportedFormat`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{ImageMetadata, MetadataLevel, MetadataOptions, MetadataValue};
use crate::common::ome_metadata::OmeMetadata;
use crate::common::reader::FormatReader;

// -- Constants --

#[cfg_attr(not(feature = "tecan"), allow(dead_code))]
const SPREADSHEET_KEYS: [&str; 7] = [
    "Application:",
    "Device",
    "Firmware",
    "System",
    "User",
    "Smooth mode",
    "Part of Plate",
];

/// Java `FormatReader.metadata` / `CoreMetadata.seriesMetadata` value: either a
/// single value or the `Vector` built by `addMetaList`.
#[derive(Clone, Debug)]
#[cfg_attr(not(feature = "tecan"), allow(dead_code))]
enum MetaEntry {
    Value(MetadataValue),
    List(Vec<MetadataValue>),
}

/// Java inner class `Image`.
#[derive(Clone, Debug)]
#[cfg_attr(not(feature = "tecan"), allow(dead_code))]
struct Image {
    file: Option<String>,
    well_row: i32,
    well_column: i32,
    field: i32,
    cycle: i32,
    series: i32,
    plane: i32,
    /// `Length` in micrometres.
    pixel_size: Option<f64>,
    /// `Time` in seconds.
    exposure_time: Option<f64>,
    channel_name: Option<String>,
    overlay: bool,
    result: bool,
    timestamp: Option<String>,
}

#[cfg_attr(not(feature = "tecan"), allow(dead_code))]
impl Image {
    fn new() -> Self {
        Image {
            file: None,
            well_row: -1,
            well_column: -1,
            field: 1,
            cycle: -1,
            series: -1,
            plane: -1,
            pixel_size: None,
            exposure_time: None,
            channel_name: None,
            overlay: false,
            result: false,
            timestamp: None,
        }
    }
}

/// Java inner class `Channel`.
#[derive(Clone, Debug)]
#[cfg_attr(not(feature = "tecan"), allow(dead_code))]
struct Channel {
    name: String,
    original_name: Option<String>,
    intensity: f64,
    focus_offset: f64,
    exposure_time: f64,
}

#[cfg_attr(not(feature = "tecan"), allow(dead_code))]
impl Channel {
    fn new(name: String) -> Self {
        Channel {
            name,
            original_name: None,
            intensity: 0.0,
            focus_offset: 0.0,
            exposure_time: 0.0,
        }
    }
}

/// Tecan Spark Cyto reader (`.db`).
pub struct TecanReader {
    // -- FormatReader state --
    current_id: Option<PathBuf>,
    core: Vec<ImageMetadata>,
    series: usize,
    metadata_options: MetadataOptions,
    /// Java `FormatReader.metadata` (global metadata table).
    metadata: HashMap<String, MetaEntry>,
    /// Java `CoreMetadata.seriesMetadata`, one table per series.
    series_meta: Vec<HashMap<String, MetaEntry>>,
    /// The populated Java `MetadataStore`.
    store: Option<OmeMetadata>,

    // -- Fields --
    plate_rows: i32,
    plate_columns: i32,
    plate_name: Option<String>,
    images: Vec<Image>,
    helper_reader: Option<crate::tiff::TiffReader>,
    /// Java `helperReader.getCurrentFile()`; `setId` on the current file is a
    /// no-op in Java's `FormatReader.setId`.
    helper_id: Option<PathBuf>,
    channels: Vec<Channel>,
    image_directory: Option<PathBuf>,
    extra_files: Vec<PathBuf>,
    max_field: i32,
    max_cycle: i32,
}

// -- Constructor --

impl TecanReader {
    /// Constructs a new Tecan Spark Cyto reader.
    pub fn new() -> Self {
        TecanReader {
            current_id: None,
            core: Vec::new(),
            series: 0,
            metadata_options: MetadataOptions::default(),
            metadata: HashMap::new(),
            series_meta: Vec::new(),
            store: None,
            plate_rows: 0,
            plate_columns: 0,
            plate_name: None,
            images: Vec::new(),
            helper_reader: None,
            helper_id: None,
            channels: Vec::new(),
            image_directory: None,
            extra_files: Vec::new(),
            max_field: 1,
            max_cycle: 1,
        }
    }
}

impl Default for TecanReader {
    fn default() -> Self {
        Self::new()
    }
}

/// Java `checkSuffix(name, "db")`.
fn check_suffix(name: &Path, suffix: &str) -> bool {
    name.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case(suffix))
}

// -- IFormatReader API methods --

impl FormatReader for TecanReader {
    /* @see loci.formats.IFormatReader#isThisType(String, boolean) */
    fn is_this_type_by_name(&self, name: &Path) -> bool {
        if !check_suffix(name, "db") {
            return false;
        }
        // Java isThisType(name, open=true): suffixSufficient is false, so the
        // database must open and expose a PlateDefinition table.
        #[cfg(feature = "tecan")]
        {
            let Ok(conn) = imp::open_connection(name) else {
                return false;
            };
            let mut probe = TecanReader::new();
            probe.find_plate_dimensions(&conn).is_ok()
        }
        #[cfg(not(feature = "tecan"))]
        {
            false
        }
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_metadata_options(&mut self, options: MetadataOptions) {
        self.metadata_options = options;
    }

    #[cfg(feature = "tecan")]
    fn set_id(&mut self, id: &Path) -> Result<()> {
        self.close()?;
        let result = self.init_file(id);
        if result.is_err() {
            self.close()?;
        }
        result
    }

    #[cfg(not(feature = "tecan"))]
    fn set_id(&mut self, _id: &Path) -> Result<()> {
        Err(BioFormatsError::UnsupportedFormat(
            "Tecan Spark Cyto (.db) requires the 'tecan' cargo feature \
             (SQLite-backed reader): cargo build --features tecan"
                .into(),
        ))
    }

    /* @see loci.formats.IFormatReader#close(boolean) */
    fn close(&mut self) -> Result<()> {
        // super.close(fileOnly)
        self.current_id = None;
        self.core.clear();
        self.series = 0;
        self.metadata.clear();
        self.series_meta.clear();
        self.store = None;
        if let Some(helper) = self.helper_reader.as_mut() {
            helper.close()?;
        }
        // !fileOnly
        self.images.clear();
        self.channels.clear();
        self.image_directory = None;
        self.plate_rows = 0;
        self.plate_columns = 0;
        self.plate_name = None;
        self.helper_reader = None;
        self.helper_id = None;
        self.extra_files.clear();
        self.max_field = 1;
        self.max_cycle = 1;
        Ok(())
    }

    fn series_count(&self) -> usize {
        self.core.len()
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.current_id.is_none() {
            return Err(BioFormatsError::NotInitialized);
        }
        if s >= self.core.len() {
            return Err(BioFormatsError::SeriesOutOfRange(s));
        }
        self.series = s;
        Ok(())
    }

    fn series(&self) -> usize {
        self.series
    }

    fn metadata(&self) -> &ImageMetadata {
        self.core
            .get(self.series)
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, no: u32) -> Result<Vec<u8>> {
        let (w, h) = {
            let m = self.metadata();
            (m.size_x, m.size_y)
        };
        self.open_bytes_region(no, 0, 0, w, h)
    }

    /**
     * @see loci.formats.IFormatReader#openBytes(int, byte[], int, int, int, int)
     */
    fn open_bytes_region(&mut self, no: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        // FormatTools.checkPlaneParameters(this, no, buf.length, x, y, w, h);
        let m = self
            .core
            .get(self.series)
            .ok_or(BioFormatsError::NotInitialized)?;
        if no >= m.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(no));
        }
        if x.checked_add(w).is_none_or(|e| e > m.size_x)
            || y.checked_add(h).is_none_or(|e| e > m.size_y)
        {
            return Err(BioFormatsError::Format(format!(
                "Tecan: invalid region {x},{y} {w}x{h} for {}x{} plane",
                m.size_x, m.size_y
            )));
        }
        let rgb_channel_count = if m.is_rgb {
            m.size_c / (m.image_count / (m.size_z * m.size_t)).max(1)
        } else {
            1
        };
        let len = w as usize
            * h as usize
            * m.pixel_type.bytes_per_sample()
            * rgb_channel_count.max(1) as usize;

        // Arrays.fill(buf, getFillColor());
        let mut buf = vec![0u8; len];
        let img = self
            .lookup_image(self.series as i32, no as i32, false, false)
            .and_then(|i| self.images[i].file.clone());
        if let Some(file) = img {
            if self.helper_reader.is_none() {
                self.helper_reader = Some(crate::tiff::TiffReader::new());
            }
            let file_path = self.get_image_file(&file);
            log::debug!(
                "Reading plane {} in series {} from {}",
                no,
                self.series,
                file_path.display()
            );
            let helper = self
                .helper_reader
                .as_mut()
                .expect("helper reader set above");
            if self.helper_id.as_deref() != Some(file_path.as_path()) {
                helper.set_id(&file_path)?;
                self.helper_id = Some(file_path);
            }
            if helper.metadata().image_count > 1 {
                log::warn!("File {} has {} planes", file, helper.metadata().image_count);
            }
            buf = helper.open_bytes_region(0, x, y, w, h)?;
        }
        Ok(buf)
    }

    fn open_thumb_bytes(&mut self, no: u32) -> Result<Vec<u8>> {
        let (sx, sy) = {
            let m = self.metadata();
            (m.size_x, m.size_y)
        };
        let tw = sx.min(256);
        let th = sy.min(256);
        self.open_bytes_region(no, (sx - tw) / 2, (sy - th) / 2, tw, th)
    }

    fn ome_metadata(&self) -> Option<OmeMetadata> {
        self.store.clone()
    }
}

impl TecanReader {
    /* @see loci.formats.IFormatReader#getUsedFiles(boolean) */
    pub fn used_files(&self, no_pixels: bool) -> Vec<PathBuf> {
        let mut files: Vec<PathBuf> = Vec::new();
        if let Some(id) = &self.current_id {
            files.push(id.clone());
        }
        files.extend(self.extra_files.iter().cloned());
        for img in &self.images {
            if img.result || img.overlay || !no_pixels {
                if let Some(file) = &img.file {
                    files.push(self.get_image_file(file));
                }
            }
        }
        files.sort();
        files
    }

    /* @see loci.formats.IFormatReader#getSeriesUsedFiles(boolean) */
    pub fn series_used_files(&self, no_pixels: bool) -> Vec<PathBuf> {
        let mut files: Vec<PathBuf> = Vec::new();
        if let Some(id) = &self.current_id {
            files.push(id.clone());
        }
        files.extend(self.extra_files.iter().cloned());
        for img in &self.images {
            if img.series == self.series as i32 && (img.result || img.overlay || !no_pixels) {
                if let Some(file) = &img.file {
                    files.push(self.get_image_file(file));
                }
            }
        }
        files.sort();
        files
    }

    /// Java `getGlobalMetadata()` after `flattenHashtables()`.
    pub fn global_metadata(&self) -> HashMap<String, MetadataValue> {
        flatten(&self.metadata)
    }

    /// Java `addGlobalMeta(key, value)`.
    #[cfg_attr(not(feature = "tecan"), allow(dead_code))]
    fn add_global_meta(&mut self, key: &str, value: Option<MetadataValue>) {
        let level = self.metadata_options.level;
        add_meta(key, value, &mut self.metadata, level);
    }

    /// Java `addGlobalMetaList(key, value)`.
    #[cfg_attr(not(feature = "tecan"), allow(dead_code))]
    fn add_global_meta_list(&mut self, key: &str, value: Option<MetadataValue>) {
        let level = self.metadata_options.level;
        add_meta_list(key, value, &mut self.metadata, level);
    }

    /// Java `addSeriesMeta(key, value)`.
    #[cfg_attr(not(feature = "tecan"), allow(dead_code))]
    fn add_series_meta(&mut self, key: &str, value: Option<MetadataValue>) {
        let level = self.metadata_options.level;
        add_meta(key, value, &mut self.series_meta[self.series], level);
    }

    #[cfg_attr(not(feature = "tecan"), allow(dead_code))]
    fn lookup_image(&self, series: i32, plane: i32, overlay: bool, result: bool) -> Option<usize> {
        self.images.iter().position(|img| {
            img.series == series
                && img.plane == plane
                && img.overlay == overlay
                && img.result == result
        })
    }

    #[cfg_attr(not(feature = "tecan"), allow(dead_code))]
    fn lookup_channel_index(&self, name: &str) -> i32 {
        for (i, channel) in self.channels.iter().enumerate() {
            if channel.name == name || channel.original_name.as_deref() == Some(name) {
                return i as i32;
            }
        }
        -1
    }

    /// Turn a relative image file path into an absolute path.
    /// Storing relative paths and one copy of the parent directory
    /// saves some space in the memo file, especially for large plates.
    fn get_image_file(&self, file: &str) -> PathBuf {
        // sanitize the relative path first
        // files might be stored in a subdirectory of "Images",
        // in which case the database may store a "\" to separate the path
        let sanitized = file.replace('\\', std::path::MAIN_SEPARATOR_STR);
        self.image_directory
            .clone()
            .unwrap_or_default()
            .join(sanitized)
    }
}

/// Java `FormatReader.addMeta(key, value, meta)` with the default
/// `filterMetadata == false`.
#[cfg_attr(not(feature = "tecan"), allow(dead_code))]
fn add_meta(
    key: &str,
    value: Option<MetadataValue>,
    meta: &mut HashMap<String, MetaEntry>,
    level: MetadataLevel,
) {
    let Some(value) = value else {
        return;
    };
    if level == MetadataLevel::Minimal {
        return;
    }
    meta.insert(key.trim().to_string(), MetaEntry::Value(value));
}

/// Java `FormatReader.addMetaList(key, value, meta)`.
#[cfg_attr(not(feature = "tecan"), allow(dead_code))]
fn add_meta_list(
    key: &str,
    value: Option<MetadataValue>,
    meta: &mut HashMap<String, MetaEntry>,
    level: MetadataLevel,
) {
    let mut list = match meta.remove(key) {
        Some(MetaEntry::List(list)) => Some(list),
        Some(MetaEntry::Value(v)) => Some(vec![v]),
        None => None,
    };
    add_meta(key, value, meta, level);
    match meta.remove(key) {
        Some(MetaEntry::Value(new_value)) => {
            list.get_or_insert_with(Vec::new).push(new_value);
            meta.insert(key.to_string(), MetaEntry::List(list.unwrap_or_default()));
        }
        Some(MetaEntry::List(_)) | None => {
            if let Some(list) = list {
                meta.insert(key.to_string(), MetaEntry::List(list));
            }
        }
    }
}

/// Java `FormatReader.updateMetadataLists(meta)`: one key per list entry.
fn flatten(meta: &HashMap<String, MetaEntry>) -> HashMap<String, MetadataValue> {
    let mut out = HashMap::new();
    for (key, v) in meta {
        match v {
            MetaEntry::Value(value) => {
                out.insert(key.clone(), value.clone());
            }
            MetaEntry::List(list) => {
                if list.len() == 1 {
                    out.insert(key.clone(), list[0].clone());
                } else {
                    let digits = list.len().to_string().len();
                    for (i, value) in list.iter().enumerate() {
                        out.insert(format!("{key} #{:0digits$}", i + 1), value.clone());
                    }
                }
            }
        }
    }
    out
}

// -- Internal FormatReader API methods --

#[cfg(feature = "tecan")]
mod imp {
    use super::*;
    use crate::common::metadata::DimensionOrder;
    use crate::common::ome_metadata::{create_lsid, OmePlane, OmePlate, OmeWell, OmeWellSample};
    use crate::formats::gpl::leica_lms::{parse_dom, XmlNode};
    use rusqlite::{params, Connection, OpenFlags, OptionalExtension};

    impl TecanReader {
        /* @see loci.formats.FormatReader#initFile(String) */
        pub(super) fn init_file(&mut self, id: &Path) -> Result<()> {
            // super.initFile(id);
            let current_id = std::path::absolute(id).map_err(BioFormatsError::Io)?;
            self.current_id = Some(current_id.clone());

            // find the parent directory for image data
            let parent = current_id
                .parent()
                .and_then(Path::parent)
                .map(Path::to_path_buf)
                .unwrap_or_default();
            let image_dir = parent.join("Images");
            if !image_dir.is_dir() {
                return Err(BioFormatsError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "Cannot find expected 'Images' directory",
                )));
            }
            self.image_directory = Some(image_dir);

            // look for extra files in the "Export" directory
            let export = parent.join("Export");
            if export.is_dir() {
                let mut extra_files = Vec::new();
                find_all_files(&export, &mut extra_files);
                self.extra_files = extra_files;
            }

            let well_labels: HashMap<i32, String>;

            {
                let conn = self.open_connection_current()?;
                let assembled = (|| -> Result<HashMap<i32, String>> {
                    self.find_plate_dimensions(&conn).map_err(sql_err)?;

                    let well_labels = get_well_labels(&conn).map_err(sql_err)?;
                    self.find_images(&conn, &well_labels)?;
                    Ok(well_labels)
                })();
                well_labels = assembled.map_err(|e| {
                    BioFormatsError::Format(format!("Could not assemble plate: {e}"))
                })?;
            }

            // update series and plane index for each Image to account for fields/timepoints
            let channel_count = self.channels.len() as i32;
            for img in &mut self.images {
                img.series *= self.max_field;
                img.series += img.field - 1;
                img.plane += (img.cycle - 1) * channel_count;
            }

            self.core.clear();
            let mut helper = crate::tiff::TiffReader::new();
            for w in 0..well_labels.len() as i32 {
                let first_image = self.lookup_image(w * self.max_field, 0, false, false);
                match first_image.and_then(|i| self.images[i].file.clone()) {
                    Some(file) => {
                        let path = self.get_image_file(&file);
                        helper.set_id(&path)?;
                        self.helper_id = Some(path);
                    }
                    None => {
                        return Err(BioFormatsError::Format("Could not find first file".into()));
                    }
                }
                for _f in 0..self.max_field {
                    // core.add(helperReader.getCoreMetadataList().get(0));
                    // MinimalTiffReader stores its TIFF metadata in the global
                    // table, so the copied CoreMetadata has no series metadata.
                    let mut m = helper.metadata_at(0, 0)?;
                    m.series_metadata.clear();
                    m.size_c *= self.channels.len() as u32;
                    m.size_t = self.max_cycle as u32;
                    m.image_count *= self.channels.len() as u32 * self.max_cycle as u32;
                    self.core.push(m);
                }
            }
            self.helper_reader = Some(helper);
            self.series_meta = vec![HashMap::new(); self.core.len()];

            self.find_extra_metadata()?;

            // populate the MetadataStore

            let mut store = OmeMetadata::default();
            for i in 0..self.core.len() {
                // MetadataTools.populatePixels(store, this, true);
                store.populate_pixels(&self.core[i], i)?;
                let m = &self.core[i];
                store.images[i].planes = (0..m.image_count)
                    .map(|p| {
                        let (z, c, t) = get_zct_coords(m, p);
                        OmePlane {
                            the_z: z,
                            the_c: c,
                            the_t: t,
                            ..Default::default()
                        }
                    })
                    .collect();
            }

            let mut plate = OmePlate {
                id: Some(create_lsid("Plate", &[0])),
                name: self.plate_name.clone(),
                rows: self.plate_rows as u32,
                columns: self.plate_columns as u32,
                wells: Vec::new(),
            };

            // PlateAcquisition (ID, MaximumFieldCount, WellSampleRefs) has no
            // counterpart in the Rust OME model.

            let mut next_well = 0usize;
            let mut next_image = 0usize;
            for row in 0..self.plate_rows {
                for col in 0..self.plate_columns {
                    let mut well = OmeWell {
                        id: Some(create_lsid("Well", &[0, next_well])),
                        row: row as u32,
                        column: col as u32,
                        well_samples: Vec::new(),
                    };

                    let label = make_well_label(row, col);
                    if !well_labels.values().any(|v| *v == label) {
                        plate.wells.push(well);
                        next_well += 1;
                        continue;
                    }

                    for field in 0..self.max_field as usize {
                        let well_sample_id = create_lsid("WellSample", &[0, next_well, field]);
                        well.well_samples.push(OmeWellSample {
                            id: Some(well_sample_id),
                            index: next_image as u32,
                            image_ref: Some(next_image),
                            ..Default::default()
                        });
                        if store.images.len() <= next_image {
                            store.images.resize_with(next_image + 1, Default::default);
                        }
                        store.images[next_image].name =
                            Some(format!("Well {label} Field {}", field + 1));
                        next_image += 1;
                    }

                    plate.wells.push(well);
                    next_well += 1;
                }
            }
            store.plates.push(plate);

            if self.metadata_options.level != MetadataLevel::Minimal {
                for i in 0..self.core.len() {
                    // getEffectiveSizeC()
                    let effective_size_c = {
                        let m = &self.core[i];
                        if m.is_rgb {
                            m.image_count / (m.size_z * m.size_t).max(1)
                        } else {
                            m.size_c
                        }
                    };
                    for c in 0..effective_size_c as usize {
                        store.images[i].channels[c].name = Some(self.channels[c].name.clone());
                    }

                    let first_image =
                        self.lookup_image(i as i32, 0, false, false)
                            .ok_or_else(|| {
                                BioFormatsError::Format(format!(
                                    "Tecan: no first image for series {i}"
                                ))
                            })?;
                    let first_image = &self.images[first_image];
                    if let Some(pixel_size) = first_image.pixel_size {
                        store.images[i].physical_size_x = Some(pixel_size);
                        store.images[i].physical_size_y = Some(pixel_size);
                    }
                    if let Some(timestamp) = &first_image.timestamp {
                        // new Timestamp(DateTools.formatDate(...)); Java throws on a
                        // null formatted date, Rust leaves the date unset instead.
                        store.images[i].acquisition_date = date_tools_format_date(timestamp);
                    }

                    for p in 0..self.core[i].image_count as usize {
                        let img = self
                            .lookup_image(i as i32, p as i32, false, false)
                            .ok_or_else(|| {
                                BioFormatsError::Format(format!(
                                    "Tecan: no image for series {i} plane {p}"
                                ))
                            })?;
                        if let Some(exposure_time) = self.images[img].exposure_time {
                            store.images[i].planes[p].exposure_time = Some(exposure_time);
                        }
                    }
                }
            }
            self.store = Some(store);

            // Rust ImageMetadata has no global table: expose the flattened
            // global metadata alongside each series' own metadata.
            let global = flatten(&self.metadata);
            for (s, m) in self.core.iter_mut().enumerate() {
                m.series_metadata = global.clone();
                m.series_metadata.extend(flatten(&self.series_meta[s]));
            }
            Ok(())
        }

        fn open_connection_current(&self) -> Result<Connection> {
            open_connection(self.current_id.as_deref().unwrap_or(Path::new("")))
        }

        fn find_extra_metadata(&mut self) -> Result<()> {
            let extra = (|| -> std::result::Result<(), String> {
                let conn = self.open_connection_current().map_err(|e| e.to_string())?;

                let serial_number: Option<Option<String>> = conn
                    .query_row(
                        "SELECT InstrumentSerial FROM InstrumentConfig ORDER BY Id",
                        [],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(|e| e.to_string())?;
                if let Some(serial_number) = serial_number {
                    self.add_global_meta("Serial number", serial_number.map(MetadataValue::String));
                }

                let workspace: Option<Option<String>> = conn
                    .query_row("SELECT CompletedAt FROM Workspace ORDER BY Id", [], |r| {
                        r.get(0)
                    })
                    .optional()
                    .map_err(|e| e.to_string())?;
                if let Some(completed) = workspace {
                    self.add_global_meta("Date/Time", completed.map(MetadataValue::String));
                }

                let acquisition: Option<(Option<i64>, Option<i64>, Option<f64>)> = conn
                    .query_row(
                        "SELECT ObjectiveTypeId, IntrawellPatternId, SettleTimeInMs \
                         FROM AcquisitionSetting ORDER BY Id",
                        [],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .optional()
                    .map_err(|e| e.to_string())?;
                if let Some((objective_type_id, intrawell_pattern_id, settle_time)) = acquisition {
                    let objective_type_id = objective_type_id.unwrap_or(0);
                    let intrawell_pattern_id = intrawell_pattern_id.unwrap_or(0);

                    self.add_global_meta(
                        "Settle time [ms]",
                        Some(MetadataValue::Float(settle_time.unwrap_or(0.0))),
                    );

                    let objective: Option<Option<String>> = conn
                        .query_row(
                            "SELECT Name FROM ObjectiveType WHERE Id=?",
                            params![objective_type_id],
                            |r| r.get(0),
                        )
                        .optional()
                        .map_err(|e| e.to_string())?;
                    if let Some(objective) = objective {
                        self.add_global_meta("Objective", objective.map(MetadataValue::String));
                    }

                    let well_pattern_type: Option<Option<String>> = conn
                        .query_row(
                            "SELECT Name FROM IntrawellPattern INNER JOIN IntrawellPatternType \
                             ON IntrawellPattern.IntrawellPatternTypeId=IntrawellPatternType.Id \
                             WHERE IntrawellPattern.Id=?",
                            params![intrawell_pattern_id],
                            |r| r.get(0),
                        )
                        .optional()
                        .map_err(|e| e.to_string())?;
                    if let Some(pattern) = well_pattern_type {
                        self.add_global_meta("Pattern", pattern.map(MetadataValue::String));
                    }
                }

                let application_type: Option<Option<String>> = conn
                    .query_row(
                        "SELECT Name FROM AnalysisSetting \
                         INNER JOIN ApplicationType \
                         ON AnalysisSetting.ApplicationTypeId=ApplicationType.Id",
                        [],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(|e| e.to_string())?;
                if let Some(application) = application_type {
                    self.add_global_meta(
                        "Application type",
                        application.map(MetadataValue::String),
                    );
                }

                {
                    let mut labels = conn
                        .prepare(
                            "SELECT DISTINCT OutputName FROM ImagingResult \
                             INNER JOIN DataLabel \
                             ON ImagingResult.DataLabelId=DataLabel.Id",
                        )
                        .map_err(|e| e.to_string())?;
                    let mut label_names = labels.query([]).map_err(|e| e.to_string())?;
                    while let Some(row) = label_names.next().map_err(|e| e.to_string())? {
                        let name: Option<String> = row.get(0).map_err(|e| e.to_string())?;
                        self.add_global_meta_list("Label Name", name.map(MetadataValue::String));
                    }
                }

                let method: Option<(Option<String>, Option<String>)> = conn
                    .query_row(
                        "SELECT MethodName, SerializedMethod FROM MethodSnapshot ORDER BY Id",
                        [],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()
                    .map_err(|e| e.to_string())?;
                if let Some((method_name, method_xml)) = method {
                    self.add_global_meta("Method name", method_name.map(MetadataValue::String));

                    // methodXML.substring(methodXML.indexOf("<MethodStrip")):
                    // a missing tag throws StringIndexOutOfBoundsException in
                    // Java, which escapes initFile.
                    let method_xml = method_xml.unwrap_or_default();
                    let start = method_xml.find("<MethodStrip").ok_or_else(|| {
                        "StringIndexOutOfBoundsException: <MethodStrip not found".to_string()
                    });
                    let method_xml = match start {
                        Ok(start) => &method_xml[start..],
                        Err(e) => return Err(format!("fatal:{e}")),
                    };
                    match parse_dom(method_xml) {
                        Some(root) => {
                            let channel_settings = get_elements_by_tag_name(
                                &root,
                                "tadodssdf:FluorescenceImagingChannelSetting",
                            );
                            let plate_strips = get_elements_by_tag_name(&root, "PlateStrip");
                            let plate_definitions =
                                get_elements_by_tag_name(&root, "MicroplateDefinition");

                            if let Some(plate_strip) = plate_strips.first() {
                                self.add_global_meta(
                                    "Lid lifter",
                                    Some(MetadataValue::String(
                                        plate_strip.get_attribute("LidType"),
                                    )),
                                );
                                self.add_global_meta(
                                    "Humidity Cassette",
                                    Some(MetadataValue::String(
                                        plate_strip.get_attribute("HumidityCassetteType"),
                                    )),
                                );
                            }

                            if let Some(plate_def) = plate_definitions.first() {
                                self.add_global_meta(
                                    "Plate",
                                    Some(MetadataValue::String(format!(
                                        "{} {}",
                                        plate_def.get_attribute("DisplayName"),
                                        plate_def.get_attribute("Comment")
                                    ))),
                                );
                            }

                            let mut channel_names = String::new();
                            for channel in &channel_settings {
                                let enabled = channel.get_attribute("IsEnabled");
                                let name = channel.get_attribute("Name");

                                // Boolean.parseBoolean(enabled)
                                if enabled.eq_ignore_ascii_case("true") {
                                    if !channel_names.is_empty() {
                                        channel_names.push_str(", ");
                                    }
                                    channel_names.push_str(&name);

                                    let index = self.lookup_channel_index(&name);
                                    if index >= 0 {
                                        let crosstalk_corrections = get_elements_by_tag_name(
                                            channel,
                                            "tadodssdf:CrosstalkSettings.CrosstalkCorrectionDict",
                                        );
                                        if let Some(crosstalk_correction) =
                                            crosstalk_corrections.first()
                                        {
                                            let dict = get_elements_by_tag_name(
                                                crosstalk_correction,
                                                "x:Int16",
                                            );
                                            let mut crosstalk = String::new();

                                            for component in &dict {
                                                if !crosstalk.is_empty() {
                                                    crosstalk.push_str(", ");
                                                }
                                                crosstalk
                                                    .push_str(&component.get_attribute("x:Key"));
                                                crosstalk.push_str(": ");
                                                crosstalk.push_str(&component.text);
                                                crosstalk.push('%');
                                            }

                                            self.add_global_meta(
                                                &format!(
                                                    "Channel #{} Cross-talk settings",
                                                    index + 1
                                                ),
                                                Some(MetadataValue::String(crosstalk)),
                                            );
                                        }
                                    }
                                }
                            }
                            self.add_global_meta(
                                "Channels",
                                Some(MetadataValue::String(channel_names)),
                            );
                        }
                        None => {
                            log::debug!("Could not parse XML");
                        }
                    }
                }
                Ok(())
            })();
            match extra {
                Err(e) if e.starts_with("fatal:") => {
                    return Err(BioFormatsError::Format(e["fatal:".len()..].to_string()));
                }
                Err(e) => log::warn!("Could not read all extra metadata: {e}"),
                Ok(()) => {}
            }

            for c in 0..self.channels.len() {
                let prefix = format!("Channel #{} ", c + 1);
                let channel = self.channels[c].clone();

                self.add_global_meta(
                    &format!("{prefix}Name"),
                    channel.original_name.map(MetadataValue::String),
                );
                self.add_global_meta(
                    &format!("{prefix}LED Intensity [%]"),
                    Some(MetadataValue::Float(channel.intensity)),
                );
                self.add_global_meta(
                    &format!("{prefix}Focus offset [µm]"),
                    Some(MetadataValue::Float(channel.focus_offset)),
                );
                self.add_global_meta(
                    &format!("{prefix}Exposure time [µs]"),
                    Some(MetadataValue::Float(channel.exposure_time)),
                );
            }

            for s in 0..self.core.len() {
                self.series = s;

                for p in 0..self.core[s].image_count as i32 {
                    let img = self
                        .lookup_image(s as i32, p, false, false)
                        .ok_or_else(|| {
                            // Java dereferences a null Image here (NullPointerException).
                            BioFormatsError::Format(format!(
                                "Tecan: no image for series {s} plane {p}"
                            ))
                        })?;
                    let img = self.images[img].clone();

                    let prefix = format!("Plane #{} ", p + 1);

                    self.add_series_meta(
                        &format!("{prefix}Position on Micro Plate"),
                        Some(MetadataValue::String(make_well_label(
                            img.well_row,
                            img.well_column,
                        ))),
                    );
                    self.add_series_meta(
                        &format!("{prefix}File creation time"),
                        img.timestamp.map(MetadataValue::String),
                    );

                    let file = img.file.unwrap_or_default();
                    let file_tokens: Vec<&str> = file.split('_').collect();
                    if file_tokens.len() < 2 {
                        // Java: ArrayIndexOutOfBoundsException escapes initFile.
                        return Err(BioFormatsError::Format(format!(
                            "Tecan: unexpected image file name {file}"
                        )));
                    }
                    self.add_series_meta(
                        &format!("{prefix}Data"),
                        Some(MetadataValue::String(
                            file_tokens[file_tokens.len() - 2].to_string(),
                        )),
                    );
                }
            }
            self.series = 0;

            self.parse_spreadsheet();
            Ok(())
        }

        fn parse_spreadsheet(&mut self) {
            let mut spreadsheet: Option<PathBuf> = None;
            for file in &self.extra_files {
                if check_suffix(file, "xlsx") {
                    spreadsheet = Some(file.clone());
                    break;
                }
            }
            let Some(spreadsheet) = spreadsheet else {
                return;
            };

            let mut shared_strings_xml: Option<String> = None;
            let mut first_sheet_xml: Option<String> = None;

            // ZipInputStream / ZipHandle adapter.
            let read_entries = (|| -> std::result::Result<(), String> {
                let file = std::fs::File::open(&spreadsheet).map_err(|e| e.to_string())?;
                let mut z = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
                for i in 0..z.len() {
                    let mut ze = z.by_index(i).map_err(|e| e.to_string())?;
                    let name = ze.name().to_string();
                    if name == "xl/sharedStrings.xml" || name == "xl/worksheets/sheet1.xml" {
                        let mut b = Vec::new();
                        std::io::Read::read_to_end(&mut ze, &mut b).map_err(|e| e.to_string())?;
                        let text = String::from_utf8_lossy(&b).trim().to_string();
                        if name == "xl/sharedStrings.xml" {
                            shared_strings_xml = Some(text);
                        } else {
                            first_sheet_xml = Some(text);
                        }
                    }
                    if shared_strings_xml.is_some() && first_sheet_xml.is_some() {
                        break;
                    }
                }
                Ok(())
            })();
            if let Err(e) = read_entries {
                log::debug!("Could not parse spreadsheet: {e}");
                return;
            }

            if let (Some(shared_strings_xml), Some(first_sheet_xml)) =
                (shared_strings_xml, first_sheet_xml)
            {
                let (Some(shared_strings), Some(worksheet)) =
                    (parse_dom(&shared_strings_xml), parse_dom(&first_sheet_xml))
                else {
                    log::debug!("Could not parse spreadsheet");
                    return;
                };
                let strings = get_elements_by_tag_name(&shared_strings, "t");
                let cells = get_elements_by_tag_name(&worksheet, "c");

                let mut key: Option<String> = None;
                let mut value = String::new();
                for cell in cells {
                    if let Some(first_child) = cell.children.first() {
                        // Integer.parseInt(...): NumberFormatException aborts the
                        // whole spreadsheet parse.
                        let Ok(cell_value_index) = first_child.text.parse::<i32>() else {
                            log::debug!("Unexpected spreadsheet contents");
                            return;
                        };
                        if cell_value_index < 0 || cell_value_index as usize >= strings.len() {
                            continue;
                        }
                        let cell_value = strings[cell_value_index as usize].text.clone();

                        if SPREADSHEET_KEYS.contains(&cell_value.as_str()) {
                            key = Some(cell_value);
                        } else if let Some(k) = key.take() {
                            self.add_global_meta(
                                &k,
                                Some(MetadataValue::String(format!("{value} {cell_value}"))),
                            );
                            value = String::new();
                        } else {
                            for known_key in SPREADSHEET_KEYS {
                                if cell_value.starts_with(known_key)
                                    || cell_value.starts_with(&format!("{known_key}:"))
                                {
                                    if let Some(index) = cell_value.find(':') {
                                        if index > 0 {
                                            key = Some(cell_value[..index].to_string());
                                            value = cell_value[index + 1..].trim().to_string();
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        pub(super) fn find_plate_dimensions(&mut self, conn: &Connection) -> rusqlite::Result<()> {
            // find the basic plate dimensions
            // expect only one plate to be defined
            let mut statement =
                conn.prepare("SELECT Name, Rows, Columns FROM PlateDefinition ORDER BY Id")?;
            let mut plates = statement.query([])?;
            if let Some(row) = plates.next()? {
                self.plate_name = row.get(0)?;
                self.plate_rows = row.get::<_, Option<i32>>(1)?.unwrap_or(0);
                self.plate_columns = row.get::<_, Option<i32>>(2)?.unwrap_or(0);
            }

            if plates.next()?.is_some() {
                log::warn!("Found more than one plate; only using the first one");
            }
            Ok(())
        }

        fn find_images(&mut self, conn: &Connection, wells: &HashMap<i32, String>) -> Result<()> {
            let mut image_query = conn
                .prepare(
                    "SELECT ImageTypeId, ImagingResultId, RelativePath, PixelSizeInNm \
                     FROM Image ORDER BY Id",
                )
                .map_err(sql_err)?;

            // not clear if CycleIndex is a field or timepoint index
            // treating as a field for now (since that's more difficult),
            // but easy to switch to timepoint if needed
            let mut well_link_query = conn
                .prepare(
                    "SELECT SelectedWellId, CycleIndex FROM ImagingResult \
                     INNER JOIN ResultContext ON \
                     ImagingResult.ResultContextId=ResultContext.Id WHERE \
                     ImagingResult.Id=?",
                )
                .map_err(sql_err)?;
            let mut type_query = conn
                .prepare(
                    "SELECT ChannelTypeId, IsResult, IsOverlay, Name \
                     FROM ImageType WHERE Id = ?",
                )
                .map_err(sql_err)?;
            let mut acquisition_type = conn
                .prepare(
                    "SELECT Name, CreatedAt FROM ImagingResultType \
                     INNER JOIN ImagingResult \
                     ON ImagingResultType.Id=ImagingResult.ImagingResultTypeId \
                     WHERE ImagingResult.Id=?",
                )
                .map_err(sql_err)?;
            let mut channel_query = conn
                .prepare(
                    "SELECT Name, ExposureTimeInUs, FocusOffsetInUm, LedIntensityInPercent FROM ChannelType \
                     INNER JOIN AcquisitionChannelSetting \
                     ON AcquisitionChannelSetting.ChannelTypeId=ChannelType.Id \
                     WHERE ChannelType.Id=?",
                )
                .map_err(sql_err)?;

            let mut all_images = image_query.query([]).map_err(sql_err)?;
            while let Some(row) = all_images.next().map_err(sql_err)? {
                let mut img = Image::new();
                img.file = row.get(2).map_err(sql_err)?;
                // FormatTools.getPhysicalSize(allImages.getDouble(4), "nm")
                img.pixel_size = get_physical_size_nm(
                    row.get::<_, Option<f64>>(3)
                        .map_err(sql_err)?
                        .unwrap_or(0.0),
                );
                log::debug!("processing image file = {:?}", img.file);

                // getString(...) followed by Integer.parseInt(...)
                let image_type_id: i64 = row.get(0).map_err(sql_err)?;
                let result_id: i64 = row.get(1).map_err(sql_err)?;

                let (channel_type_id, result, overlay, channel_name): (
                    Option<i64>,
                    Option<bool>,
                    Option<bool>,
                    Option<String>,
                ) = type_query
                    .query_row(params![image_type_id], |r| {
                        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                    })
                    .map_err(sql_err)?;
                let channel_type_id = channel_type_id.unwrap_or(0);
                img.result = result.unwrap_or(false);
                img.overlay = overlay.unwrap_or(false);
                img.channel_name = channel_name;

                // make sure the image is "Raw" and not "Processed"
                // without this check, the channel and image counts will be wrong
                // as processed files will be included
                let (acq_type_name, acq_timestamp): (Option<String>, Option<String>) =
                    acquisition_type
                        .query_row(params![result_id], |r| Ok((r.get(0)?, r.get(1)?)))
                        .map_err(sql_err)?;

                if acq_type_name.as_deref() == Some("Raw") && !img.result && !img.overlay {
                    // might return 0 rows for overlay/result images
                    let channel: Option<(Option<String>, Option<f64>, Option<f64>, Option<f64>)> =
                        channel_query
                            .query_row(params![channel_type_id], |r| {
                                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                            })
                            .optional()
                            .map_err(sql_err)?;
                    if let Some((name, exposure, focus, intensity)) = channel {
                        // a single ChannelType (e.g. brightfield) can map
                        // to multiple ImageTypes in the same well
                        // (Java string concatenation renders null as "null")
                        let channel_name = format!(
                            "{} {}",
                            img.channel_name.as_deref().unwrap_or("null"),
                            name.as_deref().unwrap_or("null")
                        );
                        img.channel_name = Some(channel_name.clone());

                        if self.lookup_channel_index(&channel_name) < 0 {
                            let mut c = Channel::new(channel_name.clone());
                            c.original_name = name;
                            c.exposure_time = exposure.unwrap_or(0.0);
                            c.focus_offset = focus.unwrap_or(0.0);
                            c.intensity = intensity.unwrap_or(0.0);
                            self.channels.push(c);
                        }
                        img.plane = self.lookup_channel_index(&channel_name);
                        // FormatTools.getTime(channel.getDouble(2), "µs")
                        img.exposure_time = Some(exposure.unwrap_or(0.0) / 1_000_000.0);
                        img.timestamp = acq_timestamp;
                    }
                }

                // now map the image to a well

                let ids = get_well_link(&mut well_link_query, result_id).map_err(sql_err)?;
                let well_label = wells.get(&ids[0]).ok_or_else(|| {
                    BioFormatsError::Format(format!("Tecan: unknown SelectedWell {}", ids[0]))
                })?;
                img.well_row = well_label.as_bytes()[0] as i32 - 'A' as i32;
                img.well_column = parse_well_column(well_label)? - 1;
                img.series = ids[0] - 1;
                // always set a 1-based index
                img.cycle = 1.max(ids[1]);

                self.max_cycle = img.cycle.max(self.max_cycle);
                self.max_field = img.field.max(self.max_field);

                self.images.push(img);
            }
            drop(all_images);

            // list of SVG objects is stored separately
            // each row in ObjectList has a file which needs to be attached to a well
            let mut object_query = conn
                .prepare("SELECT ImagingResultId, Path FROM ObjectList ORDER BY Id")
                .map_err(sql_err)?;
            let mut objects = object_query.query([]).map_err(sql_err)?;
            while let Some(row) = objects.next().map_err(sql_err)? {
                let result_id: i64 = row.get(0).map_err(sql_err)?;
                let path: Option<String> = row.get(1).map_err(sql_err)?;
                log::debug!("processing object {:?}", path);

                let ids = get_well_link(&mut well_link_query, result_id).map_err(sql_err)?;
                let well_label = wells.get(&ids[0]).ok_or_else(|| {
                    BioFormatsError::Format(format!("Tecan: unknown SelectedWell {}", ids[0]))
                })?;
                let mut img = Image::new();
                img.well_row = well_label.as_bytes()[0] as i32 - 'A' as i32;
                img.well_column = parse_well_column(well_label)? - 1;
                img.series = ids[0] - 1;
                // always set a 1-based index
                img.cycle = 1.max(ids[1]);
                img.result = true;
                img.file = path;
                self.images.push(img);
            }
            Ok(())
        }
    }

    pub(super) fn open_connection(file: &Path) -> Result<Connection> {
        // see https://github.com/xerial/sqlite-jdbc/issues/247
        Connection::open_with_flags(
            file,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| {
            log::warn!("Could not read from database");
            BioFormatsError::Io(std::io::Error::other(e))
        })
    }

    fn sql_err(e: rusqlite::Error) -> BioFormatsError {
        BioFormatsError::Format(format!("SQLException: {e}"))
    }

    fn get_well_labels(conn: &Connection) -> rusqlite::Result<HashMap<i32, String>> {
        let mut labels = HashMap::new();

        let mut statement = conn.prepare("SELECT Id, AlphanumericCoordinate FROM SelectedWell")?;
        let mut wells = statement.query([])?;
        while let Some(row) = wells.next()? {
            let id: Option<i32> = row.get(0)?;
            let label: Option<String> = row.get(1)?;
            if let Some(label) = label {
                labels.insert(id.unwrap_or(0), label);
            }
        }
        Ok(labels)
    }

    /// Get the well identifier and field index for the given Id in the
    /// ImagingResult table.
    fn get_well_link(
        link_query: &mut rusqlite::Statement<'_>,
        imaging_result_id: i64,
    ) -> rusqlite::Result<[i32; 2]> {
        link_query.query_row(params![imaging_result_id], |well_link| {
            Ok([
                well_link.get::<_, Option<i32>>(0)?.unwrap_or(0),
                well_link.get::<_, Option<i32>>(1)?.unwrap_or(0),
            ])
        })
    }

    /// `Integer.parseInt(wellLabel.substring(1))`.
    fn parse_well_column(well_label: &str) -> Result<i32> {
        well_label
            .get(1..)
            .and_then(|s| s.parse::<i32>().ok())
            .ok_or_else(|| BioFormatsError::Format(format!("Tecan: bad well label {well_label}")))
    }

    fn find_all_files(root: &Path, files: &mut Vec<PathBuf>) {
        if root.is_dir() {
            // Location.list(true) skips hidden files.
            let mut list: Vec<PathBuf> = std::fs::read_dir(root)
                .into_iter()
                .flatten()
                .flatten()
                .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
                .map(|e| e.path())
                .collect();
            list.sort();
            for path in list {
                find_all_files(&path, files);
            }
        } else {
            files.push(std::path::absolute(root).unwrap_or_else(|_| root.to_path_buf()));
        }
    }

    /// Adapter for `Element.getElementsByTagName(name)`: descendants in
    /// document order (excluding the element itself).
    fn get_elements_by_tag_name<'a>(node: &'a XmlNode, name: &str) -> Vec<&'a XmlNode> {
        let mut out = Vec::new();
        let mut stack: Vec<&XmlNode> = node.children.iter().rev().collect();
        while let Some(n) = stack.pop() {
            if n.name == name {
                out.push(n);
            }
            stack.extend(n.children.iter().rev());
        }
        out
    }

    /// Adapter for `FormatTools.getPhysicalSize(value, "nm")`, in micrometres.
    fn get_physical_size_nm(value: f64) -> Option<f64> {
        if value > 0.0 && value.is_finite() {
            Some(value / 1000.0)
        } else {
            None
        }
    }

    /// Adapter for `DateTools.formatDate(date, "yyyy-MM-dd HH:mm:ss.SSSSSS")`.
    /// Joda parses the fraction as fraction-of-second truncated to
    /// milliseconds; the ISO-8601 output omits zero milliseconds; an
    /// unparseable date yields null.
    pub(super) fn date_tools_format_date(date: &str) -> Option<String> {
        let (day, time) = date.split_once(' ')?;
        let (hms, fraction) = time.split_once('.')?;
        let digits = |s: &str, n: usize| s.len() == n && s.bytes().all(|b| b.is_ascii_digit());
        let d: Vec<&str> = day.split('-').collect();
        let t: Vec<&str> = hms.split(':').collect();
        if d.len() != 3 || !digits(d[0], 4) || !digits(d[1], 2) || !digits(d[2], 2) {
            return None;
        }
        if t.len() != 3 || !t.iter().all(|p| digits(p, 2)) {
            return None;
        }
        if fraction.is_empty()
            || fraction.len() > 9
            || !fraction.bytes().all(|b| b.is_ascii_digit())
        {
            return None;
        }
        let millis: String = format!("{fraction:0<3}").chars().take(3).collect();
        if millis == "000" {
            Some(format!("{day}T{hms}"))
        } else {
            Some(format!("{day}T{hms}.{millis}"))
        }
    }

    /// Adapter for `FormatTools.getZCTCoords(this, no)`.
    fn get_zct_coords(m: &ImageMetadata, no: u32) -> (u32, u32, u32) {
        let size_z = m.size_z.max(1);
        let size_c = if m.is_rgb {
            (m.image_count / (m.size_z * m.size_t).max(1)).max(1)
        } else {
            m.size_c.max(1)
        };
        let size_t = m.size_t.max(1);
        let (a, b) = match m.dimension_order {
            DimensionOrder::XYZCT => (size_z, size_c),
            DimensionOrder::XYZTC => (size_z, size_t),
            DimensionOrder::XYCZT => (size_c, size_z),
            DimensionOrder::XYCTZ => (size_c, size_t),
            DimensionOrder::XYTZC => (size_t, size_z),
            DimensionOrder::XYTCZ => (size_t, size_c),
        };
        let (i0, i1, i2) = (no % a, (no / a) % b, no / (a * b));
        match m.dimension_order {
            DimensionOrder::XYZCT => (i0, i1, i2),
            DimensionOrder::XYZTC => (i0, i2, i1),
            DimensionOrder::XYCZT => (i1, i0, i2),
            DimensionOrder::XYCTZ => (i2, i0, i1),
            DimensionOrder::XYTZC => (i1, i2, i0),
            DimensionOrder::XYTCZ => (i2, i1, i0),
        }
    }
}

#[cfg_attr(not(feature = "tecan"), allow(dead_code))]
fn make_well_label(row: i32, col: i32) -> String {
    format!(
        "{}{}",
        char::from_u32((row + 'A' as i32) as u32).unwrap_or('?'),
        col + 1
    )
}

#[cfg(all(test, feature = "tecan"))]
mod tests {
    use super::imp::date_tools_format_date;
    use super::*;
    use crate::common::metadata::DimensionOrder;
    use crate::common::pixel_type::PixelType;
    use crate::common::writer::FormatWriter;

    /// Synthetic Spark Cyto dataset: wells A1/A2, two kinetic cycles, one raw
    /// green channel plus one processed image per well and cycle. Raw pixels
    /// encode (well, cycle) so plane mapping is checked against pixel data.
    fn make_dataset(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("bioformats_tecan_{}_{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("Database")).unwrap();
        std::fs::create_dir_all(root.join("Images/A1")).unwrap();
        std::fs::create_dir_all(root.join("Images/A2")).unwrap();

        let meta = ImageMetadata {
            size_x: 4,
            size_y: 3,
            pixel_type: PixelType::Uint16,
            bits_per_pixel: 16,
            dimension_order: DimensionOrder::XYCZT,
            is_little_endian: true,
            ..Default::default()
        };
        let write_tiff = |rel: &str, value: u16| {
            let mut writer = crate::tiff::TiffWriter::new();
            writer.set_metadata(&meta).unwrap();
            writer.set_id(&root.join("Images").join(rel)).unwrap();
            let data: Vec<u8> = (0..12).flat_map(|_| value.to_le_bytes()).collect();
            writer.save_bytes(0, &data).unwrap();
            writer.close().unwrap();
        };

        let db = root.join("Database/plate.db");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE PlateDefinition (Id INTEGER, Name TEXT, Rows INTEGER, Columns INTEGER);
             INSERT INTO PlateDefinition VALUES (1, 'Test96', 8, 12);
             CREATE TABLE SelectedWell (Id INTEGER, AlphanumericCoordinate TEXT);
             INSERT INTO SelectedWell VALUES (1, 'A1'), (2, 'A2');
             CREATE TABLE ChannelType (Id INTEGER, Name TEXT);
             INSERT INTO ChannelType VALUES (3, 'Green');
             CREATE TABLE AcquisitionChannelSetting (Id INTEGER, ChannelTypeId INTEGER,
               ExposureTimeInUs REAL, FocusOffsetInUm REAL, LedIntensityInPercent REAL);
             INSERT INTO AcquisitionChannelSetting VALUES (1, 3, 150000, -5, 50);
             CREATE TABLE ImageType (Id INTEGER, ChannelTypeId INTEGER, IsResult INTEGER,
               IsOverlay INTEGER, Name TEXT);
             INSERT INTO ImageType VALUES (4, 3, 0, 0, 'O_G'), (10, 3, 0, 0, 'N_G');
             CREATE TABLE ImagingResultType (Id INTEGER, Name TEXT);
             INSERT INTO ImagingResultType VALUES (1, 'Raw'), (2, 'Processed');
             CREATE TABLE ResultContext (Id INTEGER, SelectedWellId INTEGER, CycleIndex INTEGER);
             CREATE TABLE ImagingResult (Id INTEGER, ResultContextId INTEGER,
               ImagingResultTypeId INTEGER, CreatedAt TEXT, DataLabelId INTEGER);
             CREATE TABLE Image (Id INTEGER, ImageTypeId INTEGER, ImagingResultId INTEGER,
               RelativePath TEXT, PixelSizeInNm INTEGER);
             CREATE TABLE ObjectList (Id INTEGER, ImagingResultId INTEGER, Path TEXT);",
        )
        .unwrap();
        let mut image_id = 0;
        // cycle-major, like a kinetic run
        for cycle in 1..=2 {
            for well in 1..=2 {
                let context = (cycle - 1) * 2 + well;
                conn.execute(
                    "INSERT INTO ResultContext VALUES (?1, ?2, ?3)",
                    rusqlite::params![context, well, cycle],
                )
                .unwrap();
                for (offset, result_type, image_type, kind) in
                    [(0, 1, 4, "O_G_Raw"), (1, 2, 10, "N_G_Processed")]
                {
                    let result_id = context * 2 - 1 + offset;
                    conn.execute(
                        "INSERT INTO ImagingResult VALUES (?1, ?2, ?3, '2025-02-26 12:08:05.160627', 1)",
                        rusqlite::params![result_id, context, result_type],
                    )
                    .unwrap();
                    image_id += 1;
                    let rel = format!("A{well}\\Kinetics_A{well}_{cycle}_{kind}_{image_id}.tiff");
                    conn.execute(
                        "INSERT INTO Image VALUES (?1, ?2, ?3, ?4, 689)",
                        rusqlite::params![image_id, image_type, result_id, rel],
                    )
                    .unwrap();
                    let value = if offset == 0 {
                        (well * 100 + cycle) as u16
                    } else {
                        9999
                    };
                    write_tiff(&rel.replace('\\', "/"), value);
                }
            }
        }
        db
    }

    #[test]
    fn tecan_reads_synthetic_plate_like_java() {
        let db = make_dataset("plate");
        let mut reader = TecanReader::new();
        assert!(reader.is_this_type_by_name(&db));
        reader.set_id(&db).unwrap();

        // one series per well, raw channels only, cycles as timepoints
        assert_eq!(reader.series_count(), 2);
        let m = reader.metadata().clone();
        assert_eq!(
            (m.size_x, m.size_y, m.size_z, m.size_c, m.size_t),
            (4, 3, 1, 1, 2)
        );
        assert_eq!(m.image_count, 2);
        assert_eq!(m.pixel_type, PixelType::Uint16);

        for series in 0..2u16 {
            reader.set_series(series as usize).unwrap();
            for t in 0..2u16 {
                let plane = reader.open_bytes(t as u32).unwrap();
                let expected = (series + 1) * 100 + t + 1;
                assert_eq!(u16::from_le_bytes([plane[0], plane[1]]), expected);
            }
        }

        let ome = reader.ome_metadata().unwrap();
        assert_eq!(ome.images.len(), 2);
        assert_eq!(ome.images[1].name.as_deref(), Some("Well A2 Field 1"));
        assert_eq!(ome.images[0].physical_size_x, Some(0.689));
        assert_eq!(ome.images[0].channels[0].name.as_deref(), Some("O_G Green"));
        assert_eq!(
            ome.images[0].acquisition_date.as_deref(),
            Some("2025-02-26T12:08:05.160")
        );
        assert_eq!(ome.images[0].planes[1].exposure_time, Some(0.15));
        assert_eq!(ome.plates[0].wells.len(), 96);
        let samples: usize = ome.plates[0]
            .wells
            .iter()
            .map(|w| w.well_samples.len())
            .sum();
        assert_eq!(samples, 2);

        let global = reader.global_metadata();
        assert_eq!(
            global
                .get("Channel #1 Name")
                .map(|v| v.to_string())
                .as_deref(),
            Some("Green")
        );
        let series = &reader.metadata().series_metadata;
        assert_eq!(
            series
                .get("Plane #1 Position on Micro Plate")
                .map(|v| v.to_string())
                .as_deref(),
            Some("A2")
        );
        assert_eq!(
            series
                .get("Plane #2 Data")
                .map(|v| v.to_string())
                .as_deref(),
            Some("Raw")
        );
        let _ = std::fs::remove_dir_all(db.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn tecan_registry_routes_db_before_permissive_byte_probes() {
        let db = make_dataset("registry");
        let reader = crate::registry::ImageReader::open(&db).unwrap();
        assert_eq!(reader.series_count(), 2);
        assert_eq!(reader.metadata().size_t, 2);
        let _ = std::fs::remove_dir_all(db.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn tecan_rejects_other_sqlite_and_missing_images_dir() {
        let db = make_dataset("reject");
        let root = db.parent().unwrap().parent().unwrap().to_path_buf();

        let other = root.join("Database/other.db");
        rusqlite::Connection::open(&other)
            .unwrap()
            .execute_batch("CREATE TABLE t (x INTEGER);")
            .unwrap();
        assert!(!TecanReader::new().is_this_type_by_name(&other));
        assert!(!TecanReader::new().is_this_type_by_name(&root.join("Database/plate.sqlite")));

        std::fs::remove_dir_all(root.join("Images")).unwrap();
        let mut reader = TecanReader::new();
        let err = reader.set_id(&db).unwrap_err();
        assert!(err.to_string().contains("Images"), "{err}");
        assert_eq!(reader.series_count(), 0);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn tecan_date_tools_format_date_matches_java() {
        // Values observed from loci.common.DateTools with the bundled jar.
        assert_eq!(
            date_tools_format_date("2025-02-26 12:08:05.160627").as_deref(),
            Some("2025-02-26T12:08:05.160")
        );
        assert_eq!(
            date_tools_format_date("2025-02-26 12:08:05.000000").as_deref(),
            Some("2025-02-26T12:08:05")
        );
        assert_eq!(
            date_tools_format_date("2025-02-26 12:08:05.1").as_deref(),
            Some("2025-02-26T12:08:05.100")
        );
        assert_eq!(date_tools_format_date("2025-02-26 12:08:05"), None);
    }
}
