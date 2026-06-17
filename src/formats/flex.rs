//! PerkinElmer/Evotec FLEX HCS format reader.
//!
//! FLEX is a TIFF-based format used for high-content screening (HCS) by
//! PerkinElmer/Evotec. Each FLEX image plane stores an XML metadata block in a
//! custom TIFF tag (65200). That XML contains one `<Array>` element per image
//! plane carrying `Name` and `Factor` attributes. `Factor` is a per-plane
//! scaling multiplier that must be applied to pixel values on read; when any
//! factor is greater than 1 the effective pixel type widens (UINT16, or UINT32
//! when the largest factor exceeds 256). This mirrors `FlexReader.java`.
//!
//! A FLEX dataset is normally a *directory* of `.flex` files, one per
//! (well, field), optionally accompanied by `.mea`/`.res` measurement
//! companions that enumerate the wells/fields. Following `FlexReader.java`:
//!   - `.flex` filenames of the form `nnnnnnnnn.flex` (14 chars) encode the
//!     well row (chars 0..3), well column (chars 3..6) and field (chars 6..9),
//!     all 1-based.
//!   - Files are grouped by well; each file within a well is a field.
//!   - One OME series is produced per (plate, well, field).
//!   - The `.mea` file lists `<Picture path=...>` entries pointing at the
//!     `.flex` files; the `.res` file carries the plate acquisition date.
//!
//! When no companion files are present and the single `.flex` cannot be grouped
//! (filename is not the 14-char well pattern), the reader falls back to the
//! original single-file behavior: the TIFF IFDs map directly to planes of a
//! single series.
//!
//! This reader wraps `TiffReader` for the raw TIFF pixel I/O and layers the
//! Flex XML factor parsing + pixel scaling + well/field series assembly on top.

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::ImageMetadata;
use crate::common::ome_metadata::{create_lsid, OmeMetadata, OmePlate, OmeWell, OmeWellSample};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::tiff::ifd::IfdValue;
use std::path::{Path, PathBuf};

/// Custom Flex IFD entry holding the per-image XML (FlexReader.FLEX = 65200).
const FLEX_TAG: u16 = 65200;

/// One `.flex` file in a grouped dataset: a (well row, well column, field).
struct FlexFile {
    row: u32,
    column: u32,
    field: u32,
    path: PathBuf,
    /// Per-plane scaling factors for this file (`None` when every factor is 1).
    factors: Option<Vec<f64>>,
}

pub struct FlexReader {
    /// TiffReader bound to the *currently selected* series' `.flex` file.
    inner: crate::tiff::TiffReader,
    /// Path currently loaded into `inner` (so we avoid re-opening on set_series).
    inner_path: Option<PathBuf>,
    /// Grouped files (multi-file HCS mode). Empty in single-file fallback.
    flex_files: Vec<FlexFile>,
    /// Current series index.
    series: usize,
    /// Effective pixel type after factor widening (applies to series 0 like Java).
    scaled_pixel_type: Option<PixelType>,
    /// Per-series metadata (cloned from the representative file's TIFF series 0).
    series_meta: Vec<ImageMetadata>,
    /// planes-per-image-count for the dataset (Z*C*T of series 0).
    image_count: u32,
    /// HCS layout.
    plate_count: u32,
    well_count: u32,
    field_count: u32,
    well_rows: u32,
    well_columns: u32,
    /// (row, col) for each well index (parallel to the column-major well list).
    well_number: Vec<(u32, u32)>,
    /// Companion measurement files found (.mea/.res), absolute paths.
    measurement_files: Vec<PathBuf>,
    /// Plate acquisition start time from the .res file.
    plate_acq_start_time: Option<String>,
    /// Plate name / barcode parsed from XML.
    plate_name: Option<String>,
    plate_barcode: Option<String>,
    /// True when running in single-file fallback mode.
    single_file: bool,
    /// When true (single-file mode with multiple fields stored inside the one
    /// `.flex` file), the file's IFDs are split across `field_count` series.
    /// FLEX (series s, plane p) maps to inner IFD `s * image_count + p`.
    fields_in_file: bool,
    /// Channel names parsed from the FLEX `<Array Name=...>` elements (one per
    /// IFD/plane in document order). Used for OME channel naming.
    channel_names: Vec<String>,
    /// Physical pixel size (x, y) in microns, from `<ImageResolutionX/Y>` * 1e6.
    physical_size: Option<(f64, f64)>,

    // ── Instrument / plane data members (mirror of FlexReader.java fields) ──
    /// Camera binning values from the most recent `<Image>` (Java `binX`/`binY`).
    bin_x: u32,
    bin_y: u32,
    /// effectiveFieldCount cached by `populate_metadata_store` (Java
    /// `effectiveFieldCount`, set in `lookupFile(int)`); 1 when
    /// wellCount*plateCount == file count, else fieldCount.
    effective_field_count: u32,
    /// Per-`<Image>` `binX x binY` strings (Java `binnings`).
    binnings: Vec<String>,
    /// Camera/object/light-source `ID` attribute pools (Java `cameraIDs`,
    /// `objectiveIDs`, `lightSourceIDs`) used to resolve refs by index.
    camera_ids: Vec<String>,
    objective_ids: Vec<String>,
    light_source_ids: Vec<String>,
    /// Per-`<Image>` resolved Detector/Objective LSIDs (Java `cameraRefs`,
    /// `objectiveRefs`).
    camera_refs: Vec<String>,
    objective_refs: Vec<String>,
    /// Per-`<Image>` `<ImageResolutionX/Y>` in microns (Java `xSizes`/`ySizes`).
    x_sizes: Vec<f64>,
    y_sizes: Vec<f64>,
    /// Plate-level `<OffsetX/Y>` well-sample positions in microns (Java
    /// `xPositions`/`yPositions`).
    x_positions: Vec<f64>,
    y_positions: Vec<f64>,
    /// Per-`<Image>` light-source / filter combination refs (Java
    /// `lightSourceCombinationRefs`, `filterSets`).
    light_source_combination_refs: Vec<String>,
    filter_sets: Vec<String>,
    /// LightSourceCombination ID → member LightSource LSIDs (Java
    /// `lightSourceCombinationIDs`).
    light_source_combination_ids: std::collections::HashMap<String, Vec<String>>,
    /// Filter/dichroic raw-ID → LSID maps (Java `filterMap`/`dichroicMap`).
    filter_map: std::collections::HashMap<String, String>,
    dichroic_map: std::collections::HashMap<String, String>,
    /// FilterSet ID → (emission, excitation, dichroic) LSIDs (Java
    /// `filterSetMap` of `FilterGroup`).
    filter_set_map: std::collections::HashMap<String, FilterGroup>,
    /// Per-plane positions / times (Java `planePositionX/Y/Z`,
    /// `planeDeltaT`, `planeExposureTime`), accumulated in document order.
    plane_position_x: Vec<f64>,
    plane_position_y: Vec<f64>,
    plane_position_z: Vec<f64>,
    plane_delta_t: Vec<f64>,
    plane_exposure_time: Vec<f64>,
    /// Acquisition `<DateTime>` per series (Java `acquisitionDates`).
    acquisition_dates: std::collections::HashMap<usize, String>,
}

/// Port of Java `FlexReader.FilterGroup`: the emission/excitation/dichroic LSIDs
/// referenced by a single `<FilterCombination>`.
#[derive(Default, Clone)]
struct FilterGroup {
    emission: Option<String>,
    excitation: Option<String>,
    dichroic: Option<String>,
}

impl FlexReader {
    pub fn new() -> Self {
        FlexReader {
            inner: crate::tiff::TiffReader::new(),
            inner_path: None,
            flex_files: Vec::new(),
            series: 0,
            scaled_pixel_type: None,
            series_meta: Vec::new(),
            image_count: 0,
            plate_count: 0,
            well_count: 0,
            field_count: 0,
            well_rows: 0,
            well_columns: 0,
            well_number: Vec::new(),
            measurement_files: Vec::new(),
            plate_acq_start_time: None,
            plate_name: None,
            plate_barcode: None,
            single_file: true,
            fields_in_file: false,
            channel_names: Vec::new(),
            physical_size: None,
            bin_x: 0,
            bin_y: 0,
            effective_field_count: 0,
            binnings: Vec::new(),
            camera_ids: Vec::new(),
            objective_ids: Vec::new(),
            light_source_ids: Vec::new(),
            camera_refs: Vec::new(),
            objective_refs: Vec::new(),
            x_sizes: Vec::new(),
            y_sizes: Vec::new(),
            x_positions: Vec::new(),
            y_positions: Vec::new(),
            light_source_combination_refs: Vec::new(),
            filter_sets: Vec::new(),
            light_source_combination_ids: std::collections::HashMap::new(),
            filter_map: std::collections::HashMap::new(),
            dichroic_map: std::collections::HashMap::new(),
            filter_set_map: std::collections::HashMap::new(),
            plane_position_x: Vec::new(),
            plane_position_y: Vec::new(),
            plane_position_z: Vec::new(),
            plane_delta_t: Vec::new(),
            plane_exposure_time: Vec::new(),
            acquisition_dates: std::collections::HashMap::new(),
        }
    }

    /// Extract the Flex XML block (tag 65200) from the first IFD as a string.
    fn flex_xml(&self) -> Option<String> {
        let ifd = self.inner.ifd(0)?;
        match ifd.get(FLEX_TAG) {
            Some(IfdValue::Ascii(s)) => Some(s.clone()),
            Some(IfdValue::Byte(b)) | Some(IfdValue::Undefined(b)) => {
                Some(String::from_utf8_lossy(b).into_owned())
            }
            _ => None,
        }
    }

    /// Ensure `inner` is bound to the `.flex` file for series `s`.
    fn bind_series(&mut self, s: usize) -> Result<()> {
        if self.single_file {
            self.series = s;
            return Ok(());
        }
        let file = self
            .flex_files
            .get(self.file_index_for_series(s))
            .ok_or(BioFormatsError::SeriesOutOfRange(s))?;
        let path = file.path.clone();
        if self.inner_path.as_deref() != Some(path.as_path()) {
            self.inner.set_id(&path)?;
            self.inner_path = Some(path);
        }
        self.series = s;
        Ok(())
    }

    /// Map an OME series index to an index into `flex_files`.
    ///
    /// Mirrors Java `lookupFile(int fileSeries)`: lengths =
    /// {fieldCount, wellCount, plateCount}, raster-to-position, then look up by
    /// (row, col, field).
    fn file_index_for_series(&self, series: usize) -> usize {
        if self.fields_in_file || self.flex_files.len() == 1 {
            return 0;
        }
        let field_count = self.field_count.max(1);
        let well_count = self.well_count.max(1);
        let plate_count = self.plate_count.max(1);

        // effectiveFieldCount: cached by populate_metadata_store (Java
        // `lookupFile(int)` writes the `effectiveFieldCount` member); 1 when
        // wellCount*plateCount == file count. Recompute as a fallback if unset.
        let effective_field_count = if self.effective_field_count > 0 {
            self.effective_field_count
        } else {
            self.effective_field_count(field_count, well_count, plate_count)
        };

        let lengths = [
            field_count as usize,
            well_count as usize,
            plate_count as usize,
        ];
        let pos = raster_to_position(&lengths, series);

        let zero_well = well_count == 1 && effective_field_count == 1;
        let (row, col) = if zero_well {
            (0, 0)
        } else {
            self.well_number.get(pos[1]).copied().unwrap_or((0, 0))
        };
        let field = if effective_field_count == 1 {
            0
        } else {
            pos[0] as u32
        };

        self.flex_files
            .iter()
            .position(|f| f.row == row && f.column == col && f.field == field)
            .unwrap_or(0)
    }

    /// Mirror of Java `lookupFile(int)`'s effectiveFieldCount computation: 1 when
    /// `wellCount * plateCount == flexFiles.size()`, else `fieldCount`.
    fn effective_field_count(&self, field_count: u32, well_count: u32, plate_count: u32) -> u32 {
        if (well_count * plate_count) as usize == self.flex_files.len() {
            1
        } else {
            field_count
        }
    }

    /// Parse `<Array Name=.. Factor=..>` arrays and derive factors / widening.
    /// Returns the factor vector (`None` when all factors are 1). The scaled
    /// pixel type is only widened from the first file's factors: Java derives
    /// `core.get(0).pixelType` from file-0's max factor (FlexReader.java:909),
    /// so a non-first file must not widen it. Pass `update_pixel_type = false`
    /// for files other than the first.
    fn derive_factors(
        &mut self,
        total_planes: usize,
        update_pixel_type: bool,
    ) -> Result<Option<Vec<f64>>> {
        if total_planes == 0 {
            return Err(BioFormatsError::Format(
                "Flex: TIFF file has no image planes".into(),
            ));
        }
        let Some(mut xml) = self.flex_xml() else {
            return Ok(None);
        };
        let trimmed = xml.trim();
        if trimmed.ends_with(">>") || trimmed.ends_with('%') {
            xml = trimmed[..trimmed.len() - 1].to_string();
        } else {
            xml = trimmed.to_string();
        }
        let (_names, factors) = parse_flex_arrays(&xml);

        if !factors.is_empty() && factors.len() != total_planes {
            return Err(BioFormatsError::Format(format!(
                "Flex: XML Array count {} does not match TIFF plane count {}",
                factors.len(),
                total_planes
            )));
        }

        let mut factor_values = vec![1.0f64; total_planes];
        let mut max_idx = 0usize;
        let mut one_factors = true;
        for (i, f) in factors.iter().enumerate() {
            let q = f.parse::<f64>().map_err(|_| {
                BioFormatsError::Format(format!("Flex: invalid Array Factor {f:?}"))
            })?;
            if !q.is_finite() || q <= 0.0 {
                return Err(BioFormatsError::Format(format!(
                    "Flex: invalid Array Factor {f:?}"
                )));
            }
            if i < factor_values.len() {
                factor_values[i] = q;
                if q > factor_values[max_idx] {
                    max_idx = i;
                }
                if q != 1.0 {
                    one_factors = false;
                }
            }
        }

        if update_pixel_type {
            let max_factor = factor_values.get(max_idx).copied().unwrap_or(1.0);
            if max_factor > 256.0 {
                self.scaled_pixel_type = Some(PixelType::Uint32);
            } else if max_factor > 1.0 {
                self.scaled_pixel_type = Some(PixelType::Uint16);
            }
        }

        if one_factors {
            Ok(None)
        } else {
            Ok(Some(factor_values))
        }
    }

    /// Apply the Flex pixel scaling factor to a freshly read plane, widening to
    /// `scaled_pixel_type` if needed. Mirrors `FlexReader.openBytes` scaling.
    fn apply_factor(&self, raw: Vec<u8>, plane: u32, little_endian: bool) -> Vec<u8> {
        let src_pt = self
            .inner
            .series_list()
            .get(self.inner.series())
            .map(|s| s.metadata.pixel_type)
            .unwrap_or(PixelType::Uint8);
        let n_bytes = src_pt.bytes_per_sample();
        let dst_pt = self.scaled_pixel_type.unwrap_or(src_pt);
        let bpp = dst_pt.bytes_per_sample();

        let factor = if self.single_file {
            // factors stored on inner-file order via derive on series 0.
            self.series_factor(0, plane)
        } else {
            let fi = self.file_index_for_series(self.series);
            self.series_factor(fi, plane)
        };

        if factor == 1.0 && n_bytes == bpp {
            return raw;
        }
        if n_bytes == 0 || bpp == 0 {
            return raw;
        }

        let num = raw.len() / n_bytes;
        let mut out = vec![0u8; num * bpp];
        for i in 0..num {
            let q = read_uint(&raw, i * n_bytes, n_bytes, little_endian);
            let scaled = (q as f64 * factor) as u64;
            write_uint(&mut out, i * bpp, bpp, scaled, little_endian);
        }
        out
    }

    /// Map a FLEX (current series, plane) to the inner TIFF plane index.
    ///
    /// When fields are stored within a single file (`fields_in_file`), the IFDs
    /// of the one file are split across series: inner IFD = series*imageCount + p
    /// (Java `imageNumber = getImageCount() * pos[0] + no`, pos[0] == series).
    /// Otherwise each series binds its own file and the plane maps directly.
    fn inner_plane(&self, plane: u32) -> u32 {
        if self.fields_in_file {
            self.series as u32 * self.image_count + plane
        } else {
            plane
        }
    }

    fn series_factor(&self, file_index: usize, plane: u32) -> f64 {
        self.flex_files
            .get(file_index)
            .and_then(|f| f.factors.as_ref())
            .and_then(|v| v.get(plane as usize).copied())
            .unwrap_or(1.0)
    }

    /// Port of `FlexReader.initFile`: dispatch on the file suffix to the
    /// matching initializer, mirroring Java's delegation.
    fn init_file(&mut self, id: &Path) -> Result<()> {
        let ext = id
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        if matches!(ext.as_deref(), Some("flex")) {
            self.init_flex_file(id)
        } else if matches!(ext.as_deref(), Some("res")) {
            self.init_res_file(id)
        } else {
            self.init_mea_file(id)
        }
    }

    /// Port of `FlexReader.initResFile`: locate the `.flex` entry point next to
    /// a `.res`/`.mea` companion, then proceed as for a `.flex` file.
    fn init_res_file(&mut self, id: &Path) -> Result<()> {
        let flex_entry = self.resolve_flex_entry(id)?;
        self.init_flex_file(&flex_entry)
    }

    /// Port of `FlexReader.initMeaFile`: as for `.res`, resolve the `.flex`
    /// entry point next to the measurement file then proceed as for `.flex`.
    fn init_mea_file(&mut self, id: &Path) -> Result<()> {
        let flex_entry = self.resolve_flex_entry(id)?;
        self.init_flex_file(&flex_entry)
    }

    /// Locate a `.flex` file in the same directory as a `.mea`/`.res` companion.
    fn resolve_flex_entry(&self, id: &Path) -> Result<PathBuf> {
        let dir = id.parent().unwrap_or_else(|| Path::new("."));
        let mut found = None;
        if let Ok(rd) = std::fs::read_dir(dir) {
            let mut candidates: Vec<PathBuf> = rd
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.extension()
                        .and_then(|e| e.to_str())
                        .map(|e| e.eq_ignore_ascii_case("flex"))
                        .unwrap_or(false)
                })
                .collect();
            candidates.sort();
            found = candidates.into_iter().next();
        }
        found.ok_or_else(|| {
            BioFormatsError::UnsupportedFormat(
                "Flex .mea/.res companion has no .flex files in its directory".into(),
            )
        })
    }

    /// Port of `FlexReader.initFlexFile`: find companion measurement files,
    /// build the grouped `.flex` list, then group and populate the store.
    fn init_flex_file(&mut self, id: &Path) -> Result<()> {
        let flex_entry = id.to_path_buf();

        // findFiles + parseResFile: locate companion .mea/.res files and read
        // the plate acquisition start time from any .res file.
        let measurement_files = self.find_files(&flex_entry);
        for m in &measurement_files {
            if m.extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("res"))
                .unwrap_or(false)
            {
                self.parse_res_file(m);
            }
        }

        // Determine the grouped file list. Prefer the .mea list when present.
        let mut grouped: Vec<PathBuf> = Vec::new();
        for m in &measurement_files {
            if m.extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("mea"))
                .unwrap_or(false)
            {
                if let Ok(text) = std::fs::read_to_string(m) {
                    let dir = flex_entry.parent().unwrap_or_else(|| Path::new("."));
                    for rel in parse_mea_flex_names(&text) {
                        // Match by file name within the .flex directory.
                        let fname = rel.rsplit('/').next().unwrap_or(&rel);
                        let candidate = dir.join(fname);
                        if candidate.exists() {
                            grouped.push(candidate);
                        }
                    }
                }
            }
        }
        if grouped.is_empty() {
            grouped = collect_flex_files(&flex_entry);
        } else {
            grouped.sort();
            grouped.dedup();
        }
        self.measurement_files = measurement_files;

        // Single-file fallback: cannot group by well pattern.
        let entry_name = flex_entry
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        let groupable = grouped.len() > 1 || parse_well(entry_name).is_some();

        if !groupable {
            return self.init_single_file(flex_entry);
        }

        self.single_file = false;
        let store = self.group_files(grouped)?;
        self.populate_metadata_store(store)
    }

    /// Single-file fallback (`doGrouping == false` path of `initFlexFile`):
    /// the one `.flex` file's TIFF IFDs map directly to a single series.
    fn init_single_file(&mut self, flex_entry: PathBuf) -> Result<()> {
        self.single_file = true;
        self.inner.set_id(&flex_entry)?;
        self.inner_path = Some(flex_entry.clone());

        let total_planes: usize = (0..self.inner.series_count())
            .map(|s| self.inner.series_list()[s].metadata.image_count as usize)
            .sum();

        // FlexHandler for the lone file: capture channel names + instrument /
        // plane data members (Java runs the handler in the single-file path too,
        // via parseFlexFile/initFlexFile).
        if let Some(mut xml) = self.flex_xml() {
            let t = xml.trim();
            xml = if t.ends_with(">>") || t.ends_with('%') {
                t[..t.len() - 1].to_string()
            } else {
                t.to_string()
            };
            let (names, _f) = parse_flex_arrays(&xml);
            self.channel_names = names;
            self.physical_size = parse_physical_size(&xml);
            self.run_flex_handler(
                &xml, /*well=*/ 0, /*this_field=*/ -1, /*populate_core=*/ true,
            );
        }
        self.image_count = self
            .inner
            .series_list()
            .first()
            .map(|s| s.metadata.image_count)
            .unwrap_or(0);

        let factors = self.derive_factors(total_planes, true)?;

        if let Some(pt) = self.scaled_pixel_type {
            let series = self.inner.series_list_mut();
            if let Some(s0) = series.first_mut() {
                s0.metadata.pixel_type = pt;
                s0.metadata.bits_per_pixel = (pt.bytes_per_sample() * 8) as u8;
            }
        }
        // store the single file's factors as flex_files[0] for apply_factor.
        self.flex_files = vec![FlexFile {
            row: 0,
            column: 0,
            field: 0,
            path: flex_entry,
            factors,
        }];
        self.series = 0;
        Ok(())
    }

    /// Port of `FlexReader.findFiles`: collect the companion `.mea`/`.res`
    /// measurement files that belong to the same dataset as `flex_entry`.
    fn find_files(&self, flex_entry: &Path) -> Vec<PathBuf> {
        find_measurement_files(flex_entry)
    }

    /// Port of `FlexReader.parseResFile`: read the plate acquisition start time
    /// from a `.res` file.
    fn parse_res_file(&mut self, res_path: &Path) {
        if let Ok(text) = std::fs::read_to_string(res_path) {
            if let Some(d) = parse_res_date(&text) {
                self.plate_acq_start_time = Some(d);
            }
        }
    }

    /// Port of `FlexReader.groupFiles`: group the `.flex` files by well, build
    /// the `flex_files`/`well_number` layout, and parse each file (the first
    /// file drives core metadata via `parse_flex_file`). Returns the base
    /// per-series metadata to be replicated by `populate_metadata_store`.
    fn group_files(&mut self, grouped: Vec<PathBuf>) -> Result<ImageMetadata> {
        // Group files by well (row, col), each file within a well is a field.
        // Build well list in (row, col) order; record well_number layout.
        use std::collections::BTreeMap;
        let mut wells: BTreeMap<(u32, u32), Vec<PathBuf>> = BTreeMap::new();
        let mut max_row = 0u32;
        let mut max_col = 0u32;
        for f in &grouped {
            let n = f.file_name().and_then(|x| x.to_str()).unwrap_or_default();
            let (row, col) = if grouped.len() == 1 {
                (0, 0)
            } else {
                parse_well(n).unwrap_or((0, 0))
            };
            max_row = max_row.max(row);
            max_col = max_col.max(col);
            wells.entry((row, col)).or_default().push(f.clone());
        }
        self.well_rows = max_row + 1;
        self.well_columns = max_col + 1;
        if grouped.len() == 1 {
            self.well_rows = 1;
            self.well_columns = 1;
        }
        self.well_count = wells.len() as u32;

        // Build the flex_files list in well order, fields sorted within a well.
        let mut flex_files: Vec<FlexFile> = Vec::new();
        let mut well_number: Vec<(u32, u32)> = Vec::new();
        let mut expected_files_per_well: Option<usize> = None;
        for (&(row, col), files) in &wells {
            well_number.push((row, col));
            let mut sorted = files.clone();
            sorted.sort();
            if let Some(expected) = expected_files_per_well {
                if sorted.len() != expected {
                    return Err(BioFormatsError::Format(format!(
                        "Flex: inconsistent field count for well ({row},{col}): got {}, expected {expected}",
                        sorted.len()
                    )));
                }
            } else {
                expected_files_per_well = Some(sorted.len());
            }
            // Java assigns the field index by sorted position within the well
            // (FlexFile.field = field loop variable), but the filename's field
            // digits (chars 6..9) are the authoritative field number. Use the
            // filename field when the 14-char pattern is present, falling back
            // to sorted position otherwise.
            for (pos, p) in sorted.into_iter().enumerate() {
                let n = p.file_name().and_then(|x| x.to_str()).unwrap_or_default();
                let field = if n.len() == 14 {
                    parse_field(n)
                } else {
                    pos as u32
                };
                flex_files.push(FlexFile {
                    row,
                    column: col,
                    field,
                    path: p,
                    factors: None,
                });
            }
        }
        self.well_number = well_number;

        // Parse the first file to obtain core dimensions + factors.
        let first_path = flex_files[0].path.clone();
        self.inner.set_id(&first_path)?;
        self.inner_path = Some(first_path);
        let n_planes = self
            .inner
            .series_list()
            .first()
            .map(|s| s.metadata.image_count)
            .unwrap_or(0);
        if n_planes == 0 {
            return Err(BioFormatsError::Format(
                "Flex: first grouped file has no image planes".into(),
            ));
        }

        // parseFlexFile (first file): XML factors + populateCoreMetadata, then
        // parse factors for the remaining grouped files.
        let base_meta = self.parse_flex_file(&mut flex_files, n_planes)?;

        // Plate name/barcode from the first file's XML.
        if let Some(xml) = {
            // rebind to first file to read its XML
            let p = flex_files[0].path.clone();
            if self.inner_path.as_deref() != Some(p.as_path()) {
                self.inner.set_id(&p)?;
                self.inner_path = Some(p);
            }
            self.flex_xml()
        } {
            self.plate_barcode = xml_element_text(&xml, "Barcode");
            self.plate_name = xml_element_text(&xml, "PlateName");
        }

        self.flex_files = flex_files;
        Ok(base_meta)
    }

    /// Port of `FlexReader.parseFlexFile`: parse the first file's FLEX XML for
    /// channel names / physical size / factors, populate core metadata (via
    /// `populate_core_metadata`), then parse the remaining files' factors.
    /// Returns the base per-series metadata.
    fn parse_flex_file(
        &mut self,
        flex_files: &mut [FlexFile],
        n_planes: u32,
    ) -> Result<ImageMetadata> {
        // Read the first file's FLEX XML for channel names, in-file field
        // count and physical size (mirrors FlexHandler / populateCoreMetadata).
        let first_xml = self.flex_xml().map(|mut xml| {
            let t = xml.trim();
            xml = if t.ends_with(">>") || t.ends_with('%') {
                t[..t.len() - 1].to_string()
            } else {
                t.to_string()
            };
            xml
        });
        let (image_names, _xml_factors) = first_xml
            .as_deref()
            .map(parse_flex_arrays)
            .unwrap_or_default();
        self.channel_names = image_names.clone();
        self.physical_size = first_xml.as_deref().and_then(parse_physical_size);

        let n_files = flex_files.len();

        // FlexHandler (first file): populate the instrument / plane data members
        // (binnings, camera/objective refs, positions, plane times, acquisition
        // dates, filter maps). thisField mirrors Java's groupFiles call:
        // -1 when the whole well is one file, else the file's field index.
        if let Some(ref xml) = first_xml {
            let this_field: i64 = if n_files == 1 {
                -1
            } else {
                flex_files[0].field as i64
            };
            self.run_flex_handler(
                xml, /*well=*/ 0, this_field, /*populate_core=*/ true,
            );
        }

        // For a single file, the field count comes from the in-file
        // `<Field No>` elements (Java passes thisField = -1). For a true
        // multi-file dataset it starts at 0 and is later multiplied by nFiles.
        let initial_field_count = if n_files == 1 {
            first_xml
                .as_deref()
                .map(|x| count_fields(x, n_planes as usize))
                .unwrap_or(0)
        } else {
            0
        };

        // Derive factors & widening from series-0 XML. Only the first file
        // sets the scaled pixel type (Java FlexReader.java:909).
        let factors = self.derive_factors(n_planes as usize, true)?;
        flex_files[0].factors = factors;

        // populateCoreMetadata: compute sizeC/sizeZ/sizeT/imageCount/fieldCount.
        let base_meta =
            self.populate_core_metadata(&image_names, initial_field_count, n_planes, n_files)?;

        // Pre-compute the (well index) for each file in document order, so the
        // FlexHandler can index acquisitionDates by series (Java's currentWell).
        let well_indices = file_well_indices(flex_files);

        // Parse factors for the remaining files (each file may have its own XML).
        for i in 1..flex_files.len() {
            let p = flex_files[i].path.clone();
            self.inner.set_id(&p)?;
            self.inner_path = Some(p.clone());
            let np = self
                .inner
                .series_list()
                .first()
                .map(|s| s.metadata.image_count)
                .unwrap_or(n_planes);
            if np != n_planes {
                return Err(BioFormatsError::Format(format!(
                    "Flex: grouped file {} has {} planes, expected {}",
                    p.display(),
                    np,
                    n_planes
                )));
            }
            // Run the FlexHandler for this file too (populate_core=false), so the
            // per-Image instrument/plane lists accumulate across the dataset in
            // document order, mirroring Java's per-file parseFlexFile calls.
            if let Some(mut xml) = self.flex_xml() {
                let t = xml.trim();
                xml = if t.ends_with(">>") || t.ends_with('%') {
                    t[..t.len() - 1].to_string()
                } else {
                    t.to_string()
                };
                let this_field = flex_files[i].field as i64;
                let well = well_indices[i];
                self.run_flex_handler(&xml, well, this_field, /*populate_core=*/ false);
            }
            // Non-first files contribute their own per-plane factors but must
            // not widen the scaled pixel type (derived from file 0 only).
            let f = self.derive_factors(np as usize, false)?;
            flex_files[i].factors = f;
        }

        Ok(base_meta)
    }

    /// Port of `FlexReader.populateCoreMetadata`: compute the series dimensions
    /// from the image names / IFD count, set HCS counts, and build the base
    /// per-series metadata from the inner TIFF (overriding the dimension split).
    fn populate_core_metadata(
        &mut self,
        image_names: &[String],
        initial_field_count: u32,
        n_planes: u32,
        n_files: usize,
    ) -> Result<ImageMetadata> {
        let core =
            compute_core_metadata(image_names, initial_field_count, n_planes, n_files as u32);
        let field_count = core.field_count.max(1);
        self.field_count = field_count;
        self.image_count = core.image_count.max(1);
        self.fields_in_file = n_files == 1 && field_count > 1;

        self.plate_count = 1;

        // Build base metadata from the inner TIFF, overriding the dimension
        // split + dimensionOrder per Java's populateCoreMetadata.
        let mut base_meta = self
            .inner
            .series_list()
            .first()
            .map(|s| s.metadata.clone())
            .ok_or_else(|| BioFormatsError::Format("Flex: no IFDs in first file".into()))?;
        base_meta.size_c = core.size_c.max(1);
        base_meta.size_z = core.size_z.max(1);
        base_meta.size_t = core.size_t.max(1);
        base_meta.image_count = core.image_count.max(1);
        base_meta.dimension_order = crate::common::metadata::DimensionOrder::XYCZT;
        base_meta.is_rgb = false;
        if let Some(pt) = self.scaled_pixel_type {
            base_meta.pixel_type = pt;
            base_meta.bits_per_pixel = (pt.bytes_per_sample() * 8) as u8;
        }
        Ok(base_meta)
    }

    /// Port of `FlexReader.populateMetadataStore`: replicate the base metadata
    /// across the (plate, well, field) series and bind series 0. (The OME
    /// plate/well/image records themselves are built lazily in `ome_metadata`.)
    fn populate_metadata_store(&mut self, base_meta: ImageMetadata) -> Result<()> {
        // seriesCount = plateCount * wellCount * fieldCount.
        let series_count = (self.plate_count * self.well_count * self.field_count).max(1) as usize;
        self.series_meta = vec![base_meta; series_count];
        // Cache effectiveFieldCount (Java sets it in lookupFile(int)).
        self.effective_field_count = self.effective_field_count(
            self.field_count.max(1),
            self.well_count.max(1),
            self.plate_count.max(1),
        );
        self.series = 0;
        // Bind inner to series 0's file.
        self.bind_series(0)
    }

    /// Port of `FlexReader.FlexHandler`: walk the FLEX XML in document order and
    /// populate the instrument / plane data members. `well` is the current well
    /// index, `this_field` mirrors Java's `thisField` (-1 when the well is a
    /// single file), `populate_core` mirrors Java's `populateCore` flag (only the
    /// first file in the dataset populates the instrument definitions).
    ///
    /// This is the data-member-filling subset of FlexHandler; the MetadataStore
    /// `set*` instrument-definition side effects (LaserID, ObjectiveID, etc.) are
    /// represented by the ID pools we resolve refs against.
    fn run_flex_handler(&mut self, xml: &str, well: u32, this_field: i64, populate_core: bool) {
        // first_well_planes(): planes in the first well's first file.
        let first_well_planes = self
            .flex_files
            .first()
            .and_then(|_| self.image_count.checked_mul(1))
            .filter(|&c| c > 0)
            .or_else(|| {
                self.inner
                    .series_list()
                    .first()
                    .map(|s| s.metadata.image_count)
            })
            .unwrap_or(0);

        // Handler-local cursors (mirror FlexHandler instance fields).
        let mut parent_qname = String::new();
        let mut next_laser: i64 = -1;
        let mut next_camera: i64 = 0;
        let mut next_objective: i64 = -1;
        let mut next_image: i64 = 0;
        let mut next_plate: i64 = 0;
        let mut light_source_id = String::new();
        let mut slider_name = String::new();
        let mut next_filter: i64 = 0;
        let mut next_dichroic: i64 = 0;
        let mut next_slider_ref: i64 = 0;
        let mut filter_set = String::new();

        for ev in XmlEvents::new(xml) {
            match ev {
                XmlEvent::Start { qname, attrs } => {
                    let attr = |k: &str| attrs.get(k);
                    match qname.as_str() {
                        "LightSource" => {
                            parent_qname = qname.clone();
                            if let Some(id) = attr("ID") {
                                self.light_source_ids.push(id.clone());
                            }
                            next_laser += 1;
                        }
                        "LightSourceCombination" => {
                            if let Some(id) = attr("ID") {
                                light_source_id = id.clone();
                                self.light_source_combination_ids
                                    .entry(light_source_id.clone())
                                    .or_default();
                            }
                        }
                        "LightSourceRef" => {
                            if self
                                .light_source_combination_ids
                                .contains_key(&light_source_id)
                            {
                                if let Some(refid) = attr("ID") {
                                    let id = self
                                        .light_source_ids
                                        .iter()
                                        .position(|s| s == refid)
                                        .map(|p| p as i64)
                                        .unwrap_or(-1);
                                    let lsid = create_lsid("LightSource", &[0, id.max(0) as usize]);
                                    if let Some(v) =
                                        self.light_source_combination_ids.get_mut(&light_source_id)
                                    {
                                        v.push(lsid);
                                    }
                                }
                            }
                        }
                        "Camera" if populate_core => {
                            parent_qname = qname.clone();
                            if let Some(id) = attr("ID") {
                                self.camera_ids.push(id.clone());
                            }
                            next_camera += 1;
                        }
                        "Objective" if populate_core => {
                            parent_qname = qname.clone();
                            next_objective += 1;
                            if let Some(id) = attr("ID") {
                                self.objective_ids.push(id.clone());
                            }
                        }
                        "Field" => {
                            parent_qname = qname.clone();
                            if let Some(no) = attr("No").and_then(|s| s.trim().parse::<u32>().ok())
                            {
                                // Mirror Java: bump fieldCount when this No exceeds
                                // it and the well isn't split across files.
                                let fc = self.field_count;
                                let bump = no > fc
                                    && ((this_field < 0 && fc < first_well_planes)
                                        || (fc as i64) < this_field * first_well_planes as i64);
                                if bump {
                                    self.field_count += 1;
                                }
                            }
                        }
                        "Plane" => {
                            parent_qname = qname.clone();
                        }
                        "Image" => {
                            parent_qname = qname.clone();
                            next_image += 1;
                            // FLEX v1.7 and below: binning on the Image element.
                            if let Some(x) = attr("CameraBinningX").and_then(|s| s.parse().ok()) {
                                self.bin_x = x;
                            }
                            if let Some(y) = attr("CameraBinningY").and_then(|s| s.parse().ok()) {
                                self.bin_y = y;
                            }
                        }
                        "Plate" => {
                            parent_qname = qname.clone();
                            next_plate += 1;
                            self.plate_count += 1;
                        }
                        "Slider" => {
                            if let Some(name) = attr("Name") {
                                slider_name = name.clone();
                            }
                        }
                        "Filter" => {
                            if let Some(id) = attr("ID") {
                                if slider_name.ends_with("Dichro") {
                                    let lsid =
                                        create_lsid("Dichroic", &[0, next_dichroic as usize]);
                                    if self.dichroic_map.get(id) != Some(&lsid) {
                                        self.dichroic_map.insert(id.clone(), lsid);
                                    }
                                    next_dichroic += 1;
                                } else {
                                    let lsid = create_lsid("Filter", &[0, next_filter as usize]);
                                    if self.filter_map.get(id) != Some(&lsid) {
                                        self.filter_map.insert(id.clone(), lsid);
                                    }
                                    next_filter += 1;
                                }
                            }
                        }
                        "FilterCombination" => {
                            if let Some(id) = attr("ID") {
                                filter_set = format!("FilterSet:{id}");
                                self.filter_set_map
                                    .insert(filter_set.clone(), FilterGroup::default());
                            }
                        }
                        "SliderRef" => {
                            let filter_name = attr("Filter").cloned().unwrap_or_default();
                            let slider = attr("ID").cloned().unwrap_or_default();
                            if let Some(group) = self.filter_set_map.get_mut(&filter_set) {
                                if next_slider_ref == 0 && slider.starts_with("Camera") {
                                    group.emission = self.filter_map.get(&filter_name).cloned();
                                } else if next_slider_ref == 1 && slider.starts_with("Camera") {
                                    group.excitation = self.filter_map.get(&filter_name).cloned();
                                } else if slider == "Primary_Dichro" {
                                    group.dichroic = self.dichroic_map.get(&filter_name).cloned();
                                }
                            }
                            let lname = filter_name.to_ascii_lowercase();
                            if !lname.starts_with("empty") && !lname.starts_with("blocked") {
                                next_slider_ref += 1;
                            }
                        }
                        _ => {}
                    }
                }
                XmlEvent::End { qname, value } => {
                    match qname.as_str() {
                        "XSize" if parent_qname == "Plate" => {
                            if let Ok(v) = value.trim().parse::<u32>() {
                                self.well_rows = v;
                            }
                        }
                        "YSize" if parent_qname == "Plate" => {
                            if let Ok(v) = value.trim().parse::<u32>() {
                                self.well_columns = v;
                            }
                        }
                        "PlateName" => {
                            if self.plate_name.is_none() {
                                self.plate_name = Some(value.clone());
                            }
                        }
                        "Barcode" => {
                            if self.plate_barcode.is_none() {
                                self.plate_barcode = Some(value.clone());
                            }
                        }
                        "OffsetX" => {
                            if let Ok(v) = value.trim().parse::<f64>() {
                                self.x_positions.push(v * 1_000_000.0);
                            }
                        }
                        "OffsetY" => {
                            if let Ok(v) = value.trim().parse::<f64>() {
                                self.y_positions.push(v * 1_000_000.0);
                            }
                        }
                        "FilterCombination" => {
                            // Java: nextFilterSet++; nextSliderRef = 0.
                            next_slider_ref = 0;
                        }
                        _ if parent_qname == "Image" => {
                            self.handle_image_end(
                                &qname,
                                &value,
                                well,
                                this_field,
                                first_well_planes,
                                next_image,
                            );
                        }
                        _ => {}
                    }
                    // Java appends `binX x binY` when the Image element closes.
                    if qname == "Image" {
                        self.binnings.push(format!("{}x{}", self.bin_x, self.bin_y));
                    }
                    let _ = (next_camera, next_objective, next_laser, next_plate);
                }
            }
        }
    }

    /// The `parentQName == "Image"` branch of FlexHandler.endElement: per-Image
    /// detector/objective refs, sizes, positions, plane times, acquisition dates.
    #[allow(clippy::too_many_arguments)]
    fn handle_image_end(
        &mut self,
        qname: &str,
        value: &str,
        well: u32,
        _this_field: i64,
        first_well_planes: u32,
        next_image: i64,
    ) {
        let parse_f = |s: &str| s.trim().parse::<f64>().ok();
        match qname {
            "DateTime" => {
                // currentSeries = (nextImage-1)/nImages + well*fieldCount.
                let field_count = self.field_count.max(1);
                let mut n_images = first_well_planes / field_count;
                if n_images == 0 {
                    n_images = 1;
                }
                let current_series =
                    ((next_image - 1).max(0) as u32 / n_images) + well * field_count;
                let series_count =
                    (self.plate_count.max(1) * self.well_count.max(1) * field_count) as usize;
                if (current_series as usize) < series_count && !value.is_empty() {
                    self.acquisition_dates
                        .insert(current_series as usize, value.to_string());
                }
            }
            "ObjectiveRef" => {
                if let Some(index) = self.objective_ids.iter().position(|s| s == value) {
                    self.objective_refs
                        .push(create_lsid("Objective", &[0, index]));
                }
            }
            "CameraRef" => {
                if let Some(index) = self.camera_ids.iter().position(|s| s == value) {
                    self.camera_refs.push(create_lsid("Detector", &[0, index]));
                }
            }
            "ImageResolutionX" => {
                if let Some(v) = parse_f(value) {
                    self.x_sizes.push(v * 1_000_000.0);
                }
            }
            "ImageResolutionY" => {
                if let Some(v) = parse_f(value) {
                    self.y_sizes.push(v * 1_000_000.0);
                }
            }
            "PositionX" => {
                if let Some(v) = parse_f(value) {
                    self.plane_position_x.push(v * 1_000_000.0);
                }
            }
            "PositionY" => {
                if let Some(v) = parse_f(value) {
                    self.plane_position_y.push(v * 1_000_000.0);
                }
            }
            "PositionZ" => {
                if let Some(v) = parse_f(value) {
                    self.plane_position_z.push(v * 1_000_000.0);
                }
            }
            "TimepointOffsetUsed" => {
                if let Some(v) = parse_f(value) {
                    self.plane_delta_t.push(v);
                }
            }
            "CameraExposureTime" => {
                if let Some(v) = parse_f(value) {
                    self.plane_exposure_time.push(v);
                }
            }
            "LightSourceCombinationRef" => {
                self.light_source_combination_refs.push(value.to_string());
            }
            "FilterCombinationRef" => {
                self.filter_sets.push(format!("FilterSet:{value}"));
            }
            "CameraBinningX" => {
                if let Ok(v) = value.trim().parse::<u32>() {
                    self.bin_x = v;
                }
            }
            "CameraBinningY" => {
                if let Ok(v) = value.trim().parse::<u32>() {
                    self.bin_y = v;
                }
            }
            _ => {}
        }
    }

    /// Build the per-plane OME records for one series, mirroring the plane loop
    /// of Java `populateMetadataStore` (`plane = i*imageCount + image`):
    /// positions index by `plane`, exposure by `plane - image + c`, deltaT by
    /// `plane`.
    fn build_series_planes(
        &self,
        series: usize,
        image_count: u32,
        size_c: u32,
    ) -> Vec<crate::common::ome_metadata::OmePlane> {
        let ic = image_count.max(1);
        let sc = size_c.max(1);
        (0..ic)
            .map(|image| {
                let plane = series * ic as usize + image as usize;
                // ZCT coords for this image index (XYCZT order: c fastest).
                let c = image % sc;
                let exp_idx = plane as i64 - image as i64 + c as i64;
                let mut p = crate::common::ome_metadata::OmePlane {
                    the_z: 0,
                    the_c: c,
                    the_t: 0,
                    ..Default::default()
                };
                p.position_x = self.plane_position_x.get(plane).copied();
                p.position_y = self.plane_position_y.get(plane).copied();
                p.position_z = self.plane_position_z.get(plane).copied();
                p.delta_t = self.plane_delta_t.get(plane).copied();
                if exp_idx >= 0 {
                    p.exposure_time = self.plane_exposure_time.get(exp_idx as usize).copied();
                }
                p
            })
            .collect()
    }

    /// Apply per-channel detector binning / detector ref (on the channel) and
    /// light-path filter refs (on `img.light_paths`) for series `i`, mirroring
    /// the channel loop of Java `populateMetadataStore`
    /// (indices keyed by `seriesIndex = i*imageCount`).
    fn apply_channel_instrument(&self, img: &mut crate::common::ome_metadata::OmeImage, i: usize) {
        let series_index = i * self.image_count.max(1) as usize;
        let n = img.channels.len();
        let mut light_paths: Vec<crate::common::ome_metadata::OmeLightPath> = Vec::new();
        let mut any_light_path = false;
        for c in 0..n {
            let index = series_index + c;
            if let Some(detector) = self.camera_refs.get(index) {
                img.channels[c].detector_ref = Some(detector.clone());
                if let Some(binning) = self.binnings.get(index) {
                    img.channels[c].detector_settings_binning = Some(binning.clone());
                }
            }
            // Light path from the filter set for this channel.
            let mut lp = crate::common::ome_metadata::OmeLightPath::default();
            if let Some(set) = self.filter_sets.get(index) {
                if let Some(group) = self.filter_set_map.get(set) {
                    if let Some(em) = &group.emission {
                        lp.emission_filter_ids.push(em.clone());
                    }
                    if let Some(ex) = &group.excitation {
                        lp.excitation_filter_ids.push(ex.clone());
                    }
                    if let Some(di) = &group.dichroic {
                        lp.dichroic_id = Some(di.clone());
                    }
                }
            }
            if !lp.emission_filter_ids.is_empty()
                || !lp.excitation_filter_ids.is_empty()
                || lp.dichroic_id.is_some()
            {
                any_light_path = true;
            }
            light_paths.push(lp);
        }
        if any_light_path {
            img.light_paths = light_paths;
        }
    }
}

impl Default for FlexReader {
    fn default() -> Self {
        Self::new()
    }
}

/// Compute the well index (column-major within nRows×nCols) for each file in
/// document/well order, matching how `group_files` numbers wells. Files with no
/// 14-char pattern map to well 0.
fn file_well_indices(flex_files: &[FlexFile]) -> Vec<u32> {
    use std::collections::BTreeMap;
    let mut order: BTreeMap<(u32, u32), u32> = BTreeMap::new();
    let mut next = 0u32;
    let mut out = Vec::with_capacity(flex_files.len());
    for f in flex_files {
        let key = (f.row, f.column);
        let idx = *order.entry(key).or_insert_with(|| {
            let v = next;
            next += 1;
            v
        });
        out.push(idx);
    }
    out
}

/// A minimal SAX-like event over an XML string: element start (with attributes)
/// and element end (with accumulated character data), mirroring the subset of
/// `DefaultHandler` callbacks that `FlexHandler` uses.
enum XmlEvent {
    Start {
        qname: String,
        attrs: std::collections::HashMap<String, String>,
    },
    End {
        qname: String,
        value: String,
    },
}

/// Streaming tokenizer producing `XmlEvent`s. Character data between a start tag
/// and its matching end tag is reported on the `End` event (Java accumulates it
/// in `charData` and reads it in `endElement`). Self-closing tags emit a `Start`
/// immediately followed by an `End` with empty value.
struct XmlEvents<'a> {
    s: &'a str,
    i: usize,
    char_data: String,
    /// Pending End events queued from a self-closing tag.
    pending: std::collections::VecDeque<XmlEvent>,
}

impl<'a> XmlEvents<'a> {
    fn new(s: &'a str) -> Self {
        XmlEvents {
            s,
            i: 0,
            char_data: String::new(),
            pending: std::collections::VecDeque::new(),
        }
    }
}

impl Iterator for XmlEvents<'_> {
    type Item = XmlEvent;

    fn next(&mut self) -> Option<XmlEvent> {
        if let Some(ev) = self.pending.pop_front() {
            return Some(ev);
        }
        let bytes = self.s.as_bytes();
        loop {
            // accumulate char data until the next '<'
            let lt = self.s[self.i..].find('<').map(|r| self.i + r)?;
            self.char_data.push_str(&self.s[self.i..lt]);
            let gt = self.s[lt..].find('>').map(|r| lt + r)?;
            let tag = &self.s[lt + 1..gt];
            self.i = gt + 1;

            if tag.starts_with('?') || tag.starts_with('!') {
                // declaration / comment / CDATA-ish: skip, keep char data.
                continue;
            }
            if let Some(name) = tag.strip_prefix('/') {
                // end tag: emit End with the collected char data.
                let qname = name.trim().to_string();
                let value = std::mem::take(&mut self.char_data);
                return Some(XmlEvent::End { qname, value });
            }
            // start tag (possibly self-closing).
            let self_closing = tag.trim_end().ends_with('/');
            let inner = if self_closing {
                tag.trim_end().trim_end_matches('/')
            } else {
                tag
            };
            let qname = inner
                .split([' ', '\t', '\n', '\r'])
                .next()
                .unwrap_or("")
                .to_string();
            let attrs = parse_tag_attrs(inner);
            self.char_data.clear();
            if self_closing {
                self.pending.push_back(XmlEvent::End {
                    qname: qname.clone(),
                    value: String::new(),
                });
            }
            let _ = bytes; // silence unused in case of empty input
            return Some(XmlEvent::Start { qname, attrs });
        }
    }
}

/// Parse all `name="value"` / `name='value'` attributes from a start-tag body.
fn parse_tag_attrs(tag: &str) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    let bytes = tag.as_bytes();
    let mut i = 0;
    // skip the element name
    while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let name_start = i;
        while i < bytes.len() && bytes[i] != b'=' && !bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let name = tag[name_start..i].trim();
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b'=' {
            if name.is_empty() {
                break;
            }
            continue;
        }
        i += 1; // skip '='
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let quote = bytes[i];
        if quote != b'"' && quote != b'\'' {
            continue;
        }
        i += 1;
        let val_start = i;
        while i < bytes.len() && bytes[i] != quote {
            i += 1;
        }
        let value = &tag[val_start..i.min(tag.len())];
        if !name.is_empty() {
            map.insert(name.to_string(), value.to_string());
        }
        i += 1; // skip closing quote
    }
    map
}

/// FormatTools.rasterToPosition: convert a raster index to per-axis positions
/// (axis 0 fastest-varying), mirroring the Java helper.
fn raster_to_position(lengths: &[usize], mut raster: usize) -> Vec<usize> {
    let mut pos = vec![0usize; lengths.len()];
    for (i, &len) in lengths.iter().enumerate() {
        let len = len.max(1);
        pos[i] = raster % len;
        raster /= len;
    }
    pos
}

/// Read an unsigned integer of `n` bytes from `buf` at `off`.
fn read_uint(buf: &[u8], off: usize, n: usize, little_endian: bool) -> u64 {
    let mut v = 0u64;
    for i in 0..n {
        let byte = buf.get(off + i).copied().unwrap_or(0) as u64;
        if little_endian {
            v |= byte << (8 * i);
        } else {
            v = (v << 8) | byte;
        }
    }
    v
}

/// Write an unsigned integer of `n` bytes into `buf` at `off`.
fn write_uint(buf: &mut [u8], off: usize, n: usize, value: u64, little_endian: bool) {
    for i in 0..n {
        let shift = if little_endian {
            8 * i
        } else {
            8 * (n - 1 - i)
        };
        if let Some(slot) = buf.get_mut(off + i) {
            *slot = ((value >> shift) & 0xff) as u8;
        }
    }
}

/// Parse all `<Array ... Name=... Factor=...>` elements, returning the lists of
/// names and factor strings in document order (mirrors FlexHandler).
fn parse_flex_arrays(xml: &str) -> (Vec<String>, Vec<String>) {
    let mut names = Vec::new();
    let mut factors = Vec::new();
    let bytes = xml.as_bytes();
    let mut i = 0;
    while let Some(rel) = xml[i..].find("<Array") {
        let start = i + rel;
        let end = match xml[start..].find('>') {
            Some(e) => start + e,
            None => break,
        };
        let tag = &xml[start..end];
        if let Some(name) = xml_attr(tag, "Name") {
            names.push(name);
        }
        if let Some(factor) = xml_attr(tag, "Factor") {
            factors.push(factor);
        }
        i = end + 1;
        if i >= bytes.len() {
            break;
        }
    }
    (names, factors)
}

/// Spreadsheet-style well row name (0 -> "A", 25 -> "Z", 26 -> "AA"), mirroring
/// `FormatTools.getWellRowName`.
fn well_row_name(row: u32) -> String {
    let last = char::from(b'A' + (row % 26) as u8);
    if row >= 26 {
        let first = char::from(b'A' + ((row / 26) - 1) as u8);
        format!("{first}{last}")
    } else {
        last.to_string()
    }
}

/// Result of the Java `populateCoreMetadata` dimension computation.
struct FlexCore {
    size_c: u32,
    size_z: u32,
    size_t: u32,
    image_count: u32,
    field_count: u32,
}

/// Port of `FlexReader.populateCoreMetadata` dimension logic.
///
/// `image_names` are the per-plane `<Array Name>` values, `initial_field_count`
/// is the count seeded by the `<Field No>` handler (single-file mode) or 0,
/// `n_planes` is the IFD count of the first file, `n_files` the file count.
fn compute_core_metadata(
    image_names: &[String],
    initial_field_count: u32,
    n_planes: u32,
    n_files: u32,
) -> FlexCore {
    let n_names = image_names.len();
    let mut field_count = initial_field_count;
    let mut size_c;
    let mut size_z;
    let mut size_t;

    // sizeC == 0 && sizeT == 0 branch (always true at populate time).
    if field_count == 0 || (n_names != 0 && n_names % field_count as usize != 0) {
        field_count = 1;
    }
    let mut unique_channels: Vec<&str> = Vec::new();
    for name in image_names {
        let by_underscore: Vec<&str> = name.split('_').collect();
        let tokens = if by_underscore.len() > 1 {
            // fields are indexed from 1
            if let Ok(field_index) = by_underscore[0].parse::<u32>() {
                if field_index > field_count {
                    field_count = field_index;
                }
            }
            by_underscore
        } else {
            name.split(':').collect()
        };
        let channel = *tokens.last().unwrap_or(&name.as_str());
        if !unique_channels.contains(&channel) {
            unique_channels.push(channel);
        }
    }
    if field_count == 0 {
        field_count = 1;
    }
    size_c = (unique_channels.len() as u32).max(1);
    size_z = 1;
    size_t = (n_names as u32 / (field_count * size_c * size_z)).max(1);

    if field_count == 0 {
        field_count = 1;
    }

    let mut image_count = size_z * size_c * size_t;

    if image_count as usize == n_names {
        field_count = 1;
    }

    // If the calculated image count differs from the number of planes in the
    // file, assume fields are stored within the file.
    if image_count * field_count != n_planes
        && ((image_count != n_planes && n_files > 1) || n_files == 1)
    {
        let per_field = (n_planes / field_count.max(1)).max(1);
        image_count = per_field;
        size_z = 1;
        size_t = per_field;
        if size_t % size_c == 0 {
            size_t /= size_c;
        } else {
            size_c = 1;
        }
    }

    if field_count == 1 {
        field_count *= n_files.max(1);
    }

    FlexCore {
        size_c: size_c.max(1),
        size_z: size_z.max(1),
        size_t: size_t.max(1),
        image_count: image_count.max(1),
        field_count: field_count.max(1),
    }
}

/// Count `<Field No="...">` elements, mirroring the Java FlexHandler's field
/// counter for the single-file case (`thisField < 0`). Each `<Field No>` whose
/// number exceeds the running count and stays below the plane count bumps the
/// field count by one. Returns the resulting field count.
fn count_fields(xml: &str, plane_count: usize) -> u32 {
    let mut field_count = 0u32;
    let mut i = 0;
    while let Some(rel) = xml[i..].find("<Field") {
        let start = i + rel;
        let end = match xml[start..].find('>') {
            Some(e) => start + e,
            None => break,
        };
        let tag = &xml[start..end];
        // Only a true <Field ...> start tag (next char after "Field" is space or >).
        let after = tag.as_bytes().get(6).copied();
        let is_field = matches!(after, Some(b) if b == b' ' || b == b'\t' || b == b'\n' || b == b'\r')
            || tag.len() == 6;
        if is_field {
            if let Some(no) = xml_attr(tag, "No").and_then(|s| s.trim().parse::<u32>().ok()) {
                // thisField < 0 branch: fieldNo > fieldCount && fieldCount < planeCount.
                if no > field_count && (field_count as usize) < plane_count {
                    field_count += 1;
                }
            }
        }
        i = end + 1;
    }
    field_count
}

/// Parse the first `<ImageResolutionX>`/`<ImageResolutionY>` decimal values
/// (in metres) and convert to microns (× 1e6), mirroring the Java handler's
/// `xSizes`/`ySizes` (value * 1000000).
fn parse_physical_size(xml: &str) -> Option<(f64, f64)> {
    let parse_one = |tag: &str| -> Option<f64> {
        let idx = xml.find(tag)?;
        let gt = xml[idx..].find('>').map(|e| idx + e + 1)?;
        let lt = xml[gt..].find('<').map(|e| gt + e)?;
        let v: f64 = xml[gt..lt].trim().parse().ok()?;
        Some(v * 1_000_000.0)
    };
    let x = parse_one("<ImageResolutionX");
    let y = parse_one("<ImageResolutionY");
    match (x, y) {
        (Some(x), Some(y)) => Some((x, y)),
        (Some(x), None) => Some((x, x)),
        (None, Some(y)) => Some((y, y)),
        (None, None) => None,
    }
}

/// Extract an XML attribute value (`attr="value"`) from a single start tag.
fn xml_attr(tag: &str, attr: &str) -> Option<String> {
    let mut search_from = 0;
    while let Some(rel) = tag[search_from..].find(attr) {
        let pos = search_from + rel;
        let prev_ok = pos == 0 || tag.as_bytes()[pos - 1].is_ascii_whitespace();
        let after = pos + attr.len();
        let rest = tag[after..].trim_start();
        if prev_ok && rest.starts_with('=') {
            let rest = rest[1..].trim_start();
            let quote = rest.chars().next()?;
            if quote == '"' || quote == '\'' {
                let val_start = 1;
                if let Some(end) = rest[val_start..].find(quote) {
                    return Some(rest[val_start..val_start + end].to_string());
                }
            }
        }
        search_from = after;
    }
    None
}

/// Extract the text content of the first `<tag>..</tag>` (or `Barcode` style)
/// occurrence. Used for plate Barcode/PlateName from the Flex XML.
fn xml_element_text(xml: &str, name: &str) -> Option<String> {
    let needle = name;
    let idx = xml.find(needle)?;
    let start = xml[idx..].find('>').map(|e| idx + e + 1)?;
    let end = xml[idx..].find('<').map(|e| idx + e)?;
    if end > start {
        Some(xml[start..end].to_string())
    } else {
        None
    }
}

/// Parse the well row/column (0-based) from a 14-char `nnnnnnnnn.flex` name.
/// Returns None if the name does not match the pattern.
fn parse_well(name: &str) -> Option<(u32, u32)> {
    if name.len() == 14 && name.to_ascii_lowercase().ends_with(".flex") {
        let row = name.get(0..3)?.parse::<u32>().ok()?;
        let col = name.get(3..6)?.parse::<u32>().ok()?;
        return Some((row.saturating_sub(1), col.saturating_sub(1)));
    }
    None
}

/// Parse the field index (0-based) from a 14-char `nnnnnnnnn.flex` name.
fn parse_field(name: &str) -> u32 {
    if name.len() == 14 && name.to_ascii_lowercase().ends_with(".flex") {
        if let Some(s) = name.get(6..9) {
            if let Ok(v) = s.parse::<u32>() {
                return v.saturating_sub(1);
            }
        }
    }
    0
}

/// Parse a `.mea` file's `<Picture path=...>` entries into a list of `.flex`
/// file names (relative). Mirrors MeaHandler (minus server-name remapping,
/// which is not applicable without a configured server map).
fn parse_mea_flex_names(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(rel) = text[i..].find("<Picture") {
        let start = i + rel;
        let end = match text[start..].find('>') {
            Some(e) => start + e,
            None => break,
        };
        let tag = &text[start..end];
        if let Some(mut path) = xml_attr(tag, "path") {
            if !path.to_ascii_lowercase().ends_with(".flex") {
                path.push_str(".flex");
            }
            // Normalise separators to native; we only use the file name below.
            let path = path.replace('\\', "/");
            out.push(path);
        }
        i = end + 1;
    }
    out
}

/// Parse the plate acquisition date attribute from a `.res` file
/// (`<AnalysisResults date="...">`). Mirrors ResHandler.
fn parse_res_date(text: &str) -> Option<String> {
    let idx = text.find("<AnalysisResults")?;
    let end = text[idx..].find('>').map(|e| idx + e)?;
    let tag = &text[idx..end];
    xml_attr(tag, "date")
}

/// Find companion `.mea`/`.res` files in the same directory as the `.flex`.
fn find_measurement_files(flex_path: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Some(dir) = flex_path.parent() else {
        return out;
    };
    if let Ok(rd) = std::fs::read_dir(dir) {
        for entry in rd.flatten() {
            let p = entry.path();
            let ext = p
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_ascii_lowercase());
            if matches!(ext.as_deref(), Some("mea") | Some("res")) {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// Collect grouped `.flex` files for a dataset. Returns the sorted list of
/// 14-char-pattern `.flex` files in the same directory, or just the input file
/// when no grouping is possible.
fn collect_flex_files(flex_path: &Path) -> Vec<PathBuf> {
    let name = flex_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    // Only group when the file follows the nnnnnnnnn.flex naming convention.
    if parse_well(name).is_none() {
        return vec![flex_path.to_path_buf()];
    }
    let Some(dir) = flex_path.parent() else {
        return vec![flex_path.to_path_buf()];
    };
    let mut files: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for entry in rd.flatten() {
            let p = entry.path();
            let n = p.file_name().and_then(|x| x.to_str()).unwrap_or_default();
            if n.len() == 14 && n.to_ascii_lowercase().ends_with(".flex") {
                files.push(p);
            }
        }
    }
    if files.is_empty() {
        files.push(flex_path.to_path_buf());
    }
    files.sort();
    files
}

impl FormatReader for FlexReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| {
                e.eq_ignore_ascii_case("flex")
                    || e.eq_ignore_ascii_case("mea")
                    || e.eq_ignore_ascii_case("res")
            })
            .unwrap_or(false)
    }
    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        if header.len() < 4 {
            return false;
        }
        (header[0] == 0x49 && header[1] == 0x49 && header[2] == 0x2A && header[3] == 0x00)
            || (header[0] == 0x4D && header[1] == 0x4D && header[2] == 0x00 && header[3] == 0x2A)
            || (header[0] == 0x49 && header[1] == 0x49 && header[2] == 0x2B && header[3] == 0x00)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let _ = self.close();
        let result = self.init_file(path);
        if result.is_err() {
            let _ = self.close();
        }
        result
    }

    fn close(&mut self) -> Result<()> {
        self.flex_files.clear();
        self.inner_path = None;
        self.scaled_pixel_type = None;
        self.series_meta.clear();
        self.well_number.clear();
        self.measurement_files.clear();
        self.plate_acq_start_time = None;
        self.plate_name = None;
        self.plate_barcode = None;
        self.series = 0;
        self.single_file = true;
        self.fields_in_file = false;
        self.channel_names.clear();
        self.physical_size = None;
        self.bin_x = 0;
        self.bin_y = 0;
        self.effective_field_count = 0;
        self.binnings.clear();
        self.camera_ids.clear();
        self.objective_ids.clear();
        self.light_source_ids.clear();
        self.camera_refs.clear();
        self.objective_refs.clear();
        self.x_sizes.clear();
        self.y_sizes.clear();
        self.x_positions.clear();
        self.y_positions.clear();
        self.light_source_combination_refs.clear();
        self.filter_sets.clear();
        self.light_source_combination_ids.clear();
        self.filter_map.clear();
        self.dichroic_map.clear();
        self.filter_set_map.clear();
        self.plane_position_x.clear();
        self.plane_position_y.clear();
        self.plane_position_z.clear();
        self.plane_delta_t.clear();
        self.plane_exposure_time.clear();
        self.acquisition_dates.clear();
        self.inner.close()
    }

    fn series_count(&self) -> usize {
        if self.single_file {
            self.inner.series_count()
        } else {
            self.series_meta.len()
        }
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.single_file {
            if self.inner.series_count() == 0 {
                return Err(BioFormatsError::NotInitialized);
            }
            self.inner.set_series(s)?;
            self.series = s;
            Ok(())
        } else {
            if s >= self.series_meta.len() {
                return Err(BioFormatsError::SeriesOutOfRange(s));
            }
            self.bind_series(s)
        }
    }

    fn series(&self) -> usize {
        self.series
    }

    fn metadata(&self) -> &ImageMetadata {
        if self.single_file {
            self.inner.metadata()
        } else {
            self.series_meta
                .get(self.series)
                .unwrap_or(crate::common::reader::uninitialized_metadata())
        }
    }

    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        if !self.single_file {
            self.bind_series(self.series)?;
        }
        let ip = self.inner_plane(p);
        let le = self.inner.is_little_endian();
        let raw = self.inner.open_bytes(ip)?;
        Ok(self.apply_factor(raw, ip, le))
    }

    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        if !self.single_file {
            self.bind_series(self.series)?;
        }
        let ip = self.inner_plane(p);
        let le = self.inner.is_little_endian();
        let raw = self.inner.open_bytes_region(ip, x, y, w, h)?;
        Ok(self.apply_factor(raw, ip, le))
    }

    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        if !self.single_file {
            self.bind_series(self.series)?;
        }
        let ip = self.inner_plane(p);
        self.inner.open_thumb_bytes(ip)
    }

    fn resolution_count(&self) -> usize {
        self.inner.resolution_count()
    }
    fn set_resolution(&mut self, l: usize) -> Result<()> {
        self.inner.set_resolution(l)
    }
    fn resolution(&self) -> usize {
        self.inner.resolution()
    }

    fn ome_metadata(&self) -> Option<OmeMetadata> {
        if std::ptr::eq(
            self.metadata(),
            crate::common::reader::uninitialized_metadata(),
        ) {
            return None;
        }
        let mut ome = OmeMetadata::from_image_metadata(self.metadata());
        if self.single_file {
            // Surface per-plane positions / times for the single series, and the
            // channel names parsed from the FLEX XML (mirrors FlexHandler).
            if let Some(img) = ome.images.first_mut() {
                let ic = self.image_count.max(1);
                img.planes = self.build_series_planes(0, ic, self.metadata().size_c.max(1));
                if let Some(date) = self.acquisition_dates.get(&0) {
                    img.acquisition_date = Some(date.clone());
                }
                if self.objective_refs.first().is_some() {
                    img.objective_ref = Some(0);
                }
                self.apply_channel_instrument(img, 0);
            }
            return Some(ome);
        }

        // Build one Image per series; one Plate with Wells/WellSamples.
        let series_count = self.series_meta.len();
        let field_count = self.field_count.max(1) as usize;
        let well_count = self.well_count.max(1) as usize;
        let plate_count = self.plate_count.max(1) as usize;
        let lengths = [field_count, well_count, plate_count];

        // Per-series Image: name "Well <row>-<col>; Field #<n>", physical size,
        // and channels named from the FLEX <Array Name> values (Java
        // populateMetadataStore). channelIndex = series*effSizeC + c when the
        // name list has one entry per (series, channel).
        let eff_size_c = self
            .series_meta
            .first()
            .map(|m| m.size_c.max(1) as usize)
            .unwrap_or(1);
        ome.images = (0..series_count)
            .map(|i| {
                let pos = raster_to_position(&lengths, i);
                let (row, col) = self.well_number.get(pos[1]).copied().unwrap_or((0, 0));
                let mut img = crate::common::ome_metadata::OmeImage {
                    name: Some(format!(
                        "Well {}-{}; Field #{}",
                        well_row_name(row),
                        col + 1,
                        pos[0] + 1
                    )),
                    ..Default::default()
                };
                // Per-Image physical size: Java prefers the per-series xSizes/
                // ySizes (indexed by series*imageCount); fall back to the first
                // physical_size.
                let series_index = i * self.image_count.max(1) as usize;
                if let Some(&sx) = self.x_sizes.get(series_index) {
                    img.physical_size_x = Some(sx);
                } else if let Some((px, _)) = self.physical_size {
                    img.physical_size_x = Some(px);
                }
                if let Some(&sy) = self.y_sizes.get(series_index) {
                    img.physical_size_y = Some(sy);
                } else if let Some((_, py)) = self.physical_size {
                    img.physical_size_y = Some(py);
                }
                // Acquisition date for this series.
                if let Some(date) = self.acquisition_dates.get(&i) {
                    img.acquisition_date = Some(date.clone());
                }
                // Instrument + objective settings ref (objectiveRefs[series*imageCount]).
                img.instrument_ref = Some(0);
                if self.objective_refs.get(series_index).is_some() {
                    img.objective_ref = Some(0);
                }
                img.channels = (0..eff_size_c)
                    .map(|c| {
                        // Mirror Java's channelIndex selection.
                        let mut idx = i * self.image_count.max(1) as usize + c;
                        if self.channel_names.len() == eff_size_c * series_count {
                            idx = i * eff_size_c + c;
                        }
                        if idx >= self.channel_names.len() {
                            idx = c;
                        }
                        crate::common::ome_metadata::OmeChannel {
                            name: self.channel_names.get(idx).cloned(),
                            samples_per_pixel: 1,
                            ..Default::default()
                        }
                    })
                    .collect();
                // Detector/binning/light-path settings per channel.
                self.apply_channel_instrument(&mut img, i);
                // Per-plane positions / times.
                img.planes =
                    self.build_series_planes(i, self.image_count.max(1), eff_size_c as u32);
                img
            })
            .collect();

        let mut plate = OmePlate {
            id: Some(create_lsid("Plate", &[0])),
            name: self
                .plate_name
                .clone()
                .or_else(|| Some("Plate".to_string())),
            rows: self.well_rows,
            columns: self.well_columns,
            wells: Vec::new(),
        };
        if let Some(barcode) = &self.plate_barcode {
            plate.name = Some(match &plate.name {
                Some(n) => format!("{barcode} {n}"),
                None => barcode.clone(),
            });
        }

        // Index wells by (row,col) -> Vec<WellSample>.
        use std::collections::BTreeMap;
        let mut well_map: BTreeMap<(u32, u32), Vec<OmeWellSample>> = BTreeMap::new();

        for i in 0..series_count {
            let pos = raster_to_position(&lengths, i);
            let (row, col) = self.well_number.get(pos[1]).copied().unwrap_or((0, 0));
            let field = pos[0] as u32;
            well_map.entry((row, col)).or_default().push(OmeWellSample {
                id: Some(create_lsid(
                    "WellSample",
                    &[
                        pos[2],
                        (row * self.well_columns + col) as usize,
                        field as usize,
                    ],
                )),
                index: i as u32,
                image_ref: Some(i),
                // Java sets WellSamplePosition{X,Y} from x/yPositions[pos[0]].
                position_x: self.x_positions.get(pos[0] as usize).copied(),
                position_y: self.y_positions.get(pos[0] as usize).copied(),
            });
        }

        for ((row, col), samples) in well_map {
            plate.wells.push(OmeWell {
                id: Some(create_lsid(
                    "Well",
                    &[0, (row * self.well_columns + col) as usize],
                )),
                row,
                column: col,
                well_samples: samples,
            });
        }

        ome.plates = vec![plate];
        Some(ome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_well_and_field_from_14char_name() {
        // 002003001.flex -> row 1, col 2, field 0 (all 1-based in file)
        assert_eq!(parse_well("002003001.flex"), Some((1, 2)));
        assert_eq!(parse_field("002003001.flex"), 0);
        assert_eq!(parse_field("002003004.flex"), 3);
    }

    #[test]
    fn non_pattern_name_is_not_groupable() {
        assert_eq!(parse_well("test.flex"), None);
        assert_eq!(parse_well("image001.flex"), None);
    }

    #[test]
    fn mea_picture_paths_get_flex_extension() {
        let mea =
            r#"<root><Picture path="dir/002003001"/><Picture path="dir\002003002.flex"/></root>"#;
        let names = parse_mea_flex_names(mea);
        assert_eq!(names, vec!["dir/002003001.flex", "dir/002003002.flex"]);
    }

    #[test]
    fn res_date_parsed() {
        let res = r#"<AnalysisResults date="01.02.2010  10:20:30" foo="bar">"#;
        assert_eq!(
            parse_res_date(res),
            Some("01.02.2010  10:20:30".to_string())
        );
    }

    #[test]
    fn raster_to_position_axis0_fastest() {
        // lengths {field=2, well=3, plate=1}; series 4 -> field 0, well 2.
        let p = raster_to_position(&[2, 3, 1], 4);
        assert_eq!(p, vec![0, 2, 0]);
    }

    #[test]
    fn fields_within_one_file_yield_one_series_per_field() {
        // 6 IFDs, 2 channels (Exp1Cam1/Exp1Cam2) repeated per field, 3 <Field No>.
        let names: Vec<String> = ["Exp1Cam1", "Exp1Cam2"]
            .iter()
            .cycle()
            .take(6)
            .map(|s| s.to_string())
            .collect();
        let core = compute_core_metadata(&names, 3, 6, 1);
        assert_eq!(core.size_c, 2);
        assert_eq!(core.size_z, 1);
        assert_eq!(core.size_t, 1);
        assert_eq!(core.image_count, 2);
        assert_eq!(core.field_count, 3);
    }

    #[test]
    fn count_fields_counts_field_no_elements() {
        let xml = r#"<a><Field No="1"></Field><Field No="2"/><Field No="3"/></a>"#;
        assert_eq!(count_fields(xml, 6), 3);
        // capped by plane count
        assert_eq!(count_fields(xml, 2), 2);
    }

    #[test]
    fn physical_size_from_image_resolution() {
        let xml = r#"<ImageResolutionX Unit="m">1.076e-007</ImageResolutionX>
                     <ImageResolutionY Unit="m">1.076e-007</ImageResolutionY>"#;
        let (x, y) = parse_physical_size(xml).unwrap();
        assert!((x - 0.1076).abs() < 1e-9, "x={x}");
        assert!((y - 0.1076).abs() < 1e-9, "y={y}");
    }

    #[test]
    fn well_row_name_letters() {
        assert_eq!(well_row_name(0), "A");
        assert_eq!(well_row_name(25), "Z");
        assert_eq!(well_row_name(26), "AA");
    }

    #[test]
    fn flex_arrays_parse_name_and_factor() {
        let xml = r#"<Arrays><Array Name="1_ch1" Factor="2.0"/><Array Name="1_ch2" Factor="1"/></Arrays>"#;
        let (names, factors) = parse_flex_arrays(xml);
        assert_eq!(names, vec!["1_ch1", "1_ch2"]);
        assert_eq!(factors, vec!["2.0", "1"]);
    }

    #[test]
    fn tag_attrs_parse_quoted_values() {
        let m = parse_tag_attrs(r#"Filter ID="abc" Name='Slider 1' Skip"#);
        assert_eq!(m.get("ID").map(|s| s.as_str()), Some("abc"));
        assert_eq!(m.get("Name").map(|s| s.as_str()), Some("Slider 1"));
        assert_eq!(m.get("Skip"), None);
    }

    #[test]
    fn xml_events_start_end_chardata_and_selfclose() {
        let xml = r#"<a><b x="1">hi</b><c y="2"/></a>"#;
        let mut starts = Vec::new();
        let mut ends = Vec::new();
        for ev in XmlEvents::new(xml) {
            match ev {
                XmlEvent::Start { qname, attrs } => starts.push((qname, attrs)),
                XmlEvent::End { qname, value } => ends.push((qname, value)),
            }
        }
        assert_eq!(starts.len(), 3); // a, b, c (self-close still one Start)
                                     // Start order: a, b, c. End order: b("hi"), c(""), a("").
        assert_eq!(starts[1].0, "b");
        assert_eq!(starts[1].1.get("x").map(|s| s.as_str()), Some("1"));
        assert!(ends.iter().any(|(q, v)| q == "b" && v == "hi"));
        assert!(ends.iter().any(|(q, v)| q == "c" && v.is_empty()));
    }

    #[test]
    fn file_well_indices_column_major() {
        let mk = |row, column, field| FlexFile {
            row,
            column,
            field,
            path: PathBuf::from("x"),
            factors: None,
        };
        // two wells, two fields each, well order (0,0) then (0,1).
        let files = vec![mk(0, 0, 0), mk(0, 0, 1), mk(0, 1, 0), mk(0, 1, 1)];
        assert_eq!(file_well_indices(&files), vec![0, 0, 1, 1]);
    }

    /// Run the FlexHandler over a representative single-file FLEX XML and assert
    /// every newly-captured data member is populated.
    fn run_handler_on(xml: &str) -> FlexReader {
        let mut r = FlexReader::new();
        // single-file context: one well/plate, two fields in file, 2 planes/field.
        r.field_count = 2;
        r.well_count = 1;
        r.plate_count = 1;
        r.image_count = 1;
        r.flex_files = vec![FlexFile {
            row: 0,
            column: 0,
            field: 0,
            path: PathBuf::from("x"),
            factors: None,
        }];
        r.run_flex_handler(xml, 0, -1, true);
        r
    }

    #[test]
    fn handler_captures_instrument_and_plane_members() {
        // One Camera + Objective definition, then two Images each referencing
        // them, with binning, positions, exposure, delta-t, offsets, sizes.
        let xml = r#"<root>
          <Camera ID="cam0" CameraType="CCD"/>
          <Objective ID="obj0"/>
          <Slider Name="Camera 1"/>
          <Filter ID="f0"/>
          <Slider Name="Primary_Dichro"/>
          <Filter ID="d0"/>
          <FilterCombination ID="set0">
            <SliderRef ID="Camera 1" Filter="f0"/>
            <SliderRef ID="Primary_Dichro" Filter="d0"/>
          </FilterCombination>
          <OffsetX>0.001</OffsetX>
          <OffsetY>0.002</OffsetY>
          <Image>
            <CameraBinningX>2</CameraBinningX>
            <CameraBinningY>2</CameraBinningY>
            <CameraRef>cam0</CameraRef>
            <ObjectiveRef>obj0</ObjectiveRef>
            <ImageResolutionX>1.0e-7</ImageResolutionX>
            <ImageResolutionY>1.0e-7</ImageResolutionY>
            <PositionX>0.00010</PositionX>
            <PositionY>0.00020</PositionY>
            <PositionZ>0.00030</PositionZ>
            <TimepointOffsetUsed>1.5</TimepointOffsetUsed>
            <CameraExposureTime>0.25</CameraExposureTime>
            <LightSourceCombinationRef>lsc0</LightSourceCombinationRef>
            <FilterCombinationRef>set0</FilterCombinationRef>
            <DateTime>2010-02-01T10:20:30</DateTime>
          </Image>
          <Image>
            <CameraRef>cam0</CameraRef>
            <PositionX>0.00011</PositionX>
            <CameraExposureTime>0.30</CameraExposureTime>
          </Image>
        </root>"#;
        let r = run_handler_on(xml);

        // ID pools + resolved refs.
        assert_eq!(r.camera_ids, vec!["cam0"]);
        assert_eq!(r.objective_ids, vec!["obj0"]);
        assert_eq!(r.camera_refs, vec!["Detector:0:0", "Detector:0:0"]);
        assert_eq!(r.objective_refs, vec!["Objective:0:0"]);

        // binnings: one per Image close ("2x2" then carries over to second).
        assert_eq!(r.binnings.len(), 2);
        assert_eq!(r.binnings[0], "2x2");

        // sizes / offsets (microns).
        assert!((r.x_sizes[0] - 0.1).abs() < 1e-9);
        assert!((r.y_sizes[0] - 0.1).abs() < 1e-9);
        assert!((r.x_positions[0] - 1000.0).abs() < 1e-6);
        assert!((r.y_positions[0] - 2000.0).abs() < 1e-6);

        // plane positions / times.
        assert_eq!(r.plane_position_x.len(), 2);
        assert!((r.plane_position_x[0] - 100.0).abs() < 1e-6);
        assert!((r.plane_position_y[0] - 200.0).abs() < 1e-6);
        assert!((r.plane_position_z[0] - 300.0).abs() < 1e-6);
        assert_eq!(r.plane_delta_t, vec![1.5]);
        assert_eq!(r.plane_exposure_time, vec![0.25, 0.30]);

        // filter sets / light path.
        assert_eq!(r.filter_sets, vec!["FilterSet:set0"]);
        assert_eq!(r.light_source_combination_refs, vec!["lsc0"]);
        let group = r.filter_set_map.get("FilterSet:set0").unwrap();
        assert_eq!(group.emission.as_deref(), Some("Filter:0:0"));
        assert_eq!(group.dichroic.as_deref(), Some("Dichroic:0:0"));

        // acquisition date for series 0.
        assert_eq!(
            r.acquisition_dates.get(&0).map(|s| s.as_str()),
            Some("2010-02-01T10:20:30")
        );
    }

    #[test]
    fn build_series_planes_indexes_positions() {
        let mut r = FlexReader::new();
        r.plane_position_x = vec![10.0, 11.0, 12.0, 13.0];
        r.plane_exposure_time = vec![0.5, 0.6];
        r.series_meta = vec![ImageMetadata::default()];
        // series 1, 2 images per series.
        let planes = r.build_series_planes(1, 2, 1);
        assert_eq!(planes.len(), 2);
        // plane = 1*2 + image -> 2, 3.
        assert!((planes[0].position_x.unwrap() - 12.0).abs() < 1e-9);
        assert!((planes[1].position_x.unwrap() - 13.0).abs() < 1e-9);
    }
}
