//! bioformats-mias — format readers:
//!
//! - CellWorxReader: CellWorX HCS (.htd / .pnl)
//! - AliconaReader: 3D image format (.al3d) with "AL3D" magic
//! - OxfordInstrumentsReader: Oxford Instruments SEM/AFM (.top)
//! - FeiSerReader: FEI SER electron-microscopy series (.ser)

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;

// ── Helpers ───────────────────────────────────────────────────────────────────

fn simple_meta(w: u32, h: u32, z: u32, pt: PixelType) -> ImageMetadata {
    let bps = pt.bytes_per_sample();
    ImageMetadata {
        size_x: w,
        size_y: h,
        size_z: z,
        size_c: 1,
        size_t: 1,
        pixel_type: pt,
        bits_per_pixel: (bps * 8) as u8,
        image_count: z,
        dimension_order: DimensionOrder::XYZCT,
        is_rgb: false,
        is_interleaved: false,
        is_indexed: false,
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

fn checked_payload_len(meta: &ImageMetadata) -> Result<u64> {
    let bps = meta.pixel_type.bytes_per_sample() as u64;
    (meta.size_x as u64)
        .checked_mul(meta.size_y as u64)
        .and_then(|px| px.checked_mul(bps))
        .and_then(|plane| plane.checked_mul(meta.image_count as u64))
        .ok_or_else(|| BioFormatsError::Format("declared image payload size overflows".into()))
}

// ── CellWorxReader ────────────────────────────────────────────────────────────

/// CellWorX / MetaXpress HCS reader.
///
/// Ported from the upstream Java `CellWorxReader` and its `MetaxpressTiffReader`
/// subclass. The entry point is a `.HTD` plate-index file (flat `"key", value`
/// text) describing the well grid, the site (field) grid, the timepoint/Z-step
/// counts and the wavelengths. Pixel data live in per-well/per-wavelength TIFF
/// files named `<plate>_<well>_w<wave>.TIF`; pixel reads are delegated to
/// [`crate::tiff::TiffReader`].
///
/// One series is produced per well x field. Companion TIFFs that are missing on
/// disk are tolerated: planes that reference them read back as zero-filled.
pub struct CellWorxReader {
    htd_path: Option<PathBuf>,
    /// One [`ImageMetadata`] per series (`field_count * well_count`).
    series: Vec<ImageMetadata>,
    current_series: usize,
    /// `well_files[row][col]` = `Some(file list)` for selected wells.
    well_files: Vec<Vec<Option<Vec<PathBuf>>>>,
    /// Selected wells in row-major order; index = well index.
    selected_wells: Vec<(usize, usize)>,
    field_count: usize,
    n_wavelengths: usize,
    n_timepoints: u32,
    z_steps: u32,
    do_channels: bool,
    /// Microscope serial number parsed from the `Scanner SN` line of the plate
    /// `scan.log` file, if present.
    serial_number: Option<String>,
    /// Resolved `Z Map File` path parsed from the plate `scan.log`, if present.
    z_map_file: Option<PathBuf>,
    /// Set when the per-well file lists were resolved from a nested
    /// `TimePoint_<t>/ZStep_<z>/` directory walk (Java `getTiffFiles`
    /// `subdirectories` branch) rather than the flat `<plate><well>_..` naming.
    /// In that case `get_file` indexes the list by ZCT coordinate instead of by
    /// `field * imageCount + no`, mirroring `CellWorxReader.getFile`. Defaults to
    /// `false`, so the normal CellWorx/ScanR/Operetta path is unaffected.
    subdirectories: bool,
    tiff_reader: crate::tiff::TiffReader,
    tiff_loaded: bool,
}

impl CellWorxReader {
    pub fn new() -> Self {
        CellWorxReader {
            htd_path: None,
            series: Vec::new(),
            current_series: 0,
            well_files: Vec::new(),
            selected_wells: Vec::new(),
            field_count: 0,
            n_wavelengths: 0,
            n_timepoints: 1,
            z_steps: 1,
            do_channels: false,
            serial_number: None,
            z_map_file: None,
            subdirectories: false,
            tiff_reader: crate::tiff::TiffReader::new(),
            tiff_loaded: false,
        }
    }

    /// Microscope serial number parsed from the plate `scan.log` (`Scanner SN`),
    /// or `None` if the log was absent or did not contain the key. Mirrors the
    /// value Java stores via `setMicroscopeSerialNumber`.
    pub fn serial_number(&self) -> Option<&str> {
        self.serial_number.as_deref()
    }

    /// Resolved `Z Map File` companion path parsed from the plate `scan.log`, or
    /// `None`. Java appends this to `getSeriesUsedFiles`.
    pub fn z_map_file(&self) -> Option<&Path> {
        self.z_map_file.as_deref()
    }

    /// Resolve the .pnl/.tif file backing the given series + plane index,
    /// following `CellWorxReader.getFile`.
    fn get_file(&self, series: usize, no: u32) -> Option<PathBuf> {
        if self.field_count == 0 {
            return None;
        }
        let well_index = series / self.field_count;
        let field = series % self.field_count;
        let &(row, col) = self.selected_wells.get(well_index)?;
        let files = self.well_files.get(row)?.get(col)?.as_ref()?;
        if files.is_empty() {
            return None;
        }
        let image_count = files.len() / self.field_count.max(1);
        let idx = field * image_count + no as usize;
        if idx < files.len() {
            // Java getFile: when the per-well list came from the nested
            // TimePoint/ZStep walk (`subdirectories`), the files are ordered by
            // ZCT coordinate rather than `field * imageCount + no`, so index by
            // the rasterized (c, field, z, t) position. `get_dimension_order` is
            // always present here (series metadata is XYCZT), mirroring the
            // Java `getDimensionOrder() != null` guard.
            if self.subdirectories {
                let meta = self.series.get(series)?;
                let (z, c, t) = zct_coords(meta, no);
                let size_c = meta.size_c.max(1) as usize;
                let size_z = meta.size_z.max(1) as usize;
                let mut plane_index = c as usize;
                plane_index += size_c * field;
                plane_index += size_c * self.field_count * z as usize;
                plane_index += size_c * self.field_count * size_z * t as usize;
                return files.get(plane_index).cloned();
            }
            files.get(idx).cloned()
        } else if field < files.len() {
            files.get(field).cloned()
        } else if image_count == 0 && files.len() == 1 {
            files.first().cloned()
        } else {
            None
        }
    }

    /// Drive the standard well x field x T x Z series assembly, optionally with
    /// an externally-resolved per-well file list.
    ///
    /// This is the body of the former `set_id`, lifted verbatim except for the
    /// per-well file-list source: when `resolver` is `Some`, each selected
    /// well's list comes from the caller (Java's overridden `getTiffFiles`
    /// result flowing into `wellFiles[row][col]`) and `subdirectories` is set so
    /// `get_file` switches to ZCT-coordinate indexing; when `None`, the flat
    /// `build_well_files` naming is used exactly as before. Mirrors how
    /// `CellWorxReader.findPixelsFiles` calls the (overridable) `getTiffFiles`.
    fn set_id_impl(
        &mut self,
        path: &Path,
        mut resolver: Option<&mut dyn FnMut(usize, usize, &WellResolveDims) -> Vec<PathBuf>>,
    ) -> Result<()> {
        self.close()?;

        let htd = find_htd(path)?;
        let info = parse_htd(&htd)?;

        // Field (site) count = number of selected sites in the field map.
        let field_count = info
            .field_map
            .iter()
            .flatten()
            .filter(|&&b| b)
            .count()
            .max(1);

        // Enumerate selected wells in row-major order and build their file lists.
        let plate = plate_base(&htd);
        let channels = info.wavelengths.len();
        let dims = WellResolveDims {
            plate: plate.clone(),
            field_count,
            channels,
            n_timepoints: info.n_timepoints,
            z_steps: info.z_steps,
            do_channels: info.do_channels,
        };
        let mut well_files: Vec<Vec<Option<Vec<PathBuf>>>> =
            vec![vec![None; info.x_wells]; info.y_wells];
        let mut selected_wells: Vec<(usize, usize)> = Vec::new();
        for row in 0..info.y_wells {
            for col in 0..info.x_wells {
                if info.well_selected[row][col] {
                    let files = match resolver.as_mut() {
                        // Subclass-supplied list (e.g. MetaXpress nested-dir walk),
                        // mirroring the overridden getTiffFiles result.
                        Some(f) => f(row, col, &dims),
                        // Flat `<plate><well>_s_w_t.tif` naming (normal CellWorx).
                        None => build_well_files(
                            &plate,
                            row,
                            col,
                            field_count,
                            channels,
                            info.n_timepoints,
                            info.do_channels,
                        ),
                    };
                    well_files[row][col] = Some(files);
                    selected_wells.push((row, col));
                }
            }
        }

        let well_count = selected_wells.len();
        let series_count = field_count * well_count;
        if series_count == 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "CellWorX HTD declares no selected wells".into(),
            ));
        }

        // Store enough state for `get_file` so we can probe for a real TIFF.
        self.htd_path = Some(htd);
        self.well_files = well_files;
        self.selected_wells = selected_wells;
        self.field_count = field_count;
        self.n_wavelengths = channels;
        self.n_timepoints = info.n_timepoints;
        self.z_steps = info.z_steps;
        self.do_channels = info.do_channels;
        // ZCT-coordinate indexing in get_file only when a resolver supplied the
        // (nested-directory) lists; the flat path keeps the original behavior.
        self.subdirectories = resolver.is_some();

        // Find the first companion TIFF that actually exists on disk.
        let planes_per = (info.z_steps as usize) * (info.n_timepoints as usize) * channels;
        let mut series_idx = 0usize;
        let mut plane_idx = 0u32;
        let mut probe: Option<PathBuf> = None;
        loop {
            if let Some(f) = self.get_file(series_idx, plane_idx) {
                if f.exists() {
                    probe = Some(f);
                    break;
                }
            }
            if (plane_idx as usize) < planes_per {
                plane_idx += 1;
            } else if series_idx < series_count - 1 {
                plane_idx = 0;
                series_idx += 1;
            } else {
                break;
            }
        }
        let probe = probe.ok_or_else(|| {
            BioFormatsError::UnsupportedFormat(
                "CellWorX/MetaXpress: no companion pixel files found on disk".into(),
            )
        })?;

        self.tiff_reader.set_id(&probe)?;
        let tm = self.tiff_reader.metadata();
        let size_x = tm.size_x;
        let size_y = tm.size_y;
        let pixel_type = tm.pixel_type;
        let bits = tm.bits_per_pixel;
        let little_endian = tm.is_little_endian;
        let interleaved = tm.is_interleaved;
        let _ = self.tiff_reader.close();

        // Parse the plate-level scan.log for instrument scalars (Scanner SN,
        // Z Map File), following the head of Java populateMetadata. The plate
        // log is "<plate>scan.log" (plate_base already ends with '_').
        let plate_log = PathBuf::from(format!("{}scan.log", plate));
        let htd_path = self
            .htd_path
            .clone()
            .unwrap_or_else(|| PathBuf::from(&plate));
        let plate_info = parse_plate_log(&plate_log, &htd_path);
        self.serial_number = plate_info.serial_number.clone();
        self.z_map_file = plate_info.z_map_file.clone();

        let image_count = info.z_steps * channels as u32 * info.n_timepoints;
        let mut series = Vec::with_capacity(series_count);
        for s in 0..series_count {
            let (row, col) = self.selected_wells[s / field_count];
            let mut md = HashMap::new();
            md.insert(
                "format".into(),
                MetadataValue::String("MetaXpress/CellWorX".into()),
            );
            md.insert("Well".into(), MetadataValue::String(well_name(row, col)));
            for (i, w) in info.wavelengths.iter().enumerate() {
                if let Some(name) = w {
                    md.insert(
                        format!("Wavelength {}", i + 1),
                        MetadataValue::String(name.clone()),
                    );
                }
            }
            // Plate-wide instrument scalars (Java sets MicroscopeSerialNumber on
            // the single instrument; we surface it on each series' metadata).
            if let Some(sn) = &plate_info.serial_number {
                md.insert(
                    "Microscope Serial Number".into(),
                    MetadataValue::String(sn.clone()),
                );
            }
            if let Some(zmap) = &plate_info.z_map_file {
                md.insert(
                    "Z Map File".into(),
                    MetadataValue::String(zmap.to_string_lossy().into_owned()),
                );
            }
            // Per-well scan.log: capture every "key: value" line as series
            // metadata (Java parseWellLogFile -> addSeriesMeta). The log file is
            // "<plate><well>_scan.log".
            let well_log = PathBuf::from(format!("{}{}_scan.log", plate, well_name(row, col)));
            parse_well_log(&well_log, &mut md);
            series.push(ImageMetadata {
                size_x,
                size_y,
                size_z: info.z_steps,
                size_c: channels as u32,
                size_t: info.n_timepoints,
                pixel_type,
                bits_per_pixel: bits,
                image_count,
                dimension_order: DimensionOrder::XYCZT,
                is_rgb: false,
                is_interleaved: interleaved,
                is_indexed: false,
                is_little_endian: little_endian,
                resolution_count: 1,
                thumbnail: false,
                series_metadata: md,
                lookup_table: None,
                modulo_z: None,
                modulo_c: None,
                modulo_t: None,
            });
        }

        self.series = series;
        self.current_series = 0;
        self.tiff_loaded = false;
        Ok(())
    }

    /// Subclass hook: run the standard CellWorx well x field x T x Z series
    /// assembly from an externally-resolved per-well TIFF list.
    ///
    /// `resolver(row, col, dims)` returns the file list for the selected well at
    /// `(row, col)` (Java's overridden `getTiffFiles(plateName, rowLetter, col,
    /// channels, nTimepoints, zSteps)` result, which Java writes back into
    /// `wellFiles[row][col]`). The list is consumed by `get_file` using
    /// ZCT-coordinate indexing (the `subdirectories` branch of Java
    /// `CellWorxReader.getFile`).
    ///
    /// This is additive: it shares all assembly logic with the normal
    /// `set_id` path and changes nothing for callers that do not use it
    /// (CellWorx/ScanR/Operetta keep the flat-naming `None` path).
    pub(crate) fn set_id_with_resolver(
        &mut self,
        path: &Path,
        resolver: &mut dyn FnMut(usize, usize, &WellResolveDims) -> Vec<PathBuf>,
    ) -> Result<()> {
        self.set_id_impl(path, Some(resolver))
    }
}

impl Default for CellWorxReader {
    fn default() -> Self {
        Self::new()
    }
}

/// Dimensions a subclass-style file-list resolver needs, mirroring the
/// arguments Java `CellWorxReader.findPixelsFiles` passes to the (overridable)
/// `getTiffFiles(plateName, rowLetter, col, channels, nTimepoints, zSteps)`.
///
/// Exposed via [`CellWorxReader::set_id_with_resolver`] so a subclass such as
/// `MetaxpressTiffReader` can supply an externally-resolved per-well TIFF list
/// (e.g. from the nested `TimePoint_<t>/ZStep_<z>/` walk) while the standard
/// well x field x T x Z series assembly proceeds unchanged.
pub(crate) struct WellResolveDims {
    /// Plate-name prefix (HTD path minus extension, plus `_`).
    pub plate: String,
    /// Number of selected sites/fields. Part of the faithful Java
    /// `getTiffFiles(...)` argument set; the nested-dir resolver does not need
    /// it (it filters by name prefix), but a flat-naming resolver would.
    #[allow(dead_code)]
    pub field_count: usize,
    /// Number of wavelengths/channels (see `field_count`).
    #[allow(dead_code)]
    pub channels: usize,
    pub n_timepoints: u32,
    pub z_steps: u32,
    /// Java `doChannels` flag (see `field_count`).
    #[allow(dead_code)]
    pub do_channels: bool,
}

/// Parsed contents of a CellWorX / MetaXpress `.HTD` plate-index file.
struct HtdInfo {
    x_wells: usize,
    y_wells: usize,
    /// `well_selected[row][col]`
    well_selected: Vec<Vec<bool>>,
    /// field acquisition map (sites grid)
    field_map: Vec<Vec<bool>>,
    n_timepoints: u32,
    z_steps: u32,
    do_channels: bool,
    /// One entry per wavelength; `Some(name)` if a `WaveName<i>` was present.
    wavelengths: Vec<Option<String>>,
}

/// `Boolean.parseBoolean` semantics: true only when the token is "true".
fn htd_bool(value: &str) -> bool {
    value.trim().eq_ignore_ascii_case("true")
}

/// Parse a CellWorX `.HTD` file. Lines are `"key", value[, value...]`; the key
/// is delimited from the value by the literal `",` sequence (matching the Java
/// `line.indexOf("\",")` logic).
fn parse_htd(path: &Path) -> Result<HtdInfo> {
    let content = std::fs::read_to_string(path).map_err(BioFormatsError::Io)?;

    let mut x_wells = 0usize;
    let mut y_wells = 0usize;
    let mut well_selected: Vec<Vec<bool>> = Vec::new();
    let mut x_fields = 0usize;
    let mut y_fields = 0usize;
    let mut field_map: Option<Vec<Vec<bool>>> = None;
    let mut n_timepoints = 1u32;
    let mut z_steps = 1u32;
    let mut do_channels = false;
    let mut wavelengths: Vec<Option<String>> = Vec::new();

    for line in content.split('\n') {
        let split = match line.find("\",") {
            Some(s) if s >= 1 => s,
            _ => continue,
        };
        let key = line[1..split].trim();
        let value = line[split + 2..].trim();

        if key == "XWells" {
            x_wells = value.parse().unwrap_or(0);
        } else if key == "YWells" {
            y_wells = value.parse().unwrap_or(0);
            well_selected = vec![vec![false; x_wells]; y_wells];
        } else if let Some(rest) = key.strip_prefix("WellsSelection") {
            if let Ok(row1) = rest.trim().parse::<usize>() {
                if row1 >= 1 && row1 <= well_selected.len() {
                    let row = row1 - 1;
                    let mapping: Vec<&str> = value.split(',').collect();
                    for (col, slot) in well_selected[row].iter_mut().enumerate() {
                        if let Some(tok) = mapping.get(col) {
                            if htd_bool(tok) {
                                *slot = true;
                            }
                        }
                    }
                }
            }
        } else if key == "XSites" {
            x_fields = value.parse().unwrap_or(0);
        } else if key == "YSites" {
            y_fields = value.parse().unwrap_or(0);
            // If field acquisition was turned off ("Sites" == FALSE), the
            // single-site map is already set; don't overwrite it.
            if field_map.is_none() {
                field_map = Some(vec![vec![false; x_fields]; y_fields]);
            }
        } else if key == "Sites" {
            if value.eq_ignore_ascii_case("false") {
                field_map = Some(vec![vec![true]]);
            }
        } else if key == "TimePoints" {
            n_timepoints = value.parse().unwrap_or(1).max(1);
        } else if key == "ZSteps" {
            z_steps = value.parse().unwrap_or(1).max(1);
        } else if let Some(rest) = key.strip_prefix("SiteSelection") {
            if let (Ok(row1), Some(fm)) = (rest.trim().parse::<usize>(), field_map.as_mut()) {
                if row1 >= 1 && row1 <= fm.len() {
                    let row = row1 - 1;
                    let mapping: Vec<&str> = value.split(',').collect();
                    for (col, slot) in fm[row].iter_mut().enumerate() {
                        if let Some(tok) = mapping.get(col) {
                            *slot = htd_bool(tok);
                        }
                    }
                }
            }
        } else if key == "Waves" {
            do_channels = htd_bool(value);
        } else if key == "NWavelengths" {
            let n = value.parse().unwrap_or(0);
            wavelengths = vec![None; n];
        } else if let Some(rest) = key.strip_prefix("WaveName") {
            if let Ok(idx1) = rest.trim().parse::<usize>() {
                if idx1 >= 1 && idx1 <= wavelengths.len() {
                    wavelengths[idx1 - 1] = Some(value.replace('"', ""));
                }
            }
        }
    }

    let mut field_map = field_map.unwrap_or_else(|| vec![vec![true]]);
    // If the acquisition only contains one site, SiteSelection1 may be absent.
    // In that case, assume the field was selected.
    if x_fields == 1 && y_fields == 1 && !field_map.is_empty() && !field_map[0].is_empty() {
        field_map[0][0] = true;
    }
    if wavelengths.is_empty() {
        wavelengths.push(None);
    }

    Ok(HtdInfo {
        x_wells,
        y_wells,
        well_selected,
        field_map,
        n_timepoints,
        z_steps,
        do_channels,
        wavelengths,
    })
}

/// Locate the `.HTD` plate-index file given any member of the dataset.
fn find_htd(path: &Path) -> Result<PathBuf> {
    let is_htd = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("htd"))
        .unwrap_or(false);
    if is_htd {
        if path.exists() {
            return Ok(path.to_path_buf());
        }
        return Err(BioFormatsError::UnsupportedFormat(
            "CellWorX HTD file does not exist".into(),
        ));
    }
    // Derive from a pixel file: strip everything after the last '_'.
    let s = path.to_string_lossy();
    if let Some(us) = s.rfind('_') {
        for ext in ["HTD", "htd"] {
            let cand = PathBuf::from(format!("{}.{}", &s[..us], ext));
            if cand.exists() {
                return Ok(cand);
            }
        }
    }
    // Fall back to scanning the parent directory for any .htd file.
    if let Some(parent) = path.parent() {
        if let Ok(entries) = std::fs::read_dir(parent) {
            let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
            paths.sort();
            for p in paths {
                if p.extension()
                    .and_then(|x| x.to_str())
                    .map(|x| x.eq_ignore_ascii_case("htd"))
                    .unwrap_or(false)
                {
                    return Ok(p);
                }
            }
        }
    }
    Err(BioFormatsError::UnsupportedFormat(
        "CellWorX: could not locate companion .htd file".into(),
    ))
}

/// Build the plate-name prefix: the HTD path with its extension stripped, plus `_`.
fn plate_base(htd: &Path) -> String {
    let s = htd.to_string_lossy();
    let cut = s.rfind('.').unwrap_or(s.len());
    format!("{}_", &s[..cut])
}

/// Well label as used in MetaXpress TIFF names, e.g. row 0 col 0 -> "A01".
fn well_name(row: usize, col: usize) -> String {
    let letter = (b'A' + (row as u8 % 26)) as char;
    format!("{}{:02}", letter, col + 1)
}

/// Build the per-well TIFF file list, following
/// `MetaxpressTiffReader.getTiffFiles`. The list is ordered field, channel,
/// timepoint. The on-disk extension (`.tif` vs `.TIF`) is probed per file.
fn build_well_files(
    plate: &str,
    row: usize,
    col: usize,
    field_count: usize,
    channels: usize,
    n_timepoints: u32,
    do_channels: bool,
) -> Vec<PathBuf> {
    let base = format!("{}{}", plate, well_name(row, col));
    let mut files: Vec<PathBuf> =
        Vec::with_capacity(field_count * channels * n_timepoints as usize);
    for field in 0..field_count {
        for channel in 0..channels {
            for _t in 0..n_timepoints {
                let mut name = base.clone();
                if field_count > 1 {
                    name.push_str(&format!("_s{}", field + 1));
                }
                if do_channels || channels > 1 {
                    name.push_str(&format!("_w{}", channel + 1));
                }
                if n_timepoints > 1 {
                    // Matches the upstream quirk: the timepoint *count* is used.
                    name.push_str(&format!("_t{}", n_timepoints));
                }
                let lower = PathBuf::from(format!("{}.tif", name));
                if lower.exists() {
                    files.push(lower);
                } else {
                    files.push(PathBuf::from(format!("{}.TIF", name)));
                }
            }
        }
    }
    files
}

/// Scalars parsed from the plate-level `<plate>_scan.log` file, following the
/// instrument-metadata branch of Java `CellWorxReader.populateMetadata`.
struct PlateLogInfo {
    /// `Scanner SN` value (becomes the microscope serial number).
    serial_number: Option<String>,
    /// Resolved `Z Map File` path (relative segment resolved against the HTD's
    /// parent directory, matching the Java logic).
    z_map_file: Option<PathBuf>,
}

/// Parse the plate-level `scan.log` file for the `Scanner SN` and `Z Map File`
/// instrument scalars. Faithful port of the loop at the top of Java
/// `CellWorxReader.populateMetadata`. `htd` is the dataset id used to resolve a
/// relative `Z Map File` path against its parent directory.
fn parse_plate_log(plate_log: &Path, htd: &Path) -> PlateLogInfo {
    let mut serial_number = None;
    let mut z_map_file = None;

    if let Ok(content) = std::fs::read_to_string(plate_log) {
        for line in content.split('\n') {
            let trimmed = line.trim();
            if trimmed.starts_with("Z Map File") {
                // Java: substring after ':', then last path segment after '/'.
                if let Some(colon) = line.find(':') {
                    let after = &line[colon + 1..];
                    let segment = after.rsplit('/').next().unwrap_or(after).trim();
                    if !segment.is_empty() {
                        let parent = htd.parent().unwrap_or_else(|| Path::new(""));
                        z_map_file = Some(parent.join(segment));
                    }
                }
            } else if trimmed.starts_with("Scanner SN") {
                if let Some(colon) = line.find(':') {
                    let value = line[colon + 1..].trim();
                    if !value.is_empty() {
                        serial_number = Some(value.to_string());
                    }
                }
            }
        }
    }

    PlateLogInfo {
        serial_number,
        z_map_file,
    }
}

/// Parse a per-well `<well>_scan.log` file, capturing every `key: value` line as
/// series metadata. Faithful to the `addSeriesMeta(key, value)` call applied to
/// each colon-delimited line in Java `CellWorxReader.parseWellLogFile`.
fn parse_well_log(log_file: &Path, md: &mut HashMap<String, MetadataValue>) {
    let content = match std::fs::read_to_string(log_file) {
        Ok(c) => c,
        Err(_) => return,
    };
    for line in content.split('\n') {
        let line = line.trim();
        let separator = match line.find(':') {
            Some(s) => s,
            None => continue,
        };
        let key = line[..separator].trim();
        let value = line[separator + 1..].trim();
        if key.is_empty() {
            continue;
        }
        md.insert(key.to_string(), MetadataValue::String(value.to_string()));
    }
}

/// Z coordinate of a plane index under an `XYCZT` dimension order.
fn z_coord(meta: &ImageMetadata, no: u32) -> u32 {
    let sc = meta.size_c.max(1);
    let sz = meta.size_z.max(1);
    (no / sc) % sz
}

/// `(z, c, t)` coordinates of a plane index under an `XYCZT` dimension order
/// (matching the `int[] {z, c, t}` returned by Java `getZCTCoords`).
fn zct_coords(meta: &ImageMetadata, no: u32) -> (u32, u32, u32) {
    let sc = meta.size_c.max(1);
    let sz = meta.size_z.max(1);
    let c = no % sc;
    let z = (no / sc) % sz;
    let t = no / (sc * sz);
    (z, c, t)
}

impl FormatReader for CellWorxReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("htd") | Some("pnl"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        // Normal path: per-well file lists come from the flat `<plate><well>_..`
        // naming (`build_well_files`). No external resolver, no subdirectories.
        self.set_id_impl(path, None)
    }

    fn close(&mut self) -> Result<()> {
        self.htd_path = None;
        self.series.clear();
        self.current_series = 0;
        self.well_files.clear();
        self.selected_wells.clear();
        self.field_count = 0;
        self.n_wavelengths = 0;
        self.n_timepoints = 1;
        self.z_steps = 1;
        self.do_channels = false;
        self.serial_number = None;
        self.z_map_file = None;
        self.subdirectories = false;
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
        let (plane_bytes, size_z) = {
            let meta = self
                .series
                .get(self.current_series)
                .ok_or(BioFormatsError::NotInitialized)?;
            if plane_index >= meta.image_count {
                return Err(BioFormatsError::PlaneOutOfRange(plane_index));
            }
            let bps = meta.pixel_type.bytes_per_sample();
            (
                meta.size_x as usize * meta.size_y as usize * bps,
                meta.size_z,
            )
        };

        // Resolve the backing file; a missing companion reads back as zeros.
        let file = match self.get_file(self.current_series, plane_index) {
            Some(f) if f.exists() => f,
            _ => return Ok(vec![0u8; plane_bytes]),
        };

        if self.tiff_loaded {
            let _ = self.tiff_reader.close();
            self.tiff_loaded = false;
        }
        if self.tiff_reader.set_id(&file).is_err() {
            return Ok(vec![0u8; plane_bytes]);
        }
        self.tiff_loaded = true;

        let tiff_series = self.tiff_reader.series_count();
        let tiff_imgs = self.tiff_reader.metadata().image_count;
        let plane = if tiff_series == self.field_count && self.field_count > 1 {
            let field = self.current_series % self.field_count;
            let _ = self.tiff_reader.set_series(field);
            plane_index
        } else if tiff_imgs == size_z {
            let meta = &self.series[self.current_series];
            z_coord(meta, plane_index)
        } else {
            0
        };
        self.tiff_reader.open_bytes(plane)
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
        crop_full_plane("CellWorX", &full, meta, 1, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        self.open_bytes(plane_index)
    }
}

// ── AliconaReader ────────────────────────────────────────────────────────────────

const AL3D_MAGIC: &[u8] = b"AL3D";
const AL3D_DATA_OFFSET: u64 = 512;

pub struct AliconaReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl AliconaReader {
    pub fn new() -> Self {
        AliconaReader {
            path: None,
            meta: None,
        }
    }
}

impl Default for AliconaReader {
    fn default() -> Self {
        Self::new()
    }
}

fn parse_al3d(path: &Path) -> Result<ImageMetadata> {
    let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
    if data.len() < AL3D_DATA_OFFSET as usize {
        return Err(BioFormatsError::UnsupportedFormat(
            "AL3D file too short for declared header offset".into(),
        ));
    }
    if &data[..4] != AL3D_MAGIC {
        return Err(BioFormatsError::UnsupportedFormat(
            "AL3D file is missing AL3D magic".into(),
        ));
    }
    // Offset 8: width (u32 LE), 12: height (u32 LE), 16: depth (u32 LE)
    let width = u32::from_le_bytes([data[8], data[9], data[10], data[11]]);
    let height = u32::from_le_bytes([data[12], data[13], data[14], data[15]]);
    let depth = u32::from_le_bytes([data[16], data[17], data[18], data[19]]);
    if width == 0 || height == 0 || depth == 0 {
        return Err(BioFormatsError::UnsupportedFormat(
            "AL3D file has zero image dimensions".into(),
        ));
    }
    // Offset 20: data_type (u16 LE)
    let data_type = u16::from_le_bytes([data[20], data[21]]);
    let pixel_type = match data_type {
        0 => PixelType::Uint8,
        1 => PixelType::Uint16,
        2 => PixelType::Float32,
        other => {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "AL3D data type {other} is not supported"
            )));
        }
    };
    let meta = simple_meta(width, height, depth, pixel_type);
    let required_len = AL3D_DATA_OFFSET
        .checked_add(checked_payload_len(&meta)?)
        .ok_or_else(|| BioFormatsError::Format("AL3D file size overflows".into()))?;
    if (data.len() as u64) < required_len {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "AL3D pixel payload is shorter than declared ({} < {required_len})",
            data.len()
        )));
    }
    Ok(meta)
}

impl FormatReader for AliconaReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("al3d"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.len() >= 4 && header[0..4] == *AL3D_MAGIC
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let meta = parse_al3d(path)?;
        self.path = Some(path.to_path_buf());
        self.meta = Some(meta);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        Ok(())
    }

    fn series_count(&self) -> usize {
        usize::from(self.meta.is_some())
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.meta.is_none() {
            Err(BioFormatsError::NotInitialized)
        } else if s == 0 {
            Ok(())
        } else {
            Err(BioFormatsError::SeriesOutOfRange(s))
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

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let bps = meta.pixel_type.bytes_per_sample();
        let plane_bytes = meta.size_x as usize * meta.size_y as usize * bps;
        let plane_offset = AL3D_DATA_OFFSET + plane_index as u64 * plane_bytes as u64;
        let path = self
            .path
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?
            .clone();
        let mut f = std::fs::File::open(&path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(plane_offset))
            .map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; plane_bytes];
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
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        crop_full_plane("AL3D", &full, meta, 1, x, y, w, h)
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

// ── FeiSerReader ──────────────────────────────────────────────────────────────

/// FEI SER format: electron-microscopy image series from TEM/STEM systems.
/// Magic: bytes 0-1 == 0x97 0x01 (series file signature).
pub struct FeiSerReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    data_offsets: Vec<u64>,
}

impl FeiSerReader {
    pub fn new() -> Self {
        FeiSerReader {
            path: None,
            meta: None,
            data_offsets: Vec::new(),
        }
    }
}

impl Default for FeiSerReader {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
struct SerParseResult {
    meta: ImageMetadata,
    data_offsets: Vec<u64>,
}

const SER_MAGIC: u16 = 0x0197;
const SER_2D_IMAGE_DATA_TYPE: u32 = 0x4122;
const SER_LONG_OFFSET_VERSION: u16 = 0x0220;
const SER_2D_ELEMENT_HEADER_LEN: u64 = 50;

fn read_u16_le(data: &[u8], offset: usize, label: &str) -> Result<u16> {
    let bytes = data.get(offset..offset + 2).ok_or_else(|| {
        BioFormatsError::UnsupportedFormat(format!("FEI SER header is too short for {label}"))
    })?;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn read_u32_le(data: &[u8], offset: usize, label: &str) -> Result<u32> {
    let bytes = data.get(offset..offset + 4).ok_or_else(|| {
        BioFormatsError::UnsupportedFormat(format!("FEI SER header is too short for {label}"))
    })?;
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn read_u64_le(data: &[u8], offset: usize, label: &str) -> Result<u64> {
    let bytes = data.get(offset..offset + 8).ok_or_else(|| {
        BioFormatsError::UnsupportedFormat(format!("FEI SER header is too short for {label}"))
    })?;
    Ok(u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ]))
}

fn ser_pixel_type(dtype: u16) -> Result<PixelType> {
    match dtype {
        1 => Ok(PixelType::Uint8),
        2 => Ok(PixelType::Uint16),
        3 => Ok(PixelType::Uint32),
        4 => Ok(PixelType::Int8),
        5 => Ok(PixelType::Int16),
        6 => Ok(PixelType::Int32),
        7 => Ok(PixelType::Float32),
        8 => Ok(PixelType::Float64),
        _ => Err(BioFormatsError::UnsupportedFormat(format!(
            "FEI SER unsupported element pixel type {dtype}"
        ))),
    }
}

fn parse_ser_element_header(data: &[u8], offset: u64) -> Result<(u32, u32, PixelType, u64)> {
    let offset_usize = usize::try_from(offset)
        .map_err(|_| BioFormatsError::Format("FEI SER element offset overflows".into()))?;
    let end = offset
        .checked_add(SER_2D_ELEMENT_HEADER_LEN)
        .ok_or_else(|| BioFormatsError::Format("FEI SER element header offset overflows".into()))?;
    if end > data.len() as u64 {
        return Err(BioFormatsError::UnsupportedFormat(
            "FEI SER image element header is shorter than declared".into(),
        ));
    }
    let dtype = read_u16_le(data, offset_usize + 40, "element pixel type")?;
    let width = read_u32_le(data, offset_usize + 42, "element width")?;
    let height = read_u32_le(data, offset_usize + 46, "element height")?;
    if width == 0 || height == 0 {
        return Err(BioFormatsError::UnsupportedFormat(
            "FEI SER image element has zero image dimensions".into(),
        ));
    }
    Ok((width, height, ser_pixel_type(dtype)?, end))
}

fn parse_ser(path: &Path) -> Result<SerParseResult> {
    let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
    if data.len() < 28 {
        return Err(BioFormatsError::UnsupportedFormat(
            "FEI SER header is too short for safe image decoding".to_string(),
        ));
    }
    let series_id = read_u16_le(&data, 0, "series id")?;
    if series_id != SER_MAGIC {
        return Err(BioFormatsError::UnsupportedFormat(
            "FEI SER header is missing 0x0197 magic".into(),
        ));
    }
    let version = read_u16_le(&data, 2, "series version")?;
    let data_type_id = read_u32_le(&data, 4, "data type id")?;
    if data_type_id != SER_2D_IMAGE_DATA_TYPE {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "FEI SER only supports 2D image data elements, found type 0x{data_type_id:04x}"
        )));
    }
    let tag_type_id = read_u32_le(&data, 8, "tag type id")?;
    let total = read_u32_le(&data, 12, "total element count")?;
    let valid = read_u32_le(&data, 16, "valid element count")?;
    if total == 0 || valid == 0 || valid > total {
        return Err(BioFormatsError::UnsupportedFormat(
            "FEI SER header has invalid element counts".into(),
        ));
    }

    let (offset_array_offset, number_dimensions_offset) = if version >= SER_LONG_OFFSET_VERSION {
        (read_u64_le(&data, 20, "offset array offset")?, 28usize)
    } else {
        (
            read_u32_le(&data, 20, "offset array offset")? as u64,
            24usize,
        )
    };
    let number_dimensions = read_u32_le(&data, number_dimensions_offset, "dimension count")?;
    if number_dimensions > 16 {
        return Err(BioFormatsError::UnsupportedFormat(
            "FEI SER header has implausible dimension count".into(),
        ));
    }
    if offset_array_offset == 0 || offset_array_offset >= data.len() as u64 {
        return Err(BioFormatsError::UnsupportedFormat(
            "FEI SER offset array is missing or outside the file".into(),
        ));
    }

    let offset_size = if version >= SER_LONG_OFFSET_VERSION {
        8u64
    } else {
        4u64
    };
    let offset_array_bytes = (valid as u64)
        .checked_mul(offset_size)
        .ok_or_else(|| BioFormatsError::Format("FEI SER offset array size overflows".into()))?;
    let offset_array_end = offset_array_offset
        .checked_add(offset_array_bytes)
        .ok_or_else(|| BioFormatsError::Format("FEI SER offset array end overflows".into()))?;
    if offset_array_end > data.len() as u64 {
        return Err(BioFormatsError::UnsupportedFormat(
            "FEI SER offset array is shorter than declared".into(),
        ));
    }

    let mut data_offsets = Vec::with_capacity(valid as usize);
    let base = usize::try_from(offset_array_offset)
        .map_err(|_| BioFormatsError::Format("FEI SER offset array offset overflows".into()))?;
    for i in 0..valid as usize {
        let entry_offset = base + i * offset_size as usize;
        let element_offset = if offset_size == 8 {
            read_u64_le(&data, entry_offset, "element offset")?
        } else {
            read_u32_le(&data, entry_offset, "element offset")? as u64
        };
        if element_offset == 0 || element_offset >= data.len() as u64 {
            return Err(BioFormatsError::UnsupportedFormat(
                "FEI SER image element offset is missing or outside the file".into(),
            ));
        }
        data_offsets.push(element_offset);
    }

    let (width, height, pixel_type, first_payload_offset) =
        parse_ser_element_header(&data, data_offsets[0])?;
    let plane_bytes = (width as u64)
        .checked_mul(height as u64)
        .and_then(|n| n.checked_mul(pixel_type.bytes_per_sample() as u64))
        .ok_or_else(|| BioFormatsError::Format("FEI SER plane size overflows".into()))?;
    let first_payload_end = first_payload_offset
        .checked_add(plane_bytes)
        .ok_or_else(|| BioFormatsError::Format("FEI SER payload end overflows".into()))?;
    if first_payload_end > data.len() as u64 {
        return Err(BioFormatsError::UnsupportedFormat(
            "FEI SER image payload is shorter than declared".into(),
        ));
    }
    for &offset in data_offsets.iter().skip(1) {
        let (frame_w, frame_h, frame_pixel_type, payload_offset) =
            parse_ser_element_header(&data, offset)?;
        if frame_w != width || frame_h != height || frame_pixel_type != pixel_type {
            return Err(BioFormatsError::UnsupportedFormat(
                "FEI SER mixed image element dimensions or pixel types are not supported".into(),
            ));
        }
        let payload_end = payload_offset
            .checked_add(plane_bytes)
            .ok_or_else(|| BioFormatsError::Format("FEI SER payload end overflows".into()))?;
        if payload_end > data.len() as u64 {
            return Err(BioFormatsError::UnsupportedFormat(
                "FEI SER image payload is shorter than declared".into(),
            ));
        }
    }

    let mut meta = simple_meta(width, height, valid, pixel_type);
    meta.series_metadata.insert(
        "format".to_string(),
        MetadataValue::String("FEI SER".to_string()),
    );
    meta.series_metadata.insert(
        "ser_version".to_string(),
        MetadataValue::Int(version as i64),
    );
    meta.series_metadata.insert(
        "ser_tag_type_id".to_string(),
        MetadataValue::Int(tag_type_id as i64),
    );
    meta.series_metadata.insert(
        "ser_number_dimensions".to_string(),
        MetadataValue::Int(number_dimensions as i64),
    );
    Ok(SerParseResult { meta, data_offsets })
}

impl FormatReader for FeiSerReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("ser"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.len() >= 2 && header[0] == 0x97 && header[1] == 0x01
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let parsed = parse_ser(path)?;
        self.path = Some(path.to_path_buf());
        self.meta = Some(parsed.meta);
        self.data_offsets = parsed.data_offsets;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.data_offsets.clear();
        Ok(())
    }
    fn series_count(&self) -> usize {
        if self.meta.is_some() {
            1
        } else {
            0
        }
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.meta.is_some() && s == 0 {
            Ok(())
        } else {
            Err(BioFormatsError::SeriesOutOfRange(s))
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

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let path = self
            .path
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?
            .clone();
        let offset = *self
            .data_offsets
            .get(plane_index as usize)
            .ok_or(BioFormatsError::PlaneOutOfRange(plane_index))?;
        let payload_offset = offset
            .checked_add(SER_2D_ELEMENT_HEADER_LEN)
            .ok_or_else(|| BioFormatsError::Format("FEI SER payload offset overflows".into()))?;
        let plane_bytes =
            meta.size_x as usize * meta.size_y as usize * meta.pixel_type.bytes_per_sample();
        let mut f = std::fs::File::open(&path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(payload_offset))
            .map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; plane_bytes];
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
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        crop_full_plane("FEI SER", &full, meta, 1, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        self.open_bytes(plane_index)
    }
}

// ── OxfordInstrumentsReader ───────────────────────────────────────────────────

const OXFORD_DATA_OFFSET: u64 = 128;

pub struct OxfordInstrumentsReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl OxfordInstrumentsReader {
    pub fn new() -> Self {
        OxfordInstrumentsReader {
            path: None,
            meta: None,
        }
    }
}

impl Default for OxfordInstrumentsReader {
    fn default() -> Self {
        Self::new()
    }
}

fn parse_oxford(path: &Path) -> Result<ImageMetadata> {
    let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
    if data.len() < 12 {
        return Err(BioFormatsError::UnsupportedFormat(
            "Oxford TOP header is too short for safe image decoding".to_string(),
        ));
    }
    // Offset 4: width (u16 LE), 6: height (u16 LE), 8: data_type (u16 LE)
    let width = u16::from_le_bytes([data[4], data[5]]) as u32;
    let height = u16::from_le_bytes([data[6], data[7]]) as u32;
    let dtype = u16::from_le_bytes([data[8], data[9]]);
    let pixel_type = match dtype {
        0 => PixelType::Uint8,
        1 => PixelType::Uint16,
        2 => PixelType::Float32,
        other => {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "Oxford TOP data type {other} is not supported"
            )));
        }
    };
    if width == 0 || height == 0 {
        return Err(BioFormatsError::UnsupportedFormat(
            "Oxford TOP header is missing image dimensions".to_string(),
        ));
    }
    let meta = simple_meta(width, height, 1, pixel_type);
    let required_len = OXFORD_DATA_OFFSET
        .checked_add(checked_payload_len(&meta)?)
        .ok_or_else(|| BioFormatsError::Format("Oxford TOP file size overflows".into()))?;
    if (data.len() as u64) < required_len {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "Oxford TOP pixel payload is shorter than declared ({} < {required_len})",
            data.len()
        )));
    }
    Ok(meta)
}

impl FormatReader for OxfordInstrumentsReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("top"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let meta = parse_oxford(path)?;
        self.path = Some(path.to_path_buf());
        self.meta = Some(meta);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        Ok(())
    }

    fn series_count(&self) -> usize {
        usize::from(self.meta.is_some())
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if s == 0 && self.meta.is_some() {
            Ok(())
        } else {
            Err(BioFormatsError::SeriesOutOfRange(s))
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

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index != 0 {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let bps = meta.pixel_type.bytes_per_sample();
        let plane_bytes = meta.size_x as usize * meta.size_y as usize * bps;
        let path = self
            .path
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?
            .clone();
        let mut f = std::fs::File::open(&path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(OXFORD_DATA_OFFSET))
            .map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; plane_bytes];
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
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        crop_full_plane("Oxford Instruments", &full, meta, 1, x, y, w, h)
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

// ── MIASReader ────────────────────────────────────────────────────────────────
//
// MIAS (Maia Scientific) HCS reader, ported from the upstream Java MIASReader.
// A dataset is a directory hierarchy:
//
//   <experiment>/<plate>/Well<xxxx>/mode<c>_z<zzz>_t<ttt>_im<r>_<col>.tif
//
// Each TIFF contains a single grayscale plane.  The "mode" block is the
// channel, "z"/"t" are the Z section and timepoint, and "im<r>_<col>" gives the
// tile coordinates within a mosaic.  One series is produced per well.
//
// This implementation handles the common (non-tiled, single-plane-per-file)
// case faithfully; tiled mosaics fall back to reading the first tile.

/// Per-well TIFF planes plus the parsed dimension structure.
struct MiasWell {
    /// Sorted TIFF file paths (one plane each).
    tiffs: Vec<PathBuf>,
    size_z: u32,
    size_c: u32,
    size_t: u32,
    well_number: i64,
}

pub struct MiasReader {
    wells: Vec<MiasWell>,
    series: Vec<ImageMetadata>,
    current_series: usize,
    tile_rows: u32,
    tile_cols: u32,
    tiff_reader: crate::tiff::TiffReader,
    tiff_loaded: bool,
}

impl MiasReader {
    pub fn new() -> Self {
        MiasReader {
            wells: Vec::new(),
            series: Vec::new(),
            current_series: 0,
            tile_rows: 1,
            tile_cols: 1,
            tiff_reader: crate::tiff::TiffReader::new(),
            tiff_loaded: false,
        }
    }
}

impl Default for MiasReader {
    fn default() -> Self {
        Self::new()
    }
}

fn is_mias_tiff(name: &str) -> bool {
    let l = name.to_ascii_lowercase();
    l.ends_with(".tif") || l.ends_with(".tiff")
}

/// Extract the integer following a `<prefix>` block in a MIAS filename, e.g.
/// `mode2_z003_t001_...` -> for prefix "z" returns Some(3).
fn mias_block(name: &str, prefix: &str) -> Option<i64> {
    let lname = name.to_ascii_lowercase();
    for part in lname.split('_') {
        if let Some(rest) = part.strip_prefix(prefix) {
            if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
                return rest.parse::<i64>().ok();
            }
        }
    }
    None
}

/// Extract the trailing tile-column index from a MIAS tile filename, e.g.
/// `mode2_z003_t001_im0_2.tif` -> the bare `2` block after `im<r>_` -> Some(2).
/// In the MIAS convention the last underscore-separated block before the
/// extension is the tile column (a bare integer with no alphabetic prefix).
fn mias_trailing_col(name: &str) -> Option<i64> {
    // Strip extension.
    let stem = name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name);
    let last = stem.rsplit('_').next()?;
    if !last.is_empty() && last.chars().all(|c| c.is_ascii_digit()) {
        last.parse::<i64>().ok()
    } else {
        None
    }
}

/// Identify whether a directory name is a MIAS well directory.
fn is_well_dir_name(name: &str) -> bool {
    if name.starts_with("Well") {
        return true;
    }
    // Four-digit well directory in the alternate layout.
    name.len() == 4 && name.chars().all(|c| c.is_ascii_digit())
}

fn well_number_from_name(name: &str) -> i64 {
    let stripped = name.trim_start_matches("Well");
    stripped.trim().parse::<i64>().map(|v| v - 1).unwrap_or(0)
}

impl MiasReader {
    /// Locate the plate directory and enumerate well directories given a TIFF
    /// (or well directory) path inside a MIAS hierarchy.
    fn build(&mut self, id: &Path) -> Result<()> {
        let base = id.canonicalize().unwrap_or_else(|_| id.to_path_buf());

        // The well directory is the parent of a TIFF, or `id` itself when a
        // directory is given.  The plate directory is the parent of the well.
        let well_dir = if base.is_dir() {
            base.clone()
        } else {
            base.parent()
                .map(|p| p.to_path_buf())
                .unwrap_or(base.clone())
        };
        let plate_dir = well_dir
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or(well_dir.clone());

        // Enumerate well directories under the plate.
        let mut well_dirs: Vec<PathBuf> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&plate_dir) {
            let mut names: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
            names.sort();
            for p in names {
                if p.is_dir() {
                    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    if is_well_dir_name(name) && dir_has_tiff_or_subdir(&p) {
                        well_dirs.push(p);
                    }
                }
            }
        }
        // Fallback: treat the single given well directory as the only well.
        if well_dirs.is_empty() {
            well_dirs.push(well_dir.clone());
        }

        let mut wells = Vec::new();
        for wd in &well_dirs {
            let mut tiffs = collect_well_tiffs(wd);
            tiffs.sort();
            if tiffs.is_empty() {
                continue;
            }

            // Determine the dimension counts from distinct block values.
            let mut z_vals: Vec<i64> = Vec::new();
            let mut t_vals: Vec<i64> = Vec::new();
            let mut c_vals: Vec<i64> = Vec::new();
            let mut im_rows: Vec<i64> = Vec::new();
            let mut im_cols: Vec<i64> = Vec::new();
            for t in &tiffs {
                let name = t.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if let Some(z) = mias_block(name, "z") {
                    if !z_vals.contains(&z) {
                        z_vals.push(z);
                    }
                }
                if let Some(tt) = mias_block(name, "t") {
                    if !t_vals.contains(&tt) {
                        t_vals.push(tt);
                    }
                }
                if let Some(c) = mias_block(name, "mode") {
                    if !c_vals.contains(&c) {
                        c_vals.push(c);
                    }
                }
                if let Some(im) = mias_block(name, "im") {
                    if !im_rows.contains(&im) {
                        im_rows.push(im);
                    }
                    // The tile column is the trailing bare-integer block; it is
                    // only meaningful for tiled mosaics (those with an "im" row
                    // block), per MIASReader's FilePattern handling.
                    if let Some(col) = mias_trailing_col(name) {
                        if !im_cols.contains(&col) {
                            im_cols.push(col);
                        }
                    }
                }
            }
            let size_z = (z_vals.len() as u32).max(1);
            let size_t = (t_vals.len() as u32).max(1);
            let size_c = (c_vals.len() as u32).max(1);
            if im_rows.len() as u32 > self.tile_rows {
                self.tile_rows = im_rows.len() as u32;
            }
            if im_cols.len() as u32 > self.tile_cols {
                self.tile_cols = im_cols.len() as u32;
            }

            let name = wd.file_name().and_then(|n| n.to_str()).unwrap_or("");
            wells.push(MiasWell {
                tiffs,
                size_z,
                size_c,
                size_t,
                well_number: well_number_from_name(name),
            });
        }

        if wells.is_empty() {
            return Err(BioFormatsError::UnsupportedFormat(
                "MIAS: no TIFF files found in any well directory".into(),
            ));
        }

        if self.tile_cols == 0 {
            self.tile_cols = 1;
        }
        if self.tile_rows == 0 {
            self.tile_rows = 1;
        }

        // Probe the first TIFF for pixel parameters (assume uniform).
        self.tiff_reader.set_id(&wells[0].tiffs[0])?;
        let tm = self.tiff_reader.metadata();
        let tile_w = tm.size_x;
        let tile_h = tm.size_y;
        let pixel_type = tm.pixel_type;
        let bits = tm.bits_per_pixel;
        let little_endian = tm.is_little_endian;
        let tiff_c = tm.size_c.max(1);
        let is_rgb = tm.is_rgb;
        let _ = self.tiff_reader.close();

        for w in &wells {
            let logical_planes = w
                .size_z
                .checked_mul(w.size_t)
                .and_then(|n| n.checked_mul(w.size_c))
                .ok_or_else(|| BioFormatsError::Format("MIAS: image count overflows".into()))?;
            let expected_tiffs = logical_planes
                .checked_mul(self.tile_rows.max(1))
                .and_then(|n| n.checked_mul(self.tile_cols.max(1)))
                .ok_or_else(|| BioFormatsError::Format("MIAS: TIFF count overflows".into()))?;
            if w.tiffs.len() != expected_tiffs as usize {
                return Err(BioFormatsError::Format(format!(
                    "MIAS: well {} references {} TIFF file(s), expected {expected_tiffs}",
                    w.well_number,
                    w.tiffs.len()
                )));
            }
            for tiff in &w.tiffs {
                self.tiff_reader.set_id(tiff)?;
                let tm = self.tiff_reader.metadata();
                let (size_x, size_y, this_pixel_type, this_bits, pages) = (
                    tm.size_x,
                    tm.size_y,
                    tm.pixel_type,
                    tm.bits_per_pixel,
                    tm.image_count.max(1),
                );
                let _ = self.tiff_reader.close();
                if size_x != tile_w || size_y != tile_h {
                    return Err(BioFormatsError::Format(format!(
                        "MIAS: companion TIFF {} has dimensions {}x{}, expected {tile_w}x{tile_h}",
                        tiff.display(),
                        size_x,
                        size_y
                    )));
                }
                if this_pixel_type != pixel_type || this_bits != bits {
                    return Err(BioFormatsError::Format(format!(
                        "MIAS: companion TIFF {} has inconsistent pixel type",
                        tiff.display()
                    )));
                }
                if pages != 1 {
                    return Err(BioFormatsError::Format(format!(
                        "MIAS: companion TIFF {} has {} page(s), expected 1",
                        tiff.display(),
                        pages
                    )));
                }
            }
        }

        let mut series = Vec::with_capacity(wells.len());
        for w in &wells {
            let size_c = w.size_c * tiff_c;
            let mut meta_map = HashMap::new();
            meta_map.insert(
                "format".to_string(),
                crate::common::metadata::MetadataValue::String("MIAS".into()),
            );
            meta_map.insert(
                "well_number".to_string(),
                crate::common::metadata::MetadataValue::Int(w.well_number),
            );
            let image_count = (w.size_z * w.size_t * w.size_c).max(1);
            let size_x = tile_w
                .checked_mul(self.tile_cols)
                .ok_or_else(|| BioFormatsError::Format("MIAS: mosaic width overflows".into()))?;
            let size_y = tile_h
                .checked_mul(self.tile_rows)
                .ok_or_else(|| BioFormatsError::Format("MIAS: mosaic height overflows".into()))?;
            series.push(ImageMetadata {
                size_x,
                size_y,
                size_z: w.size_z,
                size_c,
                size_t: w.size_t,
                pixel_type,
                bits_per_pixel: bits,
                image_count,
                dimension_order: DimensionOrder::XYZCT,
                is_rgb,
                is_interleaved: false,
                is_indexed: false,
                is_little_endian: little_endian,
                resolution_count: 1,
                thumbnail: false,
                series_metadata: meta_map,
                lookup_table: None,
                modulo_z: None,
                modulo_c: None,
                modulo_t: None,
            });
        }

        self.wells = wells;
        self.series = series;
        self.current_series = 0;
        Ok(())
    }
}

fn dir_has_tiff_or_subdir(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries.flatten().any(|e| {
                let p = e.path();
                p.is_dir()
                    || p.file_name()
                        .and_then(|n| n.to_str())
                        .map(is_mias_tiff)
                        .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

/// Collect TIFFs from a well directory; if none are present, descend into
/// single-character channel subdirectories (the alternate MIAS layout).
fn collect_well_tiffs(well_dir: &Path) -> Vec<PathBuf> {
    let mut tiffs = Vec::new();
    if let Ok(entries) = std::fs::read_dir(well_dir) {
        let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        paths.sort();
        for p in &paths {
            if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                if is_mias_tiff(name) {
                    tiffs.push(p.clone());
                }
            }
        }
        if tiffs.is_empty() {
            for p in &paths {
                if p.is_dir() {
                    if let Ok(sub) = std::fs::read_dir(p) {
                        let mut subpaths: Vec<PathBuf> = sub.flatten().map(|e| e.path()).collect();
                        subpaths.sort();
                        for sp in subpaths {
                            if let Some(name) = sp.file_name().and_then(|n| n.to_str()) {
                                if is_mias_tiff(name) {
                                    tiffs.push(sp);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    tiffs
}

impl FormatReader for MiasReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        // A MIAS TIFF lives in a Well<xxxx> directory and uses the
        // mode/z/t naming convention.
        if !path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("tif") || e.eq_ignore_ascii_case("tiff"))
            .unwrap_or(false)
        {
            return false;
        }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let in_well_dir = path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .map(is_well_dir_name)
            .unwrap_or(false);
        in_well_dir && (mias_block(name, "mode").is_some() || mias_block(name, "z").is_some())
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        // Robustly reject any .tif/.tiff that is not a genuine MIAS dataset so
        // that plain TIFFs fall through to the generic TiffReader. A real MIAS
        // file lives in a Well<xxxx> directory and uses the mode/z/t naming
        // convention (the same guard the registry uses before the TIFF magic
        // pass). Directory inputs (a well/plate dir) are allowed through.
        if !path.is_dir() && !self.is_this_type_by_name(path) {
            return Err(BioFormatsError::UnsupportedFormat(
                "MIAS: file is not a Well<xxxx>/mode<c>_z<zzz>_t<ttt> TIFF dataset".into(),
            ));
        }
        self.tile_rows = 1;
        self.tile_cols = 1;
        self.build(path)?;
        self.tiff_loaded = false;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.wells.clear();
        self.series.clear();
        self.current_series = 0;
        self.tile_rows = 1;
        self.tile_cols = 1;
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
            return Err(BioFormatsError::NotInitialized);
        }
        if s >= self.series_count() {
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
        let tile_rows = self.tile_rows.max(1);
        let tile_cols = self.tile_cols.max(1);

        // Non-tiled case: plane index maps directly to tiffs[series][no].
        if tile_rows == 1 && tile_cols == 1 {
            let well = self
                .wells
                .get(self.current_series)
                .ok_or(BioFormatsError::NotInitialized)?;
            let tiff_path = well
                .tiffs
                .get(plane_index as usize)
                .ok_or(BioFormatsError::PlaneOutOfRange(plane_index))?
                .clone();
            if self.tiff_loaded {
                let _ = self.tiff_reader.close();
            }
            self.tiff_reader.set_id(&tiff_path)?;
            self.tiff_loaded = true;
            return self.tiff_reader.open_bytes(0);
        }

        // Tiled mosaic: assemble all tiles of this plane into the full plane.
        // Tile (row, col) is the TIFF at index (no*tileRows + row)*tileCols + col
        // and is placed at output position (col*tileWidth, row*tileHeight),
        // matching MIASReader.openBytes / getTile.
        let full_w = meta.size_x as usize;
        let full_h = meta.size_y as usize;
        let bps = meta.pixel_type.bytes_per_sample();
        let rgb = meta.is_rgb;
        let samples = if rgb { meta.size_c.max(1) as usize } else { 1 };
        // bytes per output (full) row across all samples for the non-interleaved
        // layout used by the underlying TIFF reader is handled per-tile below.
        let mut out = vec![0u8; full_w * full_h * bps * samples];
        let out_row_len = full_w * bps * samples;

        for row in 0..tile_rows {
            for col in 0..tile_cols {
                let tile_index = ((plane_index * tile_rows + row) * tile_cols + col) as usize;
                let tiff_path = {
                    let well = self
                        .wells
                        .get(self.current_series)
                        .ok_or(BioFormatsError::NotInitialized)?;
                    match well.tiffs.get(tile_index) {
                        Some(p) => p.clone(),
                        None => continue, // missing tile -> leave zero-filled
                    }
                };
                if self.tiff_loaded {
                    let _ = self.tiff_reader.close();
                }
                self.tiff_reader.set_id(&tiff_path)?;
                self.tiff_loaded = true;
                let tile = self.tiff_reader.open_bytes(0)?;

                let tm = self.tiff_reader.metadata();
                let tile_w = tm.size_x as usize;
                let tile_h = tm.size_y as usize;
                let tile_row_len = tile_w * bps * samples;

                let x_off = col as usize * tile_w * bps * samples;
                let y_off = row as usize * tile_h;
                // Copy each tile row into the output, clipping at the edges.
                for trow in 0..tile_h {
                    let out_y = y_off + trow;
                    if out_y >= full_h {
                        break;
                    }
                    let src = &tile[trow * tile_row_len..(trow + 1) * tile_row_len];
                    let dst_start = out_y * out_row_len + x_off;
                    let copy_len = tile_row_len.min(out_row_len.saturating_sub(x_off));
                    out[dst_start..dst_start + copy_len].copy_from_slice(&src[..copy_len]);
                }
            }
        }
        Ok(out)
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
        crop_full_plane("MIAS", &full, meta, 1, x, y, w, h)
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
}

#[cfg(test)]
mod cellworx_log_tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        let mut d = std::env::temp_dir();
        d.push(format!(
            "bf_cellworx_{}_{}_{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn plate_log_scanner_sn_and_z_map_file() {
        let dir = tmp_dir("plate");
        let htd = dir.join("Plate1.HTD");
        let log = dir.join("Plate1_scan.log");
        std::fs::write(
            &log,
            "Some Header\n\
             Scanner SN : ABC-12345\n\
             Z Map File: C:/data/maps/zmap_001.zmp\n\
             Other: ignored\n",
        )
        .unwrap();

        let info = parse_plate_log(&log, &htd);
        assert_eq!(info.serial_number.as_deref(), Some("ABC-12345"));
        let zmap = info.z_map_file.expect("Z Map File parsed");
        // Last path segment resolved against the HTD's parent directory.
        assert_eq!(zmap, dir.join("zmap_001.zmp"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn plate_log_missing_keys_yield_none() {
        let dir = tmp_dir("plate_empty");
        let htd = dir.join("Plate2.HTD");
        let log = dir.join("Plate2_scan.log");
        std::fs::write(&log, "Header only\nNothing: here\n").unwrap();

        let info = parse_plate_log(&log, &htd);
        assert!(info.serial_number.is_none());
        assert!(info.z_map_file.is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn well_log_captures_key_value_scalars() {
        let dir = tmp_dir("well");
        let log = dir.join("Plate1_A01_scan.log");
        std::fs::write(
            &log,
            "Date: Mon Jan 02 13:45:30 2017\n\
             Scan Area: 10.5 x 8.0 mm\n\
             Channel 1: gain 1.5, EX 488/EM 525\n\
             NoColonLineSkipped\n",
        )
        .unwrap();

        let mut md = HashMap::new();
        parse_well_log(&log, &mut md);

        assert_eq!(
            md.get("Date").map(|v| v.to_string()).as_deref(),
            Some("Mon Jan 02 13:45:30 2017")
        );
        assert!(md.contains_key("Scan Area"));
        assert!(md.contains_key("Channel 1"));
        assert!(!md.contains_key("NoColonLineSkipped"));

        std::fs::remove_dir_all(&dir).ok();
    }
}
