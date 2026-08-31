//! bioformats-mias — format readers:
//!
//! - CellWorxReader: CellWorX HCS (.htd / .pnl)
//! - AliconaReader: Alicona AL3D SEM files (.al3d)
//! - OxfordInstrumentsReader: Oxford Instruments SEM/AFM (.top)
//! - FeiSerReader: FEI SER electron-microscopy series (.ser)

use std::collections::HashMap;
use std::io::{Cursor, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::ome_metadata::{
    create_lsid, OmeDetector, OmeInstrument, OmeMetadata, OmePlate, OmeROI, OmeWell, OmeWellSample,
};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;
use crate::tiff::ifd::{tag, Ifd};
use crate::tiff::parser::TiffParser;

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
        bits_per_pixel: (bps * 8) as u16,
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
    ome_template: Option<OmeMetadata>,
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
            ome_template: None,
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
                            info.z_steps,
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
        let ome_template = cellworx_companion_ome_template(&probe);
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
            parse_cellworx_well_log_structured(&well_log, &mut md, size_x, size_y, image_count);
            for (i, w) in info.wavelengths.iter().enumerate() {
                if let Some(name) = w {
                    md.insert(
                        format!("channel.{i}.name"),
                        MetadataValue::String(name.clone()),
                    );
                }
            }
            series.push(ImageMetadata {
                size_x,
                size_y,
                size_z: info.z_steps,
                size_c: channels as u32,
                size_t: info.n_timepoints,
                pixel_type,
                bits_per_pixel: (bits),
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
        self.ome_template = ome_template;
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
    let bytes = std::fs::read(path).map_err(BioFormatsError::Io)?;
    let content = String::from_utf8_lossy(&bytes);

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
    z_steps: u32,
    do_channels: bool,
) -> Vec<PathBuf> {
    let base = format!("{}{}", plate, well_name(row, col));
    let mut files: Vec<PathBuf> =
        Vec::with_capacity(field_count * channels * n_timepoints as usize * z_steps as usize);
    for field in 0..field_count {
        for channel in 0..channels {
            for _t in 0..n_timepoints {
                for _z in 0..z_steps {
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

fn parse_cellworx_well_log_structured(
    log_file: &Path,
    md: &mut HashMap<String, MetadataValue>,
    size_x: u32,
    size_y: u32,
    image_count: u32,
) {
    parse_well_log(log_file, md);

    if let Some(date) = metadata_string(md, "Date") {
        md.insert(
            "acquisition_date".into(),
            MetadataValue::String(date.clone()),
        );
        if let Some(iso) = cellworx_date_to_iso8601(&date) {
            md.insert(
                "acquisition_datetime_iso8601".into(),
                MetadataValue::String(iso),
            );
        }
    }

    if let Some(origin) = metadata_string(md, "Scan Origin") {
        let axes: Vec<&str> = origin.split(',').collect();
        if axes.len() >= 2 {
            if let (Some(x), Some(y)) = (parse_f64(axes[0]), parse_f64(axes[1])) {
                md.insert("WellSamplePositionX".into(), MetadataValue::Float(x));
                md.insert("WellSamplePositionY".into(), MetadataValue::Float(y));
                for plane in 0..image_count {
                    md.insert(format!("plane.{plane}.position_x"), MetadataValue::Float(x));
                    md.insert(format!("plane.{plane}.position_y"), MetadataValue::Float(y));
                }
            }
        }
    }

    if let Some(area) = metadata_string(md, "Scan Area") {
        if let Some((scan_x, scan_y)) = parse_cellworx_scan_area(&area) {
            if size_x > 0 {
                md.insert(
                    "PhysicalSizeX".into(),
                    MetadataValue::Float(scan_x / size_x as f64),
                );
                md.insert(
                    "physical_size_x".into(),
                    MetadataValue::Float(scan_x / size_x as f64),
                );
            }
            if size_y > 0 {
                md.insert(
                    "PhysicalSizeY".into(),
                    MetadataValue::Float(scan_y / size_y as f64),
                );
                md.insert(
                    "physical_size_y".into(),
                    MetadataValue::Float(scan_y / size_y as f64),
                );
            }
        }
    }

    let channel_lines: Vec<(String, String)> = md
        .iter()
        .filter_map(|(key, value)| {
            key.strip_prefix("Channel ")
                .and_then(|_| metadata_value_string(value))
                .map(|v| (key.clone(), v))
        })
        .collect();
    for (key, value) in channel_lines {
        let Some(channel_index) = cellworx_channel_index(&key) else {
            continue;
        };
        for token in value.split(',').map(str::trim) {
            if let Some(gain) = token.strip_prefix("gain ").and_then(parse_f64) {
                md.insert(
                    format!("channel.{channel_index}.detector_settings_gain"),
                    MetadataValue::Float(gain),
                );
                md.insert(
                    format!("channel.{channel_index}.detector_ref"),
                    MetadataValue::String("Detector:0:0".into()),
                );
                md.entry("detector.0.gain".into())
                    .or_insert(MetadataValue::Float(gain));
            } else if token.starts_with("EX") {
                if let Some((ex, em)) = parse_cellworx_ex_em(token) {
                    md.insert(
                        format!("channel.{channel_index}.excitation_wavelength"),
                        MetadataValue::Float(ex),
                    );
                    md.insert(
                        format!("channel.{channel_index}.emission_wavelength"),
                        MetadataValue::Float(em),
                    );
                }
            }
        }
    }
}

fn metadata_string(md: &HashMap<String, MetadataValue>, key: &str) -> Option<String> {
    md.get(key).and_then(metadata_value_string)
}

fn metadata_value_string(value: &MetadataValue) -> Option<String> {
    match value {
        MetadataValue::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        _ => None,
    }
}

fn parse_f64(value: &str) -> Option<f64> {
    value.trim().parse::<f64>().ok().filter(|v| v.is_finite())
}

fn parse_cellworx_scan_area(value: &str) -> Option<(f64, f64)> {
    let (x, rest) = value.split_once('x')?;
    let y = rest.split_whitespace().next()?;
    Some((parse_f64(x)?, parse_f64(y)?))
}

fn parse_cellworx_ex_em(token: &str) -> Option<(f64, f64)> {
    let (ex, em) = token.split_once('/')?;
    let ex = ex.split_whitespace().last()?;
    let em = em
        .split_whitespace()
        .nth(1)
        .or_else(|| em.split_whitespace().next())?;
    Some((parse_f64(ex)?, parse_f64(em)?))
}

fn cellworx_channel_index(key: &str) -> Option<usize> {
    let rest = key.strip_prefix("Channel ")?;
    let index = rest.split_whitespace().next()?.parse::<usize>().ok()?;
    index.checked_sub(1)
}

fn cellworx_date_to_iso8601(value: &str) -> Option<String> {
    let parts: Vec<&str> = value.split_whitespace().collect();
    if parts.len() != 5 {
        return None;
    }
    let month = match parts[1] {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let day = parts[2].parse::<u32>().ok()?;
    let time = parts[3];
    if time.split(':').count() != 3 {
        return None;
    }
    let year = parts[4].parse::<u32>().ok()?;
    Some(format!("{year:04}-{month:02}-{day:02}T{time}"))
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

fn enrich_ome_from_series_metadata(ome: &mut OmeMetadata, meta: &ImageMetadata) {
    if let Some(image) = ome.images.get_mut(0) {
        if let Some(v) =
            metadata_f64_any(&meta.series_metadata, &["physical_size_x", "PhysicalSizeX"])
        {
            image.physical_size_x = Some(v);
        }
        if let Some(v) =
            metadata_f64_any(&meta.series_metadata, &["physical_size_y", "PhysicalSizeY"])
        {
            image.physical_size_y = Some(v);
        }
        for (channel_index, channel) in image.channels.iter_mut().enumerate() {
            let prefix = format!("channel.{channel_index}");
            if let Some(gain) = metadata_f64_any(
                &meta.series_metadata,
                &[&format!("{prefix}.detector_settings_gain")],
            ) {
                channel.detector_settings_gain = Some(gain);
                channel.detector_ref = Some("Detector:0:0".into());
            }
            if let Some(detector) =
                metadata_string_any(&meta.series_metadata, &[&format!("{prefix}.detector_ref")])
            {
                channel.detector_ref = Some(detector);
            }
            if let Some(color) =
                metadata_i32_any(&meta.series_metadata, &[&format!("{prefix}.color")])
            {
                channel.color = Some(color);
            }
        }
    }
}

fn ome_from_all_mias_series(series: &[ImageMetadata]) -> OmeMetadata {
    let mut ome = OmeMetadata::default();
    let mut roi_index = 0usize;
    for meta in series {
        let mut image_ome = OmeMetadata::from_image_metadata(meta);
        enrich_ome_from_series_metadata(&mut image_ome, meta);
        offset_image_roi_refs(&mut image_ome.images, roi_index);
        ome.images.extend(image_ome.images);
        for mut roi in image_ome.rois {
            renumber_roi(&mut roi, roi_index);
            roi_index += 1;
            ome.rois.push(roi);
        }
        if ome.instruments.is_empty() {
            ome.instruments = image_ome.instruments;
        }
        if ome.experimenters.is_empty() {
            ome.experimenters = image_ome.experimenters;
        }
        if ome.annotations.is_empty() {
            ome.annotations = image_ome.annotations;
        }
    }
    ome
}

fn offset_image_roi_refs(images: &mut [crate::common::ome_metadata::OmeImage], offset: usize) {
    if offset == 0 {
        return;
    }
    for image in images {
        for roi_ref in &mut image.roi_refs {
            if let Some(index) = roi_ref
                .strip_prefix("ROI:")
                .and_then(|index| index.parse::<usize>().ok())
            {
                *roi_ref = create_lsid("ROI", &[offset + index]);
            }
        }
    }
}

fn renumber_roi(roi: &mut OmeROI, index: usize) {
    roi.id = Some(create_lsid("ROI", &[index]));
}

fn cellworx_plate_name(htd_path: Option<&Path>) -> Option<String> {
    let stem = htd_path?.file_stem()?.to_str()?;
    Some(stem.to_string())
}

fn add_cellworx_spw_metadata(ome: &mut OmeMetadata, reader: &CellWorxReader) {
    if reader.well_files.is_empty() || reader.field_count == 0 {
        return;
    }
    let rows = reader.well_files.len();
    let cols = reader.well_files.first().map(|r| r.len()).unwrap_or(0);
    if rows == 0 || cols == 0 {
        return;
    }

    let mut wells = Vec::with_capacity(rows * cols);
    for row in 0..rows {
        for col in 0..cols {
            let well_index = row * cols + col;
            let mut well_samples = Vec::new();
            if reader.well_files[row][col].is_some() {
                if let Some(selected_index) = reader
                    .selected_wells
                    .iter()
                    .position(|&(r, c)| r == row && c == col)
                {
                    for field in 0..reader.field_count {
                        let image_index = selected_index * reader.field_count + field;
                        if image_index >= ome.images.len() {
                            continue;
                        }
                        let (position_x, position_y) = reader
                            .series
                            .get(image_index)
                            .map(|meta| {
                                (
                                    metadata_f64_any(
                                        &meta.series_metadata,
                                        &["WellSamplePositionX"],
                                    ),
                                    metadata_f64_any(
                                        &meta.series_metadata,
                                        &["WellSamplePositionY"],
                                    ),
                                )
                            })
                            .unwrap_or((None, None));
                        well_samples.push(OmeWellSample {
                            id: Some(create_lsid("WellSample", &[0, well_index, field])),
                            index: image_index as u32,
                            image_ref: Some(image_index),
                            position_x,
                            position_y,
                        });
                        if let Some(image) = ome.images.get_mut(image_index) {
                            image.name =
                                Some(format!("Well {} Field #{}", well_name(row, col), field + 1));
                        }
                    }
                }
            }
            wells.push(OmeWell {
                id: Some(create_lsid("Well", &[0, well_index])),
                row: row as u32,
                column: col as u32,
                well_samples,
            });
        }
    }

    ome.plates.push(OmePlate {
        id: Some(create_lsid("Plate", &[0])),
        name: cellworx_plate_name(reader.htd_path.as_deref()),
        rows: rows as u32,
        columns: cols as u32,
        wells,
    });
}

fn add_cellworx_populated_planes(ome: &mut OmeMetadata, series: &[ImageMetadata]) {
    for (image, meta) in ome.images.iter_mut().zip(series) {
        if !image.planes.is_empty() {
            continue;
        }
        for plane_index in 0..meta.image_count {
            let (the_z, the_c, the_t) = zct_coords(meta, plane_index);
            image.planes.push(crate::common::ome_metadata::OmePlane {
                the_z,
                the_c,
                the_t,
                delta_t: None,
                exposure_time: None,
                position_x: None,
                position_y: None,
                position_z: None,
            });
        }
    }
}

fn cellworx_companion_ome_template(path: &Path) -> Option<OmeMetadata> {
    let mut metamorph = crate::formats::metamorph::MetamorphReader::new();
    if metamorph.set_id(path).is_ok() {
        if let Some(ome) = metamorph.ome_metadata() {
            return Some(ome);
        }
    }
    crate::registry::ImageReader::open(path)
        .ok()
        .and_then(|reader| reader.ome_metadata())
}

fn add_cellworx_template_metadata(ome: &mut OmeMetadata, template: Option<&OmeMetadata>) {
    let template_image = template.and_then(|template| template.images.first());
    for image in &mut ome.images {
        if let Some(template) = template_image {
            image.physical_size_x = template.physical_size_x;
            image.physical_size_y = template.physical_size_y;
            image.physical_size_z = template.physical_size_z;
            if image.time_increment.is_none() {
                image.time_increment = template.time_increment;
            }
            for (channel, template_channel) in
                image.channels.iter_mut().zip(template.channels.iter())
            {
                if channel.detector_ref.is_none() {
                    channel.detector_ref = template_channel.detector_ref.clone();
                }
                if channel.detector_settings_gain.is_none() {
                    channel.detector_settings_gain = template_channel.detector_settings_gain;
                }
                if channel.detector_settings_binning.is_none() {
                    channel.detector_settings_binning =
                        template_channel.detector_settings_binning.clone();
                }
                if channel.detector_settings_offset.is_none() {
                    channel.detector_settings_offset = template_channel.detector_settings_offset;
                }
            }
        }
        if image.instrument_ref.is_none() {
            image.instrument_ref = Some(0);
        }
    }

    if let Some(template) = template {
        if !template.instruments.is_empty() {
            ome.instruments = template.instruments.clone();
        }
    }
    if ome.instruments.is_empty() {
        ome.instruments.push(OmeInstrument {
            id: Some(create_lsid("Instrument", &[0])),
            detectors: vec![OmeDetector {
                id: Some(create_lsid("Detector", &[0, 0])),
                ..Default::default()
            }],
            ..Default::default()
        });
    }
}

fn mias_well_columns(n_wells: usize, wells: &[MiasWell]) -> u32 {
    if n_wells == 96 {
        12
    } else if n_wells == 384 {
        24
    } else {
        let max_col = wells
            .iter()
            .map(|well| ((well.well_number.max(0) as u32) % 24) + 1)
            .max()
            .unwrap_or(1);
        max_col.max(24)
    }
}

fn add_mias_spw_metadata(ome: &mut OmeMetadata, reader: &MiasReader) {
    if reader.wells.is_empty() {
        return;
    }
    let n_wells = reader.wells.len();
    let well_columns = mias_well_columns(n_wells, &reader.wells);
    let rows = if n_wells as u32 >= well_columns {
        (n_wells as u32) / well_columns
    } else {
        1
    };
    let template_meta = reader
        .template_file
        .as_deref()
        .and_then(parse_mias_template_file)
        .unwrap_or_default();
    let mut wells = Vec::with_capacity(reader.wells.len());
    for (well, mias_well) in reader.wells.iter().enumerate() {
        let well_index = mias_well.well_number.max(0) as u32;
        let row = well_index / well_columns;
        let column = well_index % well_columns;
        let well_sample_id = create_lsid("WellSample", &[0, well, 0]);
        if let Some(image) = ome.images.get_mut(well) {
            image.name = Some(format!("Well {}{}", mias_well_row_name(row), column + 1));
        }
        wells.push(OmeWell {
            id: Some(create_lsid("Well", &[0, well])),
            row,
            column,
            well_samples: vec![OmeWellSample {
                id: Some(well_sample_id),
                index: well as u32,
                image_ref: Some(well),
                position_x: None,
                position_y: None,
            }],
        });
    }
    ome.plates.push(OmePlate {
        id: Some(create_lsid("Plate", &[0])),
        name: reader
            .plate_name
            .as_deref()
            .and_then(mias_java_plate_label)
            .or(template_meta.plate_name)
            .or(template_meta.plate_external_id),
        rows,
        columns: well_columns,
        wells,
    });
}

fn mias_java_plate_label(plate_name: &str) -> Option<String> {
    if plate_name.is_empty() {
        return None;
    }
    let start = plate_name.find('-').map(|index| index + 1).unwrap_or(0);
    Some(plate_name[start..].to_string())
}

fn mias_well_row_name(mut row: u32) -> String {
    let mut name = String::new();
    loop {
        let rem = (row % 26) as u8;
        name.insert(0, (b'A' + rem) as char);
        if row < 26 {
            break;
        }
        row = row / 26 - 1;
    }
    name
}

fn metadata_f64_any(md: &HashMap<String, MetadataValue>, keys: &[&str]) -> Option<f64> {
    for key in keys {
        match md.get(*key) {
            Some(MetadataValue::Float(v)) if v.is_finite() => return Some(*v),
            Some(MetadataValue::Int(v)) => return Some(*v as f64),
            Some(MetadataValue::String(v)) => {
                if let Some(parsed) = parse_f64(v) {
                    return Some(parsed);
                }
            }
            _ => {}
        }
    }
    None
}

fn metadata_string_any(md: &HashMap<String, MetadataValue>, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(value) = md.get(*key).and_then(metadata_value_string) {
            return Some(value);
        }
    }
    None
}

fn metadata_i32_any(md: &HashMap<String, MetadataValue>, keys: &[&str]) -> Option<i32> {
    for key in keys {
        let Some(value) = md.get(*key) else {
            continue;
        };
        match value {
            MetadataValue::Int(v) => {
                if let Ok(v) = i32::try_from(*v) {
                    return Some(v);
                }
            }
            MetadataValue::String(v) => {
                if let Ok(v) = v.parse::<i32>() {
                    return Some(v);
                }
            }
            _ => {}
        }
    }
    None
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
        self.ome_template = None;
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

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        if self.series.is_empty() {
            return None;
        }
        let mut ome = ome_from_all_mias_series(&self.series);
        add_cellworx_template_metadata(&mut ome, self.ome_template.as_ref());
        add_cellworx_populated_planes(&mut ome, &self.series);
        add_cellworx_spw_metadata(&mut ome, self);
        Some(ome)
    }
}

// ── AliconaReader ────────────────────────────────────────────────────────────────

const AL3D_MAGIC_STRING: &str = "Alicona";
const AL3D_FULL_MAGIC_STRING: &str = "AliconaImaging";

pub struct AliconaReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    texture_offset: u64,
    num_bytes: usize,
    padded_rows: bool,
}

impl AliconaReader {
    pub fn new() -> Self {
        AliconaReader {
            path: None,
            meta: None,
            texture_offset: 0,
            num_bytes: 0,
            padded_rows: false,
        }
    }
}

impl Default for AliconaReader {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
struct Al3dParseResult {
    meta: ImageMetadata,
    texture_offset: u64,
    num_bytes: usize,
    padded_rows: bool,
}

fn al3d_pixel_type_from_bytes(bytes: usize) -> Result<PixelType> {
    match bytes {
        1 => Ok(PixelType::Uint8),
        2 => Ok(PixelType::Uint16),
        4 => Ok(PixelType::Uint32),
        8 => Ok(PixelType::Float64),
        _ => Err(BioFormatsError::UnsupportedFormat(format!(
            "AL3D unsupported byte depth: {bytes}"
        ))),
    }
}

fn parse_al3d_u32(value: &str, key: &str) -> Result<u32> {
    value.parse::<u32>().map_err(|e| {
        BioFormatsError::UnsupportedFormat(format!(
            "AL3D tag {key} has invalid integer {value:?}: {e}"
        ))
    })
}

fn parse_al3d_offset(value: &str, key: &str) -> Result<u64> {
    value.parse::<u64>().map_err(|e| {
        BioFormatsError::UnsupportedFormat(format!(
            "AL3D tag {key} has invalid offset {value:?}: {e}"
        ))
    })
}

fn parse_al3d(path: &Path) -> Result<Al3dParseResult> {
    let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
    if data.len() < 17 {
        return Err(BioFormatsError::UnsupportedFormat(
            "AL3D file too short for magic string".into(),
        ));
    }
    let magic = String::from_utf8_lossy(&data[..17]);
    if magic.trim() != AL3D_FULL_MAGIC_STRING {
        return Err(BioFormatsError::UnsupportedFormat(
            "AL3D file is missing AliconaImaging magic".into(),
        ));
    }

    let mut pos = 17usize;
    let mut count = 2usize;
    let mut i = 0usize;
    let mut width = 0u32;
    let mut height = 0u32;
    let mut image_count = 0u32;
    let mut texture_offset = 0u64;
    let mut depth_offset = 0u64;
    let mut has_c = false;
    let mut series_metadata = HashMap::new();

    while i < count {
        let tag = data.get(pos..pos + 52).ok_or_else(|| {
            BioFormatsError::UnsupportedFormat("AL3D tag table is truncated".into())
        })?;
        let key = String::from_utf8_lossy(&tag[..20])
            .trim_matches(char::from(0))
            .trim()
            .to_string();
        let value = String::from_utf8_lossy(&tag[20..50])
            .trim_matches(char::from(0))
            .trim()
            .to_string();
        series_metadata.insert(key.clone(), MetadataValue::String(value.clone()));

        match key.as_str() {
            "TagCount" => {
                count = count
                    .checked_add(parse_al3d_u32(&value, &key)? as usize)
                    .ok_or_else(|| BioFormatsError::Format("AL3D tag count overflows".into()))?
            }
            "Rows" => height = parse_al3d_u32(&value, &key)?,
            "Cols" => width = parse_al3d_u32(&value, &key)?,
            "NumberOfPlanes" => image_count = parse_al3d_u32(&value, &key)?,
            "TextureImageOffset" => texture_offset = parse_al3d_offset(&value, &key)?,
            "TexturePtr" if value != "7" => has_c = true,
            "DepthImageOffset" => depth_offset = parse_al3d_offset(&value, &key)?,
            _ => {}
        }

        pos += 52;
        i += 1;
    }

    if width == 0 || height == 0 {
        return Err(BioFormatsError::UnsupportedFormat(
            "AL3D file has zero image dimensions".into(),
        ));
    }
    if texture_offset == 0 && depth_offset == 0 {
        return Err(BioFormatsError::UnsupportedFormat(
            "AL3D file is missing TextureImageOffset or DepthImageOffset".into(),
        ));
    }

    let (pixel_type, size_c, size_t, image_count, texture_offset, num_bytes, padded_rows) =
        if texture_offset != 0 {
            if image_count == 0 {
                return Err(BioFormatsError::UnsupportedFormat(
                    "AL3D file has zero image planes".into(),
                ));
            }
            let divisor = (width as u64)
                .checked_mul(height as u64)
                .and_then(|v| v.checked_mul(image_count as u64))
                .ok_or_else(|| {
                    BioFormatsError::Format("AL3D byte-depth divisor overflows".into())
                })?;
            if data.len() as u64 <= texture_offset || divisor == 0 {
                return Err(BioFormatsError::UnsupportedFormat(
                    "AL3D texture payload is missing".into(),
                ));
            }
            let num_bytes = ((data.len() as u64 - texture_offset) / divisor) as usize;
            let pixel_type = al3d_pixel_type_from_bytes(num_bytes)?;
            let size_c = if has_c { 3 } else { 1 };
            let size_t = image_count / size_c;
            (
                pixel_type,
                size_c,
                size_t,
                image_count,
                texture_offset,
                num_bytes,
                true,
            )
        } else {
            (PixelType::Float32, 1, 1, 1, depth_offset, 4, false)
        };

    let mut meta = ImageMetadata {
        size_x: width,
        size_y: height,
        size_z: 1,
        size_c,
        size_t,
        pixel_type,
        bits_per_pixel: (pixel_type.bytes_per_sample() * 8) as u16,
        image_count,
        dimension_order: DimensionOrder::XYCTZ,
        is_rgb: false,
        is_interleaved: false,
        is_indexed: false,
        is_little_endian: true,
        resolution_count: 1,
        thumbnail: false,
        series_metadata,
        lookup_table: None,
        modulo_z: None,
        modulo_c: None,
        modulo_t: None,
    };
    if meta.size_t == 0 {
        meta.size_t = 1;
    }

    let pad = if padded_rows {
        (8 - (width % 8)) % 8
    } else {
        0
    };
    let plane_size = (width as u64)
        .checked_add(pad as u64)
        .and_then(|v| v.checked_mul(height as u64))
        .and_then(|v| v.checked_mul(num_bytes as u64))
        .ok_or_else(|| BioFormatsError::Format("AL3D padded plane size overflows".into()))?;
    let required_len =
        texture_offset
            .checked_add(plane_size.checked_mul(image_count as u64).ok_or_else(|| {
                BioFormatsError::Format("AL3D pixel payload size overflows".into())
            })?)
            .ok_or_else(|| BioFormatsError::Format("AL3D file size overflows".into()))?;
    if (data.len() as u64) < required_len {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "AL3D pixel payload is shorter than declared ({} < {required_len})",
            data.len()
        )));
    }
    Ok(Al3dParseResult {
        meta,
        texture_offset,
        num_bytes,
        padded_rows,
    })
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
        header.len() >= 16 && String::from_utf8_lossy(&header[..16]).contains(AL3D_MAGIC_STRING)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let parsed = parse_al3d(path)?;
        self.path = Some(path.to_path_buf());
        self.texture_offset = parsed.texture_offset;
        self.num_bytes = parsed.num_bytes;
        self.padded_rows = parsed.padded_rows;
        self.meta = Some(parsed.meta);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.texture_offset = 0;
        self.num_bytes = 0;
        self.padded_rows = false;
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
        let path = self
            .path
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?
            .clone();
        let mut f = std::fs::File::open(&path).map_err(BioFormatsError::Io)?;

        let width = meta.size_x as usize;
        let height = meta.size_y as usize;
        let pad = if self.padded_rows {
            (8 - (width % 8)) % 8
        } else {
            0
        };
        let padded_row = width + pad;
        let plane_samples = padded_row
            .checked_mul(height)
            .ok_or_else(|| BioFormatsError::Format("AL3D padded plane size overflows".into()))?;
        let plane_stride = plane_samples.checked_mul(self.num_bytes).ok_or_else(|| {
            BioFormatsError::Format("AL3D padded plane byte size overflows".into())
        })?;
        let plane_offset = self
            .texture_offset
            .checked_add(
                (plane_index as u64)
                    .checked_mul(plane_stride as u64)
                    .ok_or_else(|| BioFormatsError::Format("AL3D plane offset overflows".into()))?,
            )
            .ok_or_else(|| BioFormatsError::Format("AL3D plane offset overflows".into()))?;
        let plane_bytes = width
            .checked_mul(height)
            .and_then(|v| v.checked_mul(self.num_bytes))
            .ok_or_else(|| BioFormatsError::Format("AL3D plane byte size overflows".into()))?;

        if meta.pixel_type == PixelType::Float32 {
            let mut buf = vec![0u8; plane_bytes];
            f.seek(SeekFrom::Start(plane_offset))
                .map_err(BioFormatsError::Io)?;
            for row in 0..height {
                let dst = row * width * self.num_bytes;
                f.read_exact(&mut buf[dst..dst + width * self.num_bytes])
                    .map_err(BioFormatsError::Io)?;
                if pad > 0 && row + 1 < height {
                    f.seek(SeekFrom::Current((pad * self.num_bytes) as i64))
                        .map_err(BioFormatsError::Io)?;
                }
            }
            return Ok(buf);
        }

        let mut planar = vec![0u8; plane_bytes];
        for byte_index in 0..self.num_bytes {
            let byte_plane_offset = plane_offset
                .checked_add(
                    (byte_index as u64)
                        .checked_mul(plane_samples as u64)
                        .ok_or_else(|| {
                            BioFormatsError::Format("AL3D byte-plane offset overflows".into())
                        })?,
                )
                .ok_or_else(|| {
                    BioFormatsError::Format("AL3D byte-plane offset overflows".into())
                })?;
            f.seek(SeekFrom::Start(byte_plane_offset))
                .map_err(BioFormatsError::Io)?;
            for row in 0..height {
                let dst = byte_index * width * height + row * width;
                f.read_exact(&mut planar[dst..dst + width])
                    .map_err(BioFormatsError::Io)?;
                if pad > 0 && row + 1 < height {
                    f.seek(SeekFrom::Current(pad as i64))
                        .map_err(BioFormatsError::Io)?;
                }
            }
        }

        if self.num_bytes == 1 {
            return Ok(planar);
        }
        let mut buf = vec![0u8; plane_bytes];
        let samples = width * height;
        for i in 0..samples {
            for j in 0..self.num_bytes {
                buf[i * self.num_bytes + j] = planar[samples * j + i];
            }
        }
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

const OXFORD_MAGIC_STRING: &[u8] = b"Oxford Instruments";
const OXFORD_PRIMARY_DIMS_OFFSET: usize = 1048;
const OXFORD_FALLBACK_DIMS_OFFSET: usize = 1084;
const OXFORD_LUT_SIZE_OFFSET: usize = 1288;

pub struct OxfordInstrumentsReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    header_size: u64,
}

impl OxfordInstrumentsReader {
    pub fn new() -> Self {
        OxfordInstrumentsReader {
            path: None,
            meta: None,
            header_size: 0,
        }
    }
}

impl Default for OxfordInstrumentsReader {
    fn default() -> Self {
        Self::new()
    }
}

struct OxfordParseResult {
    meta: ImageMetadata,
    header_size: u64,
}

fn read_i32_le_at(data: &[u8], offset: usize, label: &str) -> Result<i32> {
    let bytes = data.get(offset..offset + 4).ok_or_else(|| {
        BioFormatsError::UnsupportedFormat(format!("Oxford TOP header is too short for {label}"))
    })?;
    Ok(i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn read_u32_le_at(data: &[u8], offset: usize, label: &str) -> Result<u32> {
    let bytes = data.get(offset..offset + 4).ok_or_else(|| {
        BioFormatsError::UnsupportedFormat(format!("Oxford TOP header is too short for {label}"))
    })?;
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn parse_oxford(path: &Path) -> Result<OxfordParseResult> {
    let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
    if data.len() < OXFORD_LUT_SIZE_OFFSET + 4 {
        return Err(BioFormatsError::UnsupportedFormat(
            "Oxford TOP header is too short for safe image decoding".to_string(),
        ));
    }
    let mut width = read_i32_le_at(&data, OXFORD_PRIMARY_DIMS_OFFSET, "primary width")?;
    let mut height = read_i32_le_at(&data, OXFORD_PRIMARY_DIMS_OFFSET + 4, "primary height")?;
    if width == 0 && height == 0 {
        width = read_i32_le_at(&data, OXFORD_FALLBACK_DIMS_OFFSET, "fallback width")?;
        height = read_i32_le_at(&data, OXFORD_FALLBACK_DIMS_OFFSET + 4, "fallback height")?;
    }
    if width <= 0 || height <= 0 {
        return Err(BioFormatsError::UnsupportedFormat(
            "Oxford TOP header is missing image dimensions".to_string(),
        ));
    }

    let mut meta = simple_meta(width as u32, height as u32, 1, PixelType::Uint16);
    if checked_payload_len(&meta)? + OXFORD_LUT_SIZE_OFFSET as u64 > data.len() as u64 {
        meta.size_y = 1;
    }

    let lut_size = read_u32_le_at(&data, OXFORD_LUT_SIZE_OFFSET, "LUT size")? as u64;
    let header_size = (OXFORD_LUT_SIZE_OFFSET as u64)
        .checked_add(4)
        .and_then(|n| n.checked_add(lut_size))
        .ok_or_else(|| BioFormatsError::Format("Oxford TOP header size overflows".into()))?;
    if header_size > data.len() as u64 {
        return Err(BioFormatsError::UnsupportedFormat(
            "Oxford TOP LUT payload is shorter than declared".to_string(),
        ));
    }
    let required_len = header_size
        .checked_add(checked_payload_len(&meta)?)
        .ok_or_else(|| BioFormatsError::Format("Oxford TOP file size overflows".into()))?;
    if (data.len() as u64) < required_len {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "Oxford TOP pixel payload is shorter than declared ({} < {required_len})",
            data.len()
        )));
    }
    meta.series_metadata.insert(
        "format".to_string(),
        MetadataValue::String("Oxford Instruments".to_string()),
    );
    Ok(OxfordParseResult { meta, header_size })
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
        _header.starts_with(OXFORD_MAGIC_STRING)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let parsed = parse_oxford(path)?;
        self.path = Some(path.to_path_buf());
        self.meta = Some(parsed.meta);
        self.header_size = parsed.header_size;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.header_size = 0;
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
        f.seek(SeekFrom::Start(self.header_size))
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
    dimension_order: DimensionOrder,
    well_number: i64,
}

pub struct MiasReader {
    wells: Vec<MiasWell>,
    series: Vec<ImageMetadata>,
    current_series: usize,
    tile_rows: u32,
    tile_cols: u32,
    analysis_files: Vec<MiasAnalysisFile>,
    template_file: Option<PathBuf>,
    plate_name: Option<String>,
    parse_masks: bool,
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
            analysis_files: Vec::new(),
            template_file: None,
            plate_name: None,
            parse_masks: false,
            tiff_reader: crate::tiff::TiffReader::new(),
            tiff_loaded: false,
        }
    }
}

#[derive(Clone, Debug)]
struct MiasAnalysisFile {
    filename: PathBuf,
    plate: i64,
    well: i64,
    kind: MiasAnalysisKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum MiasAnalysisKind {
    Result,
    PlateOutput,
    PlateResult,
    Detail,
    RoiOverlay,
    MaskOverlay,
    Other,
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
    let stem = name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name);
    let lname = stem.to_ascii_lowercase();
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

/// Parse the alternate MIAS layout used by Java MIASReader:
///
///   `<plate>/<well>/<channel>/<Z>_<T>_<tile-col>_<tile-row>.tif`
///
/// The Java FilePattern branch treats numeric filename blocks by their block
/// index: block 0 is Z, block 1 is T, block 2 is tile column, and block 3 is
/// tile row (with channel counted from the single-character parent
/// directories).
fn mias_alternate_blocks(path: &Path) -> Option<(i64, i64, i64, i64)> {
    let channel_dir = path.parent()?.file_name()?.to_str()?;
    if channel_dir.len() != 1 || !channel_dir.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }

    let name = path.file_name()?.to_str()?;
    let stem = name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name);
    let parts: Vec<&str> = stem.split('_').collect();
    if parts.len() != 4 || !parts.iter().all(|p| p.chars().all(|c| c.is_ascii_digit())) {
        return None;
    }

    Some((
        parts[0].parse().ok()?,
        parts[1].parse().ok()?,
        parts[2].parse().ok()?,
        parts[3].parse().ok()?,
    ))
}

fn mias_dimension_order_from_axes(axes: &[char]) -> DimensionOrder {
    let mut order = String::from("XY");
    for axis in axes {
        if !order.contains(*axis) {
            order.push(*axis);
        }
    }
    for axis in ['Z', 'C', 'T'] {
        if !order.contains(axis) {
            order.push(axis);
        }
    }
    match order.as_str() {
        "XYCTZ" => DimensionOrder::XYCTZ,
        "XYCZT" => DimensionOrder::XYCZT,
        "XYTCZ" => DimensionOrder::XYTCZ,
        "XYTZC" => DimensionOrder::XYTZC,
        "XYZTC" => DimensionOrder::XYZTC,
        _ => DimensionOrder::XYZCT,
    }
}

fn mias_java_dimension_order(path: &Path, alternate_layout: bool) -> DimensionOrder {
    if alternate_layout {
        // Java's numeric alternate layout visits blocks from right to left:
        // block 3 = tile row, block 2 = tile column, block 1 = T, block 0 = Z;
        // C is appended afterward from the single-character channel directory.
        return DimensionOrder::XYTZC;
    }

    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let stem = name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name);
    let mut axes = Vec::new();
    for part in stem.split('_').rev() {
        let lower = part.to_ascii_lowercase();
        if lower.starts_with('z') && lower[1..].chars().all(|c| c.is_ascii_digit()) {
            axes.push('Z');
        } else if lower.starts_with('t') && lower[1..].chars().all(|c| c.is_ascii_digit()) {
            axes.push('T');
        } else if lower.starts_with("mode") && lower[4..].chars().all(|c| c.is_ascii_digit()) {
            axes.push('C');
        }
    }
    mias_dimension_order_from_axes(&axes)
}

/// Identify whether a directory name is a MIAS well directory.
fn is_well_dir_name(name: &str) -> bool {
    if name.starts_with("Well") {
        return true;
    }
    // Four-digit well directory in the alternate layout.
    name.len() == 4 && name.chars().all(|c| c.is_ascii_digit())
}

fn is_in_mias_alternate_layout(path: &Path) -> bool {
    if mias_alternate_blocks(path).is_none() {
        return false;
    }
    path.parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .map(is_well_dir_name)
        .unwrap_or(false)
}

fn is_mias_software_header(header: &[u8]) -> bool {
    let mut parser = match TiffParser::new(Cursor::new(header)) {
        Ok(parser) => parser,
        Err(_) => return false,
    };
    let (ifd, _) = match parser.read_ifd(parser.first_ifd_offset) {
        Ok(ifd) => ifd,
        Err(_) => return false,
    };
    let Some(software) = ifd.get(tag::SOFTWARE).and_then(|v| v.as_str()) else {
        return false;
    };
    software.starts_with("eaZYX")
        || software.starts_with("SCIL_Image")
        || software.starts_with("IDL")
}

#[derive(Default)]
struct MiasCompanions {
    analysis_files: Vec<MiasAnalysisFile>,
    template_file: Option<PathBuf>,
}

#[derive(Default)]
struct MiasTemplateMetadata {
    plate_name: Option<String>,
    plate_external_id: Option<String>,
    physical_size_x: Option<f64>,
    physical_size_y: Option<f64>,
    objective_model: Option<String>,
    objective_magnification: Option<f64>,
    channel_names: Vec<String>,
    acquisition_date: Option<String>,
    exposure_time: Option<f64>,
}

fn resolve_mias_entrypoint(id: &Path) -> Result<PathBuf> {
    if !id
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("txt"))
        .unwrap_or(false)
    {
        return Ok(id.to_path_buf());
    }

    let base = id.canonicalize().unwrap_or_else(|_| id.to_path_buf());
    let parent = base.parent().ok_or_else(|| {
        BioFormatsError::UnsupportedFormat("MIAS: .txt entry has no parent directory".into())
    })?;
    let plate = match parent.file_name().and_then(|n| n.to_str()) {
        Some("Batchresults") => {
            let experiment = parent.parent().ok_or_else(|| {
                BioFormatsError::UnsupportedFormat(
                    "MIAS: Batchresults has no experiment parent".into(),
                )
            })?;
            first_child_dir(experiment).ok_or_else(|| {
                BioFormatsError::UnsupportedFormat(
                    "MIAS: no plate directory beside Batchresults".into(),
                )
            })?
        }
        Some("results") => parent.parent().unwrap_or(parent).to_path_buf(),
        _ => parent.to_path_buf(),
    };

    find_first_mias_tiff(&plate).ok_or_else(|| {
        BioFormatsError::UnsupportedFormat("MIAS: could not locate TIFF for .txt entry".into())
    })
}

fn first_child_dir(dir: &Path) -> Option<PathBuf> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    entries.sort();
    entries.into_iter().find(|p| {
        p.file_name()
            .and_then(|n| n.to_str())
            .map(|n| n != "Batchresults")
            .unwrap_or(false)
    })
}

fn find_first_mias_tiff(plate: &Path) -> Option<PathBuf> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(plate)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .collect();
    entries.sort();
    for well in entries {
        if !well.is_dir() {
            continue;
        }
        let name = well.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !is_well_dir_name(name) {
            continue;
        }
        let mut tiffs = collect_well_tiffs(&well);
        tiffs.sort();
        if let Some(tiff) = tiffs.into_iter().next() {
            return Some(tiff);
        }
    }
    None
}

fn mias_plate_number(plate: &Path) -> Option<i64> {
    let name = plate.file_name()?.to_str()?;
    let first_three: String = name.chars().take(3).collect();
    first_three.parse::<i64>().ok()
}

fn collect_mias_companions(
    plate: &Path,
    experiment: Option<&Path>,
    plate_number: Option<i64>,
) -> MiasCompanions {
    let mut companions = MiasCompanions::default();

    if let Some(experiment) = experiment {
        let batch = experiment.join("Batchresults");
        if let Ok(entries) = std::fs::read_dir(&batch) {
            let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
            paths.sort();
            for file in paths {
                let name = file.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if name.starts_with("NEO_Results") {
                    companions.analysis_files.push(MiasAnalysisFile {
                        filename: file,
                        plate: -1,
                        well: -1,
                        kind: MiasAnalysisKind::Result,
                    });
                } else if name.starts_with("NEO_PlateOutput_") {
                    let file_plate = name.get(16..19).and_then(|s| s.parse::<i64>().ok());
                    if file_plate == plate_number {
                        companions.analysis_files.push(MiasAnalysisFile {
                            filename: file,
                            plate: 0,
                            well: -1,
                            kind: MiasAnalysisKind::PlateOutput,
                        });
                    }
                }
            }
        }
    }

    let template = plate.join("Nugenesistemplate.txt");
    if template.exists() {
        companions.template_file = Some(template);
    }

    let results = plate.join("results");
    if let Ok(entries) = std::fs::read_dir(&results) {
        let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        paths.sort();
        for file in paths {
            let name = file
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            if name.ends_with(".sav") || name.ends_with(".dsv") || name.ends_with(".dat") {
                continue;
            }
            let lower = name.to_ascii_lowercase();
            let kind = if name.ends_with("detail.txt") {
                MiasAnalysisKind::Detail
            } else if name.ends_with("AllModesOverlay.tif") {
                MiasAnalysisKind::RoiOverlay
            } else if name.ends_with("overlay.tif") {
                MiasAnalysisKind::MaskOverlay
            } else if lower.ends_with(".txt") || lower.ends_with(".tif") || lower.ends_with(".tiff")
            {
                MiasAnalysisKind::PlateResult
            } else {
                MiasAnalysisKind::Other
            };
            companions.analysis_files.push(MiasAnalysisFile {
                filename: file,
                plate: 0,
                well: mias_well_from_analysis_name(&name),
                kind,
            });
        }
    }

    companions
}

fn mias_well_from_analysis_name(name: &str) -> i64 {
    if name.to_ascii_lowercase().starts_with("well") && name.len() >= 8 {
        name.get(4..8)
            .and_then(|s| s.parse::<i64>().ok())
            .map(|v| v - 1)
            .unwrap_or(-1)
    } else {
        -1
    }
}

fn parse_mias_template_file(path: &Path) -> Option<MiasTemplateMetadata> {
    let content = std::fs::read_to_string(path).ok()?;
    let mut template = MiasTemplateMetadata::default();
    let mut date = None;
    for raw in content.split(['\n', '\r']) {
        let Some((key, value)) = raw.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        match key {
            "Barcode" => template.plate_external_id = Some(value.to_string()),
            "Carrier" => template.plate_name = Some(value.to_string()),
            "Pixel_X" => template.physical_size_x = parse_f64(value),
            "Pixel_Y" => template.physical_size_y = parse_f64(value),
            "Objective_ID" => template.objective_model = Some(value.to_string()),
            "Magnification" => template.objective_magnification = parse_f64(value),
            "Date" => date = Some(value.to_string()),
            "Time" => {
                let prefix = date.take().unwrap_or_default();
                template.acquisition_date = Some(format!("{prefix} {value}").trim().to_string());
            }
            "Exposure" => template.exposure_time = parse_f64(value),
            _ if key.starts_with("Mode_") => template.channel_names.push(value.to_string()),
            _ => {}
        }
    }
    if template.acquisition_date.is_none() {
        template.acquisition_date = date;
    }
    Some(template)
}

fn apply_mias_template_metadata(
    md: &mut HashMap<String, MetadataValue>,
    template: &MiasTemplateMetadata,
    logical_channels: u32,
) {
    if let Some(value) = &template.plate_name {
        md.insert(
            "mias.plate.name".into(),
            MetadataValue::String(value.clone()),
        );
    }
    if let Some(value) = &template.plate_external_id {
        md.insert(
            "mias.plate.external_identifier".into(),
            MetadataValue::String(value.clone()),
        );
    }
    if let Some(value) = template.physical_size_x {
        md.insert("PhysicalSizeX".into(), MetadataValue::Float(value));
        md.insert("physical_size_x".into(), MetadataValue::Float(value));
    }
    if let Some(value) = template.physical_size_y {
        md.insert("PhysicalSizeY".into(), MetadataValue::Float(value));
        md.insert("physical_size_y".into(), MetadataValue::Float(value));
    }
    if let Some(value) = &template.objective_model {
        md.insert(
            "objective.0.model".into(),
            MetadataValue::String(value.clone()),
        );
    }
    if let Some(value) = template.objective_magnification {
        md.insert(
            "objective.0.nominal_magnification".into(),
            MetadataValue::Float(value),
        );
    }
    if let Some(value) = &template.acquisition_date {
        md.insert(
            "acquisition_date".into(),
            MetadataValue::String(value.clone()),
        );
    }
    if let Some(value) = template.exposure_time {
        for plane in 0..logical_channels.max(1) {
            md.insert(
                format!("plane.{plane}.exposure_time"),
                MetadataValue::Float(value),
            );
        }
    }
    for (index, name) in template.channel_names.iter().enumerate() {
        md.insert(
            format!("channel.{index}.name"),
            MetadataValue::String(name.clone()),
        );
    }
}

fn add_mias_companion_metadata(
    md: &mut HashMap<String, MetadataValue>,
    analysis_files: &[MiasAnalysisFile],
    template_file: Option<&Path>,
    well_number: i64,
) {
    if let Some(template) = template_file {
        md.insert(
            "mias.template_file".into(),
            MetadataValue::String(template.to_string_lossy().into_owned()),
        );
    }

    let mut count = 0usize;
    let mut detail_count = 0usize;
    let mut roi_overlay_count = 0usize;
    let mut mask_overlay_count = 0usize;
    let mut roi_count = 0usize;
    for file in analysis_files {
        if file.plate > 0 {
            continue;
        }
        if !(file.well == well_number || file.well < 0) {
            continue;
        }
        md.insert(
            format!("mias.analysis_file.{count}"),
            MetadataValue::String(file.filename.to_string_lossy().into_owned()),
        );
        md.insert(
            format!("mias.analysis_file.{count}.kind"),
            MetadataValue::String(format!("{:?}", file.kind)),
        );
        match file.kind {
            MiasAnalysisKind::Detail => {
                detail_count += 1;
                roi_count += add_mias_detail_rois(md, &file.filename, roi_count);
            }
            MiasAnalysisKind::RoiOverlay => roi_overlay_count += 1,
            MiasAnalysisKind::MaskOverlay => mask_overlay_count += 1,
            _ => {}
        }
        count += 1;
    }
    md.insert(
        "mias.analysis_file_count".into(),
        MetadataValue::Int(count as i64),
    );
    md.insert(
        "mias.roi_detail_file_count".into(),
        MetadataValue::Int(detail_count as i64),
    );
    md.insert(
        "mias.roi_overlay_file_count".into(),
        MetadataValue::Int(roi_overlay_count as i64),
    );
    md.insert(
        "mias.mask_overlay_file_count".into(),
        MetadataValue::Int(mask_overlay_count as i64),
    );
    md.insert(
        "mias.roi_detail_count".into(),
        MetadataValue::Int(roi_count as i64),
    );
}

fn add_mias_overlay_channel_colors(
    md: &mut HashMap<String, MetadataValue>,
    colors: &[Option<i32>],
) {
    for (channel, color) in colors.iter().enumerate() {
        if let Some(color) = color {
            md.insert(
                format!("channel.{channel}.color"),
                MetadataValue::Int(i64::from(*color)),
            );
        }
    }
}

fn collect_mias_overlay_channel_colors(
    analysis_files: &[MiasAnalysisFile],
    size_c: u32,
) -> Vec<Option<i32>> {
    let mut colors = vec![None; size_c as usize];
    for file in analysis_files {
        if file.kind != MiasAnalysisKind::RoiOverlay {
            continue;
        }
        let Some((_, _, _, channel)) = mias_position_from_analysis_file(&file.filename) else {
            continue;
        };
        let Ok(channel) = usize::try_from(channel) else {
            continue;
        };
        if channel >= colors.len() || colors[channel].is_some() {
            continue;
        }
        colors[channel] = mias_channel_color_from_overlay(&file.filename);
    }
    colors
}

fn add_mias_mask_rois(
    md: &mut HashMap<String, MetadataValue>,
    analysis_files: &[MiasAnalysisFile],
    well_number: i64,
    size_x: u32,
    size_y: u32,
) {
    let mut next_roi = mias_next_roi_index(md);
    for file in analysis_files {
        if file.kind != MiasAnalysisKind::MaskOverlay || file.well != well_number {
            continue;
        }
        for (channel, bin_data) in mias_masks_from_overlay(&file.filename)
            .into_iter()
            .enumerate()
        {
            let Some(bin_data) = bin_data else {
                continue;
            };
            let color = match channel {
                0 => pack_ome_rgba(255, 0, 0, 255),
                1 => pack_ome_rgba(0, 255, 0, 255),
                2 => pack_ome_rgba(0, 0, 255, 255),
                _ => continue,
            };
            let prefix = format!("roi.{next_roi}");
            md.insert(
                format!("{prefix}.shape"),
                MetadataValue::String("mask".into()),
            );
            md.insert(format!("{prefix}.x"), MetadataValue::Float(0.0));
            md.insert(format!("{prefix}.y"), MetadataValue::Float(0.0));
            md.insert(
                format!("{prefix}.width"),
                MetadataValue::Float(size_x as f64),
            );
            md.insert(
                format!("{prefix}.height"),
                MetadataValue::Float(size_y as f64),
            );
            md.insert(
                format!("{prefix}.stroke_color"),
                MetadataValue::Int(i64::from(color)),
            );
            md.insert(
                format!("{prefix}.fill_color"),
                MetadataValue::Int(i64::from(color)),
            );
            md.insert(format!("{prefix}.bin_data"), MetadataValue::Bytes(bin_data));
            md.insert(
                format!("image.roi_ref.{next_roi}"),
                MetadataValue::Int(next_roi as i64),
            );
            next_roi += 1;
        }
    }
}

fn mias_next_roi_index(md: &HashMap<String, MetadataValue>) -> usize {
    md.keys()
        .filter_map(|key| {
            let mut parts = key.split('.');
            match (parts.next(), parts.next()) {
                (Some("roi"), Some(index)) => index.parse::<usize>().ok(),
                _ => None,
            }
        })
        .max()
        .map(|index| index + 1)
        .unwrap_or(0)
}

fn mias_masks_from_overlay(file: &Path) -> Vec<Option<Vec<u8>>> {
    let mut data = Vec::new();
    if std::fs::File::open(file)
        .and_then(|mut file| file.read_to_end(&mut data))
        .is_err()
    {
        return vec![None, None, None];
    }
    let mut parser = match TiffParser::new(Cursor::new(&data)) {
        Ok(parser) => parser,
        Err(_) => return vec![None, None, None],
    };
    let (ifd, _) = match parser.read_ifd(parser.first_ifd_offset) {
        Ok(result) => result,
        Err(_) => return vec![None, None, None],
    };
    let width = ifd.get_u32(tag::IMAGE_WIDTH).unwrap_or(0) as usize;
    let height = ifd.get_u32(tag::IMAGE_LENGTH).unwrap_or(0) as usize;
    let pixel_count = width.saturating_mul(height);
    if pixel_count == 0 {
        return vec![None, None, None];
    }
    let color_map = ifd
        .get(tag::COLOR_MAP)
        .map(|v| v.as_vec_u16())
        .unwrap_or_default();
    let n_entries = color_map.len() / 3;
    let plane = match mias_overlay_plane_bytes(&data, &ifd) {
        Some(plane) => plane,
        None => return vec![None, None, None],
    };

    let mut rgb = if n_entries > 0 && color_map.len() == n_entries * 3 {
        let mut rgb = vec![vec![0u8; pixel_count]; 3];
        for (pixel, &index) in plane.iter().take(pixel_count).enumerate() {
            let index = index as usize;
            if index >= n_entries {
                continue;
            }
            for channel in 0..3 {
                rgb[channel][pixel] = (color_map[channel * n_entries + index] >> 8) as u8;
            }
        }
        rgb
    } else {
        match mias_rgb_planes_from_overlay(&ifd, &plane, pixel_count) {
            Some(rgb) => rgb,
            None => return vec![None, None, None],
        }
    };
    for pixel in 0..pixel_count {
        let first = rgb[0][pixel];
        if rgb.iter().all(|channel| channel[pixel] == first) {
            for channel in &mut rgb {
                channel[pixel] = 0;
            }
        }
    }

    (0..3)
        .map(|channel| mias_pack_mask_bits(&rgb[channel]))
        .collect()
}

fn mias_overlay_plane_bytes(data: &[u8], ifd: &Ifd) -> Option<Vec<u8>> {
    let offsets = ifd.get_vec_u32(tag::STRIP_OFFSETS);
    let byte_counts = ifd.get_vec_u32(tag::STRIP_BYTE_COUNTS);
    if offsets.is_empty() || byte_counts.is_empty() {
        return None;
    }
    let mut plane = Vec::new();
    for (&offset, &byte_count) in offsets.iter().zip(byte_counts.iter()) {
        let offset = offset as usize;
        let byte_count = byte_count as usize;
        if offset >= data.len() {
            return None;
        }
        let end = offset.saturating_add(byte_count).min(data.len());
        plane.extend_from_slice(&data[offset..end]);
    }
    Some(plane)
}

fn mias_rgb_planes_from_overlay(
    ifd: &Ifd,
    plane: &[u8],
    pixel_count: usize,
) -> Option<Vec<Vec<u8>>> {
    let samples = usize::from(ifd.samples_per_pixel());
    if samples < 3 {
        return None;
    }
    let bits = ifd.bits_per_sample();
    let bytes_per_sample = if bits.is_empty() {
        1
    } else {
        let first = bits[0];
        if first != 8 || bits.iter().take(samples).any(|&b| b != first) {
            return None;
        }
        usize::from(first / 8)
    };
    if bytes_per_sample != 1 {
        return None;
    }

    let planar = ifd.planar_configuration() == 2;
    let mut rgb = vec![vec![0u8; pixel_count]; 3];
    if planar {
        let required = pixel_count.checked_mul(samples)?;
        if plane.len() < required {
            return None;
        }
        for channel in 0usize..3 {
            let start = channel.checked_mul(pixel_count)?;
            let end = start.checked_add(pixel_count)?;
            rgb[channel].copy_from_slice(&plane[start..end]);
        }
    } else {
        let required = pixel_count.checked_mul(samples)?;
        if plane.len() < required {
            return None;
        }
        for pixel in 0..pixel_count {
            let base = pixel.checked_mul(samples)?;
            for channel in 0..3 {
                rgb[channel][pixel] = plane[base + channel];
            }
        }
    }
    Some(rgb)
}

fn mias_pack_mask_bits(plane: &[u8]) -> Option<Vec<u8>> {
    let mut valid = false;
    let mut out = vec![0u8; (plane.len() + 7) / 8];
    for (index, &pixel) in plane.iter().enumerate() {
        if pixel != 0 {
            valid = true;
            out[index / 8] |= 1 << (7 - (index % 8));
        }
    }
    valid.then_some(out)
}

fn mias_channel_color_from_overlay(file: &Path) -> Option<i32> {
    let mut data = Vec::new();
    std::fs::File::open(file)
        .ok()?
        .read_to_end(&mut data)
        .ok()?;
    let mut parser = TiffParser::new(Cursor::new(data)).ok()?;
    let (ifd, _) = parser.read_ifd(parser.first_ifd_offset).ok()?;
    let color_map = ifd.get(tag::COLOR_MAP)?.as_vec_u16();
    let n_entries = color_map.len() / 3;
    if n_entries == 0 || color_map.len() != n_entries * 3 {
        return None;
    }

    let mut max = i32::MIN;
    let mut max_index = None;
    for c in 0..3 {
        let value = ((color_map[c * n_entries] >> 8) & 0xff) as i32;
        if value > max {
            max = value;
            max_index = Some(c);
        } else if value == max {
            return Some(pack_ome_rgba(0, 0, 0, 255));
        }
    }

    match max_index {
        Some(0) => Some(pack_ome_rgba(255, 0, 0, 255)),
        Some(1) => Some(pack_ome_rgba(0, 255, 0, 255)),
        Some(2) => Some(pack_ome_rgba(0, 0, 255, 255)),
        _ => None,
    }
}

fn pack_ome_rgba(red: u8, green: u8, blue: u8, alpha: u8) -> i32 {
    u32::from_be_bytes([red, green, blue, alpha]) as i32
}

fn add_mias_detail_rois(
    md: &mut HashMap<String, MetadataValue>,
    detail_file: &Path,
    first_roi: usize,
) -> usize {
    let content = match std::fs::read_to_string(detail_file) {
        Ok(content) => content,
        Err(_) => return 0,
    };
    let (the_t, the_z) = mias_position_from_analysis_file(detail_file)
        .map(|(_, t, z, _)| (Some(t), Some(z)))
        .unwrap_or((None, None));
    let mut columns: Option<Vec<String>> = None;
    let mut count = 0usize;

    for raw in content.lines() {
        let line = raw.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        if columns.is_none() {
            if line.starts_with("Label") {
                columns = Some(line.split('\t').map(|s| s.trim().to_string()).collect());
            }
            continue;
        }

        let Some(columns) = columns.as_ref() else {
            continue;
        };
        let data: Vec<&str> = line.split('\t').map(str::trim).collect();
        let Some(label) = mias_detail_column(columns, &data, "Label") else {
            continue;
        };
        let Some(x) = mias_detail_column(columns, &data, "Col").and_then(parse_f64) else {
            continue;
        };
        let Some(y) = mias_detail_column(columns, &data, "Row").and_then(parse_f64) else {
            continue;
        };
        let Some(diameter) = mias_detail_column(columns, &data, "Cell Diam.").and_then(parse_f64)
        else {
            continue;
        };
        if diameter <= 0.0 {
            continue;
        }

        let roi = first_roi + count;
        let prefix = format!("roi.{roi}");
        md.insert(
            format!("{prefix}.name"),
            MetadataValue::String(label.to_string()),
        );
        md.insert(
            format!("image.roi_ref.{roi}"),
            MetadataValue::Int(roi as i64),
        );
        md.insert(
            format!("{prefix}.label"),
            MetadataValue::String(label.to_string()),
        );
        md.insert(format!("{prefix}.x"), MetadataValue::Float(x));
        md.insert(format!("{prefix}.y"), MetadataValue::Float(y));
        md.insert(
            format!("{prefix}.radius_x"),
            MetadataValue::Float(diameter / 2.0),
        );
        md.insert(
            format!("{prefix}.radius_y"),
            MetadataValue::Float(diameter / 2.0),
        );
        if let Some(t) = the_t {
            md.insert(format!("{prefix}.the_t"), MetadataValue::Int(t));
        }
        if let Some(z) = the_z {
            md.insert(format!("{prefix}.the_z"), MetadataValue::Int(z));
        }
        count += 1;
    }

    count
}

fn mias_detail_column<'a>(columns: &[String], data: &'a [&'a str], name: &str) -> Option<&'a str> {
    columns
        .iter()
        .position(|column| column == name)
        .and_then(|index| data.get(index).copied())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn mias_position_from_analysis_file(file: &Path) -> Option<(i64, i64, i64, i64)> {
    let name = file.file_name()?.to_str()?;
    let well = name
        .strip_prefix("Well")?
        .split_once('_')?
        .0
        .parse::<i64>()
        .ok()?
        - 1;
    let t = mias_analysis_token(name, "_t")?;
    let z = mias_analysis_token(name, "_z")?;
    let c = name
        .find("mode")
        .and_then(|start| {
            let rest = &name[start + 4..];
            let end = rest.find('_').unwrap_or(rest.len());
            rest[..end].parse::<i64>().ok()
        })
        .map(|value| value - 1)?;
    Some((well, t, z, c))
}

fn mias_analysis_token(name: &str, marker: &str) -> Option<i64> {
    let start = name.find(marker)? + marker.len();
    let rest = &name[start..];
    let end = rest.find('_').unwrap_or(rest.len());
    rest[..end].parse::<i64>().ok()
}

fn well_number_from_name(name: &str) -> i64 {
    let stripped = name.trim_start_matches("Well");
    stripped.trim().parse::<i64>().map(|v| v - 1).unwrap_or(0)
}

impl MiasReader {
    /// Toggle MIAS mask overlay parsing, matching Java
    /// `MIASReader.setAutomaticallyParseMasks`. Disabled by default.
    pub fn set_automatically_parse_masks(&mut self, parse: bool) {
        self.parse_masks = parse;
    }

    /// Locate the plate directory and enumerate well directories given a TIFF
    /// (or well directory) path inside a MIAS hierarchy.
    fn build(&mut self, id: &Path) -> Result<()> {
        let entry = resolve_mias_entrypoint(id)?;
        let base = entry.canonicalize().unwrap_or(entry);

        // The well directory is the parent of a normal-layout TIFF. In the
        // alternate numeric layout the TIFF lives under a channel directory, so
        // the well directory is one level higher, matching Java's
        // baseFile.getParentFile().getParentFile() plate discovery.
        let well_dir = if base.is_dir() {
            base.clone()
        } else if is_in_mias_alternate_layout(&base) {
            base.parent()
                .and_then(|p| p.parent())
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| base.parent().unwrap_or(&base).to_path_buf())
        } else {
            base.parent()
                .map(|p| p.to_path_buf())
                .unwrap_or(base.clone())
        };
        let plate_dir = well_dir
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or(well_dir.clone());
        self.plate_name = plate_dir
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| name.to_string());
        let experiment_dir = plate_dir.parent().map(|p| p.to_path_buf());
        let plate_number = mias_plate_number(&plate_dir);
        let companions =
            collect_mias_companions(&plate_dir, experiment_dir.as_deref(), plate_number);
        self.template_file = companions.template_file.clone();
        self.analysis_files = companions.analysis_files.clone();
        let template_meta = self
            .template_file
            .as_deref()
            .and_then(parse_mias_template_file)
            .unwrap_or_default();

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
            let mut alt_rows: Vec<i64> = Vec::new();
            let mut alt_cols: Vec<i64> = Vec::new();
            let mut saw_alternate_layout = false;
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
                if let Some((z, tt, col, row)) = mias_alternate_blocks(t) {
                    saw_alternate_layout = true;
                    if !alt_rows.contains(&row) {
                        alt_rows.push(row);
                    }
                    if !alt_cols.contains(&col) {
                        alt_cols.push(col);
                    }
                    if !z_vals.contains(&z) {
                        z_vals.push(z);
                    }
                    if !t_vals.contains(&tt) {
                        t_vals.push(tt);
                    }
                    if let Some(ch) = t
                        .parent()
                        .and_then(|p| p.file_name())
                        .and_then(|n| n.to_str())
                        .and_then(|s| s.parse::<i64>().ok())
                    {
                        if !c_vals.contains(&ch) {
                            c_vals.push(ch);
                        }
                    }
                }
            }
            let size_z = (z_vals.len() as u32).max(1);
            let size_t = (t_vals.len() as u32).max(1);
            let size_c = (c_vals.len() as u32).max(1);
            let well_tile_rows = if saw_alternate_layout {
                alt_rows.len() as u32
            } else {
                im_rows.len() as u32
            };
            let well_tile_cols = if saw_alternate_layout {
                alt_cols.len() as u32
            } else {
                im_cols.len() as u32
            };
            if well_tile_rows > self.tile_rows {
                self.tile_rows = well_tile_rows;
            }
            if well_tile_cols > self.tile_cols {
                self.tile_cols = well_tile_cols;
            }

            let name = wd.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let dimension_order = mias_java_dimension_order(&tiffs[0], saw_alternate_layout);
            wells.push(MiasWell {
                tiffs,
                size_z,
                size_c,
                size_t,
                dimension_order,
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
        let max_size_c = wells
            .iter()
            .map(|w| w.size_c.saturating_mul(tiff_c))
            .max()
            .unwrap_or(tiff_c);
        let overlay_channel_colors =
            collect_mias_overlay_channel_colors(&self.analysis_files, max_size_c);
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
            add_mias_companion_metadata(
                &mut meta_map,
                &self.analysis_files,
                self.template_file.as_deref(),
                w.well_number,
            );
            let image_count = (w.size_z * w.size_t * w.size_c).max(1);
            apply_mias_template_metadata(&mut meta_map, &template_meta, image_count);
            add_mias_overlay_channel_colors(&mut meta_map, &overlay_channel_colors);
            let size_x = tile_w
                .checked_mul(self.tile_cols)
                .ok_or_else(|| BioFormatsError::Format("MIAS: mosaic width overflows".into()))?;
            let size_y = tile_h
                .checked_mul(self.tile_rows)
                .ok_or_else(|| BioFormatsError::Format("MIAS: mosaic height overflows".into()))?;
            if self.parse_masks {
                add_mias_mask_rois(
                    &mut meta_map,
                    &self.analysis_files,
                    w.well_number,
                    size_x,
                    size_y,
                );
            }
            series.push(ImageMetadata {
                size_x,
                size_y,
                size_z: w.size_z,
                size_c,
                size_t: w.size_t,
                pixel_type,
                bits_per_pixel: (bits),
                image_count,
                dimension_order: w.dimension_order,
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
        if path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("txt"))
            .unwrap_or(false)
        {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let parent = path
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .unwrap_or("");
            return parent == "results"
                || parent == "Batchresults"
                || name == "Nugenesistemplate.txt"
                || name.starts_with("mode");
        }
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
        (in_well_dir && (mias_block(name, "mode").is_some() || mias_block(name, "z").is_some()))
            || is_in_mias_alternate_layout(path)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        is_mias_software_header(header)
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
                "MIAS: file is not a Well<xxxx>/mode<c>_z<zzz>_t<ttt> TIFF dataset or alternate numeric MIAS layout".into(),
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
        self.analysis_files.clear();
        self.template_file = None;
        self.plate_name = None;
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

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        if self.series.is_empty() {
            return None;
        }
        let mut ome = ome_from_all_mias_series(&self.series);
        add_mias_spw_metadata(&mut ome, self);
        Some(ome)
    }
}

#[cfg(test)]
mod cellworx_log_tests {
    use super::*;

    fn build_tiff_with_software(software: &str) -> Vec<u8> {
        let mut value = software.as_bytes().to_vec();
        value.push(0);

        let ifd_start = 8u32;
        let value_offset = 8 + 2 + 12 + 4;
        let mut tiff = Vec::new();
        tiff.extend_from_slice(b"II");
        tiff.extend_from_slice(&42u16.to_le_bytes());
        tiff.extend_from_slice(&ifd_start.to_le_bytes());
        tiff.extend_from_slice(&1u16.to_le_bytes());
        tiff.extend_from_slice(&tag::SOFTWARE.to_le_bytes());
        tiff.extend_from_slice(&2u16.to_le_bytes());
        tiff.extend_from_slice(&(value.len() as u32).to_le_bytes());
        if value.len() <= 4 {
            let mut inline = [0u8; 4];
            inline[..value.len()].copy_from_slice(&value);
            tiff.extend_from_slice(&inline);
        } else {
            tiff.extend_from_slice(&(value_offset as u32).to_le_bytes());
        }
        tiff.extend_from_slice(&0u32.to_le_bytes());
        if value.len() > 4 {
            tiff.extend_from_slice(&value);
        }
        tiff
    }

    fn push_u16_le(out: &mut Vec<u8>, value: u16) {
        out.extend_from_slice(&value.to_le_bytes());
    }

    fn push_u32_le(out: &mut Vec<u8>, value: u32) {
        out.extend_from_slice(&value.to_le_bytes());
    }

    fn push_ifd_short(out: &mut Vec<u8>, tag_id: u16, value: u16) {
        push_u16_le(out, tag_id);
        push_u16_le(out, 3);
        push_u32_le(out, 1);
        push_u16_le(out, value);
        push_u16_le(out, 0);
    }

    fn push_ifd_long(out: &mut Vec<u8>, tag_id: u16, value: u32) {
        push_u16_le(out, tag_id);
        push_u16_le(out, 4);
        push_u32_le(out, 1);
        push_u32_le(out, value);
    }

    fn push_ifd_short_array(out: &mut Vec<u8>, tag_id: u16, count: u32, offset: u32) {
        push_u16_le(out, tag_id);
        push_u16_le(out, 3);
        push_u32_le(out, count);
        push_u32_le(out, offset);
    }

    fn build_palette_tiff(color_map: &[u16]) -> Vec<u8> {
        build_palette_tiff_pixels(1, 1, color_map, &[0])
    }

    fn build_palette_tiff_pixels(
        width: u32,
        height: u32,
        color_map: &[u16],
        pixels: &[u8],
    ) -> Vec<u8> {
        let entry_count = 9u16;
        let ifd_start = 8u32;
        let color_map_offset = ifd_start + 2 + u32::from(entry_count) * 12 + 4;
        let pixel_offset = color_map_offset + (color_map.len() as u32 * 2);
        let mut tiff = Vec::new();
        tiff.extend_from_slice(b"II");
        push_u16_le(&mut tiff, 42);
        push_u32_le(&mut tiff, ifd_start);
        push_u16_le(&mut tiff, entry_count);
        push_ifd_long(&mut tiff, tag::IMAGE_WIDTH, width);
        push_ifd_long(&mut tiff, tag::IMAGE_LENGTH, height);
        push_ifd_short(&mut tiff, tag::BITS_PER_SAMPLE, 8);
        push_ifd_short(&mut tiff, tag::COMPRESSION, 1);
        push_ifd_short(&mut tiff, tag::PHOTOMETRIC_INTERPRETATION, 3);
        push_ifd_long(&mut tiff, tag::STRIP_OFFSETS, pixel_offset);
        push_ifd_long(&mut tiff, tag::STRIP_BYTE_COUNTS, pixels.len() as u32);
        push_ifd_short(&mut tiff, tag::SAMPLES_PER_PIXEL, 1);
        push_ifd_short_array(
            &mut tiff,
            tag::COLOR_MAP,
            color_map.len() as u32,
            color_map_offset,
        );
        push_u32_le(&mut tiff, 0);
        for &value in color_map {
            push_u16_le(&mut tiff, value);
        }
        tiff.extend_from_slice(pixels);
        tiff
    }

    fn build_rgb_tiff_pixels(width: u32, height: u32, pixels: &[u8]) -> Vec<u8> {
        let entry_count = 9u16;
        let ifd_start = 8u32;
        let bits_offset = ifd_start + 2 + u32::from(entry_count) * 12 + 4;
        let pixel_offset = bits_offset + 6;
        let mut tiff = Vec::new();
        tiff.extend_from_slice(b"II");
        push_u16_le(&mut tiff, 42);
        push_u32_le(&mut tiff, ifd_start);
        push_u16_le(&mut tiff, entry_count);
        push_ifd_long(&mut tiff, tag::IMAGE_WIDTH, width);
        push_ifd_long(&mut tiff, tag::IMAGE_LENGTH, height);
        push_ifd_short_array(&mut tiff, tag::BITS_PER_SAMPLE, 3, bits_offset);
        push_ifd_short(&mut tiff, tag::COMPRESSION, 1);
        push_ifd_short(&mut tiff, tag::PHOTOMETRIC_INTERPRETATION, 2);
        push_ifd_long(&mut tiff, tag::STRIP_OFFSETS, pixel_offset);
        push_ifd_long(&mut tiff, tag::STRIP_BYTE_COUNTS, pixels.len() as u32);
        push_ifd_short(&mut tiff, tag::SAMPLES_PER_PIXEL, 3);
        push_ifd_short(&mut tiff, tag::PLANAR_CONFIGURATION, 1);
        push_u32_le(&mut tiff, 0);
        push_u16_le(&mut tiff, 8);
        push_u16_le(&mut tiff, 8);
        push_u16_le(&mut tiff, 8);
        tiff.extend_from_slice(pixels);
        tiff
    }

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
    fn mias_byte_probe_matches_java_software_prefixes() {
        let reader = MiasReader::new();
        for software in ["eaZYX 1.0", "SCIL_Image 2.0", "IDL export"] {
            assert!(
                reader.is_this_type_by_bytes(&build_tiff_with_software(software)),
                "MIASReader should accept Software={software:?}"
            );
        }
        assert!(!reader.is_this_type_by_bytes(&build_tiff_with_software("ImageJ")));
        assert!(!reader.is_this_type_by_bytes(b"not a tiff"));
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

    #[test]
    fn well_log_projects_java_structured_metadata() {
        let dir = tmp_dir("well_structured");
        let log = dir.join("Plate1_A01_scan.log");
        std::fs::write(
            &log,
            "Date: Mon Jan 02 13:45:30 2017\n\
             Scan Origin: 12.5, -3.25\n\
             Scan Area: 100 x 50 um\n\
             Channel 1: gain 1.5, EX 488/EM 525\n\
             Channel 2: gain 2.25, EX 561/EM 610 nm\n",
        )
        .unwrap();

        let mut md = HashMap::new();
        parse_cellworx_well_log_structured(&log, &mut md, 200, 100, 2);

        assert_eq!(
            md.get("acquisition_datetime_iso8601")
                .map(|v| v.to_string())
                .as_deref(),
            Some("2017-01-02T13:45:30")
        );
        assert!(
            matches!(md.get("PhysicalSizeX"), Some(MetadataValue::Float(v)) if (*v - 0.5).abs() < 1e-9)
        );
        assert!(
            matches!(md.get("PhysicalSizeY"), Some(MetadataValue::Float(v)) if (*v - 0.5).abs() < 1e-9)
        );
        assert!(
            matches!(md.get("WellSamplePositionX"), Some(MetadataValue::Float(v)) if (*v - 12.5).abs() < 1e-9)
        );
        assert!(
            matches!(md.get("plane.1.position_y"), Some(MetadataValue::Float(v)) if (*v + 3.25).abs() < 1e-9)
        );
        assert!(
            matches!(md.get("channel.0.detector_settings_gain"), Some(MetadataValue::Float(v)) if (*v - 1.5).abs() < 1e-9)
        );
        assert!(
            matches!(md.get("channel.0.excitation_wavelength"), Some(MetadataValue::Float(v)) if (*v - 488.0).abs() < 1e-9)
        );
        assert!(
            matches!(md.get("channel.1.emission_wavelength"), Some(MetadataValue::Float(v)) if (*v - 610.0).abs() < 1e-9)
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn mias_template_and_companion_metadata_are_indexed() {
        let dir = tmp_dir("mias_companions");
        let plate = dir.join("001-Barcode");
        let results = plate.join("results");
        let batch = dir.join("Batchresults");
        std::fs::create_dir_all(&results).unwrap();
        std::fs::create_dir_all(&batch).unwrap();
        let template = plate.join("Nugenesistemplate.txt");
        std::fs::write(
            &template,
            "Barcode=BC123\r\nCarrier=Plate A\r\nPixel_X=0.25\r\nPixel_Y=0.5\r\nObjective_ID=Obj-20x\r\nMagnification=20\r\nMode_1=DAPI\r\nMode_2=FITC\r\nDate=02/01/2017\r\nTime=13:45:30\r\nExposure=0.125\r\n",
        )
        .unwrap();
        std::fs::write(batch.join("NEO_Results.txt"), "header\n").unwrap();
        std::fs::write(batch.join("NEO_PlateOutput_001.txt"), "plate\n").unwrap();
        std::fs::write(results.join("Well0001_mode1_z0_t0_detail.txt"), "Label\n").unwrap();
        std::fs::write(results.join("Well0001_mode1_z0_t0_overlay.tif"), "").unwrap();
        std::fs::write(results.join("Well0001_mode1_z0_t0_AllModesOverlay.tif"), "").unwrap();

        let companions = collect_mias_companions(&plate, Some(&dir), Some(1));
        assert_eq!(
            companions.template_file.as_deref(),
            Some(template.as_path())
        );
        assert_eq!(companions.analysis_files.len(), 5);

        let template_meta = parse_mias_template_file(&template).unwrap();
        let mut md = HashMap::new();
        add_mias_companion_metadata(&mut md, &companions.analysis_files, Some(&template), 0);
        apply_mias_template_metadata(&mut md, &template_meta, 3);

        assert!(matches!(
            md.get("mias.analysis_file_count"),
            Some(MetadataValue::Int(5))
        ));
        assert!(matches!(
            md.get("mias.roi_detail_file_count"),
            Some(MetadataValue::Int(1))
        ));
        assert!(matches!(
            md.get("mias.roi_overlay_file_count"),
            Some(MetadataValue::Int(1))
        ));
        assert!(matches!(
            md.get("mias.mask_overlay_file_count"),
            Some(MetadataValue::Int(1))
        ));
        assert_eq!(
            md.get("channel.1.name").map(|v| v.to_string()).as_deref(),
            Some("FITC")
        );
        assert!(
            matches!(md.get("physical_size_x"), Some(MetadataValue::Float(v)) if (*v - 0.25).abs() < 1e-9)
        );
        assert!(
            matches!(md.get("plane.2.exposure_time"), Some(MetadataValue::Float(v)) if (*v - 0.125).abs() < 1e-9)
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn mias_all_modes_overlay_colormap_sets_java_channel_color() {
        let dir = tmp_dir("mias_overlay_color");
        let red = dir.join("Well0001_mode1_z0_t0_AllModesOverlay.tif");
        let green = dir.join("Well0001_mode2_z0_t0_AllModesOverlay.tif");
        let tied = dir.join("Well0001_mode3_z0_t0_AllModesOverlay.tif");
        std::fs::write(&red, build_palette_tiff(&[65535, 0, 0, 0, 0, 0])).unwrap();
        std::fs::write(&green, build_palette_tiff(&[0, 0, 65535, 0, 0, 0])).unwrap();
        std::fs::write(&tied, build_palette_tiff(&[0, 0, 0, 0, 0, 0])).unwrap();

        assert_eq!(
            mias_channel_color_from_overlay(&red),
            Some(pack_ome_rgba(255, 0, 0, 255))
        );
        assert_eq!(
            mias_channel_color_from_overlay(&green),
            Some(pack_ome_rgba(0, 255, 0, 255))
        );
        assert_eq!(
            mias_channel_color_from_overlay(&tied),
            Some(pack_ome_rgba(0, 0, 0, 255))
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn mias_overlay_channel_color_projects_to_ome_channels() {
        let dir = tmp_dir("mias_overlay_ome_color");
        let overlay = dir.join("Well0001_mode2_z0_t0_AllModesOverlay.tif");
        std::fs::write(&overlay, build_palette_tiff(&[0, 0, 256, 0, 65535, 0])).unwrap();
        let analysis_files = vec![MiasAnalysisFile {
            filename: overlay,
            plate: 0,
            well: 0,
            kind: MiasAnalysisKind::RoiOverlay,
        }];
        let colors = collect_mias_overlay_channel_colors(&analysis_files, 2);
        assert_eq!(colors[0], None);
        assert_eq!(colors[1], Some(pack_ome_rgba(0, 0, 255, 255)));

        let mut meta = ImageMetadata {
            size_x: 10,
            size_y: 10,
            size_z: 1,
            size_c: 2,
            size_t: 1,
            image_count: 2,
            ..ImageMetadata::default()
        };
        add_mias_overlay_channel_colors(&mut meta.series_metadata, &colors);

        let mut ome = crate::common::ome_metadata::OmeMetadata::from_image_metadata(&meta);
        enrich_ome_from_series_metadata(&mut ome, &meta);
        assert_eq!(
            ome.images[0].channels[1].color,
            Some(pack_ome_rgba(0, 0, 255, 255))
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn mias_mask_overlay_projects_java_mask_shapes_with_bin_data() {
        let dir = tmp_dir("mias_mask_overlay");
        let overlay = dir.join("Well0001_mode1_z0_t0_overlay.tif");
        let color_map = [
            0, 65535, 0, 0, // red table
            0, 0, 65535, 0, // green table
            0, 0, 0, 65535, // blue table
        ];
        std::fs::write(
            &overlay,
            build_palette_tiff_pixels(8, 1, &color_map, &[1, 2, 3, 0, 1, 2, 3, 0]),
        )
        .unwrap();
        let analysis_files = vec![MiasAnalysisFile {
            filename: overlay,
            plate: 0,
            well: 0,
            kind: MiasAnalysisKind::MaskOverlay,
        }];
        let mut meta = ImageMetadata {
            size_x: 8,
            size_y: 1,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            image_count: 1,
            ..ImageMetadata::default()
        };
        add_mias_mask_rois(&mut meta.series_metadata, &analysis_files, 0, 8, 1);

        let ome = crate::common::ome_metadata::OmeMetadata::from_image_metadata(&meta);

        assert_eq!(ome.rois.len(), 3);
        assert_eq!(
            ome.images[0].roi_refs,
            vec![
                "ROI:0".to_string(),
                "ROI:1".to_string(),
                "ROI:2".to_string()
            ]
        );
        match &ome.rois[0].shapes[..] {
            [crate::common::ome_metadata::OmeShape::Mask {
                width,
                height,
                stroke_color,
                fill_color,
                bin_data,
                ..
            }] => {
                assert_eq!((*width, *height), (8.0, 1.0));
                assert_eq!(*stroke_color, Some(pack_ome_rgba(255, 0, 0, 255)));
                assert_eq!(*fill_color, Some(pack_ome_rgba(255, 0, 0, 255)));
                assert_eq!(bin_data.as_deref(), Some(&[0x88][..]));
            }
            other => panic!("expected red mask ROI, got {other:?}"),
        }
        match &ome.rois[1].shapes[..] {
            [crate::common::ome_metadata::OmeShape::Mask { bin_data, .. }] => {
                assert_eq!(bin_data.as_deref(), Some(&[0x44][..]));
            }
            other => panic!("expected green mask ROI, got {other:?}"),
        }
        match &ome.rois[2].shapes[..] {
            [crate::common::ome_metadata::OmeShape::Mask { bin_data, .. }] => {
                assert_eq!(bin_data.as_deref(), Some(&[0x22][..]));
            }
            other => panic!("expected blue mask ROI, got {other:?}"),
        }
        let xml = ome.to_ome_xml(&meta);
        assert!(xml.contains(r#"<BinData Length="1">iA==</BinData>"#));
        assert!(xml.contains(r#"<BinData Length="1">RA==</BinData>"#));
        assert!(xml.contains(r#"<BinData Length="1">Ig==</BinData>"#));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn mias_rgb_mask_overlay_splits_channels_like_java() {
        let dir = tmp_dir("mias_rgb_mask_overlay");
        let overlay = dir.join("Well0001_mode1_z0_t0_overlay_rgb.tif");
        let pixels = [
            255, 0, 0, // red
            0, 128, 0, // green
            0, 0, 64, // blue
            9, 9, 9, // grayscale, ignored by Java
            1, 0, 1, // red + blue
            0, 0, 0, // grayscale zero
            5, 5, 5, // grayscale non-zero, ignored by Java
            0, 2, 0, // green
        ];
        std::fs::write(&overlay, build_rgb_tiff_pixels(8, 1, &pixels)).unwrap();

        let masks = mias_masks_from_overlay(&overlay);
        assert_eq!(masks[0].as_deref(), Some(&[0x88][..]));
        assert_eq!(masks[1].as_deref(), Some(&[0x41][..]));
        assert_eq!(masks[2].as_deref(), Some(&[0x28][..]));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn mias_detail_file_projects_java_ellipse_rois() {
        let dir = tmp_dir("mias_detail_rois");
        let detail = dir.join("Well0001_mode2_z003_t004_detail.txt");
        std::fs::write(
            &detail,
            "Summary before table\n\
             Label\tCol\tRow\tCell Diam.\tNucleus Area\n\
             Cell-1\t12.5\t8.25\t6.0\t20\n\
             Cell-2\t4\t5\t2\t7\n\
             Missing diameter\t1\t2\t\t3\n",
        )
        .unwrap();

        let mut md = HashMap::new();
        let count = add_mias_detail_rois(&mut md, &detail, 3);
        assert_eq!(count, 2);
        assert_eq!(
            md.get("roi.3.name").map(|v| v.to_string()).as_deref(),
            Some("Cell-1")
        );
        assert!(matches!(
            md.get("roi.3.x"),
            Some(MetadataValue::Float(v)) if (*v - 12.5).abs() < 1e-9
        ));
        assert!(matches!(
            md.get("roi.3.y"),
            Some(MetadataValue::Float(v)) if (*v - 8.25).abs() < 1e-9
        ));
        assert!(matches!(
            md.get("roi.3.radius_x"),
            Some(MetadataValue::Float(v)) if (*v - 3.0).abs() < 1e-9
        ));
        assert!(matches!(md.get("roi.3.the_t"), Some(MetadataValue::Int(4))));
        assert!(matches!(md.get("roi.3.the_z"), Some(MetadataValue::Int(3))));

        let mut meta = ImageMetadata {
            size_x: 32,
            size_y: 32,
            size_z: 5,
            size_c: 2,
            size_t: 6,
            image_count: 60,
            ..ImageMetadata::default()
        };
        meta.series_metadata = md;
        let ome = crate::common::ome_metadata::OmeMetadata::from_image_metadata(&meta);
        assert_eq!(ome.rois.len(), 2);
        assert_eq!(
            ome.images[0].roi_refs,
            vec!["ROI:3".to_string(), "ROI:4".to_string()]
        );
        assert_eq!(ome.rois[0].name.as_deref(), Some("Cell-1"));
        match &ome.rois[0].shapes[..] {
            [crate::common::ome_metadata::OmeShape::Ellipse {
                x,
                y,
                radius_x,
                radius_y,
                the_t,
                the_z,
                the_c,
            }] => {
                assert!((*x - 12.5).abs() < 1e-9);
                assert!((*y - 8.25).abs() < 1e-9);
                assert!((*radius_x - 3.0).abs() < 1e-9);
                assert!((*radius_y - 3.0).abs() < 1e-9);
                assert_eq!(*the_t, Some(4));
                assert_eq!(*the_z, Some(3));
                assert_eq!(*the_c, None);
            }
            other => panic!("expected one ellipse ROI shape, got {other:?}"),
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn mias_ome_offsets_image_roi_refs_when_combining_series() {
        let mut first_md = HashMap::new();
        first_md.insert(
            "roi.0.shape".into(),
            MetadataValue::String("rectangle".into()),
        );
        first_md.insert("roi.0.x".into(), MetadataValue::Float(1.0));
        first_md.insert("roi.0.y".into(), MetadataValue::Float(2.0));
        first_md.insert("roi.0.width".into(), MetadataValue::Float(3.0));
        first_md.insert("roi.0.height".into(), MetadataValue::Float(4.0));
        first_md.insert("image.roi_ref.0".into(), MetadataValue::Int(0));

        let mut second_md = HashMap::new();
        second_md.insert(
            "roi.0.shape".into(),
            MetadataValue::String("rectangle".into()),
        );
        second_md.insert("roi.0.x".into(), MetadataValue::Float(5.0));
        second_md.insert("roi.0.y".into(), MetadataValue::Float(6.0));
        second_md.insert("roi.0.width".into(), MetadataValue::Float(7.0));
        second_md.insert("roi.0.height".into(), MetadataValue::Float(8.0));
        second_md.insert("image.roi_ref.0".into(), MetadataValue::Int(0));

        let meta = |series_metadata| ImageMetadata {
            size_x: 16,
            size_y: 16,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            image_count: 1,
            series_metadata,
            ..ImageMetadata::default()
        };

        let ome = ome_from_all_mias_series(&[meta(first_md), meta(second_md)]);

        assert_eq!(ome.rois.len(), 2);
        assert_eq!(ome.rois[0].id.as_deref(), Some("ROI:0"));
        assert_eq!(ome.rois[1].id.as_deref(), Some("ROI:1"));
        assert_eq!(ome.images[0].roi_refs, vec!["ROI:0".to_string()]);
        assert_eq!(ome.images[1].roi_refs, vec!["ROI:1".to_string()]);
    }

    #[test]
    fn ome_projection_keeps_channel_count_and_cellworx_channel_fields() {
        let mut meta = ImageMetadata {
            size_x: 10,
            size_y: 10,
            size_z: 1,
            size_c: 2,
            size_t: 1,
            image_count: 2,
            ..ImageMetadata::default()
        };
        meta.series_metadata
            .insert("PhysicalSizeX".into(), MetadataValue::Float(0.25));
        meta.series_metadata
            .insert("PhysicalSizeY".into(), MetadataValue::Float(0.5));
        meta.series_metadata.insert(
            "channel.0.name".into(),
            MetadataValue::String("DAPI".into()),
        );
        meta.series_metadata.insert(
            "channel.1.name".into(),
            MetadataValue::String("FITC".into()),
        );
        meta.series_metadata.insert(
            "channel.1.excitation_wavelength".into(),
            MetadataValue::Float(488.0),
        );
        meta.series_metadata.insert(
            "channel.1.emission_wavelength".into(),
            MetadataValue::Float(525.0),
        );
        meta.series_metadata.insert(
            "channel.1.detector_settings_gain".into(),
            MetadataValue::Float(2.0),
        );

        let mut ome = crate::common::ome_metadata::OmeMetadata::from_image_metadata(&meta);
        enrich_ome_from_series_metadata(&mut ome, &meta);
        let image = &ome.images[0];

        assert_eq!(image.channels.len(), 2);
        assert_eq!(image.channels[0].name.as_deref(), Some("DAPI"));
        assert_eq!(image.channels[1].name.as_deref(), Some("FITC"));
        assert_eq!(image.physical_size_x, Some(0.25));
        assert_eq!(image.physical_size_y, Some(0.5));
        assert_eq!(image.channels[1].excitation_wavelength, Some(488.0));
        assert_eq!(image.channels[1].emission_wavelength, Some(525.0));
        assert_eq!(image.channels[1].detector_settings_gain, Some(2.0));
    }

    #[test]
    fn cellworx_ome_projects_java_plate_well_field_links() {
        let mut reader = CellWorxReader::new();
        reader.htd_path = Some(PathBuf::from("/tmp/PlateAlpha.HTD"));
        reader.well_files = vec![
            vec![None, Some(vec![PathBuf::from("A02_w1.TIF")])],
            vec![Some(vec![PathBuf::from("B01_w1.TIF")]), None],
        ];
        reader.selected_wells = vec![(0, 1), (1, 0)];
        reader.field_count = 2;
        reader.series = (0..4)
            .map(|index| {
                let mut series_metadata = HashMap::new();
                if index < 2 {
                    series_metadata
                        .insert("WellSamplePositionX".into(), MetadataValue::Float(12.5));
                    series_metadata
                        .insert("WellSamplePositionY".into(), MetadataValue::Float(-3.25));
                }
                ImageMetadata {
                    size_x: 10,
                    size_y: 10,
                    size_z: 1,
                    size_c: 1,
                    size_t: 1,
                    image_count: 1,
                    series_metadata,
                    ..ImageMetadata::default()
                }
            })
            .collect();

        let ome = reader.ome_metadata().unwrap();

        assert_eq!(ome.images.len(), 4);
        assert_eq!(ome.plates.len(), 1);
        let plate = &ome.plates[0];
        assert_eq!(plate.name.as_deref(), Some("PlateAlpha"));
        assert_eq!(plate.rows, 2);
        assert_eq!(plate.columns, 2);
        assert_eq!(plate.wells.len(), 4);
        assert!(plate.wells[0].well_samples.is_empty());
        assert_eq!(plate.wells[1].row, 0);
        assert_eq!(plate.wells[1].column, 1);
        assert_eq!(plate.wells[1].well_samples.len(), 2);
        assert_eq!(plate.wells[1].well_samples[0].image_ref, Some(0));
        assert_eq!(plate.wells[1].well_samples[1].image_ref, Some(1));
        assert_eq!(plate.wells[1].well_samples[0].position_x, Some(12.5));
        assert_eq!(plate.wells[1].well_samples[0].position_y, Some(-3.25));
        assert_eq!(plate.wells[1].well_samples[1].position_x, Some(12.5));
        assert_eq!(plate.wells[1].well_samples[1].position_y, Some(-3.25));
        assert_eq!(plate.wells[2].well_samples[0].image_ref, Some(2));
        assert_eq!(plate.wells[2].well_samples[0].position_x, None);
        assert_eq!(plate.wells[2].well_samples[0].position_y, None);
        assert_eq!(ome.images[0].name.as_deref(), Some("Well A02 Field #1"));
        assert_eq!(ome.images[3].name.as_deref(), Some("Well B01 Field #2"));
    }

    #[test]
    fn cellworx_ome_inherits_metamorph_tiff_template_metadata() {
        let mut reader = CellWorxReader::new();
        reader.htd_path = Some(PathBuf::from("/tmp/PlateAlpha.HTD"));
        reader.well_files = vec![vec![Some(vec![PathBuf::from("A01_w1.TIF")])]];
        reader.selected_wells = vec![(0, 0)];
        reader.field_count = 1;
        reader.series = vec![ImageMetadata {
            size_x: 10,
            size_y: 10,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            image_count: 1,
            ..ImageMetadata::default()
        }];

        let mut template = OmeMetadata::from_image_metadata(&reader.series[0]);
        template.images[0].physical_size_x = Some(1.72);
        template.images[0].physical_size_y = Some(1.72);
        template.instruments.push(OmeInstrument {
            id: Some(create_lsid("Instrument", &[0])),
            detectors: vec![OmeDetector {
                id: Some(create_lsid("Detector", &[0, 0])),
                ..Default::default()
            }],
            ..Default::default()
        });
        reader.ome_template = Some(template);

        let ome = reader.ome_metadata().unwrap();

        assert_eq!(ome.images[0].physical_size_x, Some(1.72));
        assert_eq!(ome.images[0].physical_size_y, Some(1.72));
        assert_eq!(ome.images[0].instrument_ref, Some(0));
        assert_eq!(ome.instruments.len(), 1);
        assert_eq!(ome.instruments[0].detectors.len(), 1);
    }

    #[test]
    fn mias_ome_projects_java_plate_well_links() {
        let mut reader = MiasReader::new();
        reader.plate_name = Some("001-BarcodeFromDirectory".into());
        reader.wells = vec![
            MiasWell {
                tiffs: vec![PathBuf::from("Well0001/mode0_z0_t0.tif")],
                size_z: 1,
                size_c: 1,
                size_t: 1,
                dimension_order: DimensionOrder::XYCZT,
                well_number: 0,
            },
            MiasWell {
                tiffs: vec![PathBuf::from("Well0025/mode0_z0_t0.tif")],
                size_z: 1,
                size_c: 1,
                size_t: 1,
                dimension_order: DimensionOrder::XYCZT,
                well_number: 24,
            },
        ];
        reader.series = (0..2)
            .map(|_| ImageMetadata {
                size_x: 10,
                size_y: 10,
                size_z: 1,
                size_c: 1,
                size_t: 1,
                image_count: 1,
                ..ImageMetadata::default()
            })
            .collect();

        let ome = reader.ome_metadata().unwrap();

        assert_eq!(ome.images.len(), 2);
        assert_eq!(ome.plates.len(), 1);
        let plate = &ome.plates[0];
        assert_eq!(plate.columns, 24);
        assert_eq!(plate.rows, 1);
        assert_eq!(plate.wells.len(), 2);
        assert_eq!(plate.wells[0].row, 0);
        assert_eq!(plate.wells[0].column, 0);
        assert_eq!(plate.wells[0].well_samples[0].image_ref, Some(0));
        assert_eq!(plate.wells[1].row, 1);
        assert_eq!(plate.wells[1].column, 0);
        assert_eq!(plate.wells[1].well_samples[0].image_ref, Some(1));
        assert_eq!(ome.images[0].name.as_deref(), Some("Well A1"));
        assert_eq!(ome.images[1].name.as_deref(), Some("Well B1"));
        assert_eq!(plate.name.as_deref(), Some("BarcodeFromDirectory"));
    }

    #[test]
    fn mias_plate_name_uses_java_directory_suffix_after_template_parse() {
        let dir = tmp_dir("mias_plate_name");
        let plate_dir = dir.join("001-DirectoryBarcode");
        std::fs::create_dir_all(&plate_dir).unwrap();
        let template = plate_dir.join("Nugenesistemplate.txt");
        std::fs::write(
            &template,
            "Barcode=TemplateBarcode\r\nCarrier=TemplateCarrier\r\n",
        )
        .unwrap();

        let mut reader = MiasReader::new();
        reader.plate_name = Some("001-DirectoryBarcode".into());
        reader.template_file = Some(template);
        reader.wells = vec![MiasWell {
            tiffs: vec![PathBuf::from("Well0001/mode0_z0_t0.tif")],
            size_z: 1,
            size_c: 1,
            size_t: 1,
            dimension_order: DimensionOrder::XYCZT,
            well_number: 0,
        }];
        reader.series = vec![ImageMetadata {
            size_x: 10,
            size_y: 10,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            image_count: 1,
            ..ImageMetadata::default()
        }];

        let ome = reader.ome_metadata().unwrap();
        assert_eq!(
            ome.plates[0].name.as_deref(),
            Some("DirectoryBarcode"),
            "Java MIASReader overwrites template Carrier/Barcode with the plate directory suffix"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
