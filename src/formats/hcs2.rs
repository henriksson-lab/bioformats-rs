//! HCS (High-Content Screening) format readers — group 2.
//!
//! TIFF-based HCS wrappers and extension-only placeholder readers for
//! various plate/HCS acquisition platforms.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;

// ---------------------------------------------------------------------------
// Macro: thin TIFF wrapper (extension-only detection)
// ---------------------------------------------------------------------------
macro_rules! tiff_wrapper {
    (
        $(#[$attr:meta])*
        pub struct $name:ident;
        extensions: [$($ext:literal),+];
    ) => {
        $(#[$attr])*
        pub struct $name {
            inner: crate::tiff::TiffReader,
        }

        impl $name {
            pub fn new() -> Self {
                $name { inner: crate::tiff::TiffReader::new() }
            }
        }

        impl Default for $name {
            fn default() -> Self { Self::new() }
        }

        impl FormatReader for $name {
            fn is_this_type_by_name(&self, path: &Path) -> bool {
                let ext = path.extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.to_ascii_lowercase());
                matches!(ext.as_deref(), $(Some($ext))|+)
            }

            fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool { false }

            fn set_id(&mut self, path: &Path) -> Result<()> {
                self.inner.close()?;
                self.inner.set_id(path)?;
                for series in self.inner.series_list_mut() {
                    series.metadata.series_metadata.insert(
                        "hcs2.wrapper".to_string(),
                        MetadataValue::String(stringify!($name).to_string()),
                    );
                }
                Ok(())
            }

            fn close(&mut self) -> Result<()> {
                self.inner.close()
            }

            fn series_count(&self) -> usize {
                self.inner.series_count()
            }

            fn set_series(&mut self, s: usize) -> Result<()> {
                if self.inner.series_count() == 0 {
                    return Err(BioFormatsError::NotInitialized);
                }
                self.inner.set_series(s)
            }

            fn series(&self) -> usize {
                self.inner.series()
            }

            fn metadata(&self) -> &ImageMetadata {
                self.inner.metadata()
            }

            fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
                self.inner.open_bytes(p)
            }

            fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
                self.inner.open_bytes_region(p, x, y, w, h)
            }

            fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
                self.inner.open_thumb_bytes(p)
            }

            fn resolution_count(&self) -> usize {
                self.inner.resolution_count()
            }

            fn set_resolution(&mut self, level: usize) -> Result<()> {
                self.inner.set_resolution(level)
            }
        }
    };
}

// (placeholder_reader macro removed — all former stubs now have real implementations)

// ===========================================================================
// TIFF-based HCS wrappers
// ===========================================================================

// ---------------------------------------------------------------------------
// 1. MetaXpress (Molecular Devices) HCS
// ---------------------------------------------------------------------------
/// MetaXpress (Molecular Devices) HCS: one `.htd` plate-index file plus one or
/// more `.tif` pixel files.
///
/// Faithful port of `MetaxpressTiffReader`, which `extends CellWorxReader`. The
/// HTD parsing and per-well/per-field series assembly are inherited from
/// `CellWorxReader` (here: [`crate::formats::mias::CellWorxReader`], whose
/// file-naming already mirrors `MetaxpressTiffReader.getTiffFiles`). MetaXpress
/// adds only: the `.htd`/`.tif` suffix set, a no-op `parseWellLogFile`
/// (CellWorx's per-well `scan.log` parsing is skipped), and the use of
/// `MetamorphReader` rather than `DeltavisionReader` for pixel reads (both route
/// through the shared TIFF engine here).
pub struct MetaxpressTiffReader {
    inner: crate::formats::mias::CellWorxReader,
}

impl MetaxpressTiffReader {
    pub fn new() -> Self {
        MetaxpressTiffReader {
            inner: crate::formats::mias::CellWorxReader::new(),
        }
    }

    /// Well label as used in MetaXpress TIFF names, e.g. row 0 col 0 -> "A01".
    /// Mirrors `rowLetter + String.format("%02d", col + 1)` in Java
    /// `getTiffFiles` (the `mias::well_name` helper is private to that module).
    fn well_name(row: usize, col: usize) -> String {
        let letter = (b'A' + (row as u8 % 26)) as char;
        format!("{}{:02}", letter, col + 1)
    }

    /// Faithful port of the `subdirectories` branch of
    /// `MetaxpressTiffReader.getTiffFiles` for a single well. Given the parent
    /// directory, the bare filename prefix `base` (Java's `plateName + well`
    /// stripped to the last path segment) and the parsed `nTimepoints`/`zSteps`,
    /// walk `TimePoint_<i+1>/ZStep_<z+1>/` (falling back to the `TimePoint_<i+1>`
    /// directory itself when `zSteps == 1`) and collect every entry whose name
    /// starts with `base` and does not contain `_thumb` (case-insensitive),
    /// sorted within each directory (`Arrays.sort` on `list(true)`). Returns the
    /// ordered file list that Java writes back into `wellFiles[row][col]`.
    fn collect_subdir_tiff_files(
        parent: &Path,
        base: &str,
        n_timepoints: u32,
        z_steps: u32,
    ) -> Vec<PathBuf> {
        let mut files: Vec<PathBuf> = Vec::new();
        for i in 0..n_timepoints {
            let dir = parent.join(format!("TimePoint_{}", i + 1));
            if !(dir.exists() && dir.is_dir()) {
                continue;
            }
            for z in 0..z_steps {
                let zdir = dir.join(format!("ZStep_{}", z + 1));
                let (scan_dir, list): (PathBuf, Vec<std::ffi::OsString>) =
                    if zdir.exists() && zdir.is_dir() {
                        (zdir.clone(), Self::list_dir_sorted(&zdir))
                    } else if z_steps == 1 {
                        // SizeZ == 1: TIFFs may be in the TimePoint_<t> directory.
                        (dir.clone(), Self::list_dir_sorted(&dir))
                    } else {
                        continue;
                    };
                for f in list {
                    let name = f.to_string_lossy();
                    let path = scan_dir.join(&f);
                    let lower = path.to_string_lossy().to_ascii_lowercase();
                    // Java: f.startsWith(base) && path.toLowerCase not _thumb.
                    if name.starts_with(base) && !lower.contains("_thumb") {
                        files.push(path);
                    }
                }
            }
        }
        files
    }

    /// `Location.list(true)` analogue: directory entry file-names sorted
    /// ascending (matching the `Arrays.sort(zList)` in Java).
    fn list_dir_sorted(dir: &Path) -> Vec<std::ffi::OsString> {
        let mut names: Vec<std::ffi::OsString> = match std::fs::read_dir(dir) {
            Ok(rd) => rd.flatten().map(|e| e.file_name()).collect(),
            Err(_) => Vec::new(),
        };
        names.sort();
        names
    }

    /// Resolve the HTD path from the dataset id (the id is either the `.htd`
    /// itself or a member `.tif`). Mirrors the resolution in
    /// `CellWorxReader.find_htd` without reaching into that private helper.
    fn resolve_htd(path: &Path) -> Option<PathBuf> {
        let is_htd = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("htd"))
            .unwrap_or(false);
        if is_htd {
            return path.exists().then(|| path.to_path_buf());
        }
        if let Some(parent) = path.parent() {
            if let Ok(rd) = std::fs::read_dir(parent) {
                let mut paths: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
                paths.sort();
                for p in paths {
                    if p.extension()
                        .and_then(|x| x.to_str())
                        .map(|x| x.eq_ignore_ascii_case("htd"))
                        .unwrap_or(false)
                    {
                        return Some(p);
                    }
                }
            }
        }
        None
    }

    /// Does a `TimePoint_*` directory exist beside the HTD? Java only reaches the
    /// `subdirectories` branch after the flat naming finds nothing; we use this
    /// as the structural gate before retrying the inner assembly via the hook.
    fn has_timepoint_layout(path: &Path) -> Option<PathBuf> {
        let htd = Self::resolve_htd(path)?;
        let parent = htd.parent()?.to_path_buf();
        let found = std::fs::read_dir(&parent)
            .ok()?
            .flatten()
            .any(|e| e.file_name().to_string_lossy().starts_with("TimePoint_"));
        found.then_some(htd)
    }
}

impl Default for MetaxpressTiffReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for MetaxpressTiffReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        // Java: checkSuffix(name, "htd") || (open && foundHTDFile(name)).
        // The dataset id is the `.htd`; `.tif` is accepted as a member file.
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("htd") | Some("tif"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        // CellWorxReader inherits the HTD parse + well/field series assembly.
        // MetaXpress overrides `parseWellLogFile` to a no-op, which CellWorxReader
        // already skips here (no per-well scan.log parsing on the MetaXpress path).
        match self.inner.set_id(path) {
            Ok(()) => Ok(()),
            Err(e) => {
                // Java `MetaxpressTiffReader.getTiffFiles` falls back to a nested
                // TimePoint_<t>/ZStep_<z> directory layout when the flat naming
                // resolves no files on disk (the delegate errors in exactly that
                // case). Java writes that resolved list into wellFiles[row][col]
                // and lets the normal CellWorx series assembly proceed; we mirror
                // that by re-running the assembly through the CellWorxReader hook
                // with a per-well subdir resolver, so the full well x field x T x Z
                // series grid is produced instead of a flat single series.
                let Some(htd) = Self::has_timepoint_layout(path) else {
                    return Err(e);
                };
                let parent = htd
                    .parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| PathBuf::from("."));
                // Per-well resolver: build `base = plateName + well`, stripped to
                // its last path segment (Java: base.substring(lastIndexOf(sep)+1)),
                // then walk TimePoint/ZStep collecting that well's TIFFs.
                let mut resolver =
                    |row: usize, col: usize, dims: &crate::formats::mias::WellResolveDims| {
                        let base = format!("{}{}", dims.plate, Self::well_name(row, col));
                        let file_base = Path::new(&base)
                            .file_name()
                            .map(|f| f.to_string_lossy().into_owned())
                            .unwrap_or(base);
                        Self::collect_subdir_tiff_files(
                            &parent,
                            &file_base,
                            dims.n_timepoints,
                            dims.z_steps,
                        )
                    };
                self.inner.set_id_with_resolver(path, &mut resolver)
            }
        }
    }

    fn close(&mut self) -> Result<()> {
        self.inner.close()
    }

    fn series_count(&self) -> usize {
        self.inner.series_count()
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        self.inner.set_series(s)
    }

    fn series(&self) -> usize {
        self.inner.series()
    }

    fn metadata(&self) -> &ImageMetadata {
        self.inner.metadata()
    }

    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        self.inner.open_bytes(p)
    }

    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        self.inner.open_bytes_region(p, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        self.inner.open_thumb_bytes(p)
    }

    fn resolution_count(&self) -> usize {
        self.inner.resolution_count()
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        self.inner.set_resolution(level)
    }
}

// ---------------------------------------------------------------------------
// 2. SimplePCI / HCImage
// ---------------------------------------------------------------------------
/// SimplePCI/HCImage TIFF (`.tif`).
pub struct SimplePciTiffReader {
    inner: crate::tiff::TiffReader,
}

impl SimplePciTiffReader {
    pub fn new() -> Self {
        SimplePciTiffReader {
            inner: crate::tiff::TiffReader::new(),
        }
    }

    fn enrich_metadata(&mut self) {
        let desc = {
            let series = self.inner.series_list();
            if series.is_empty() {
                return;
            }
            series[0]
                .metadata
                .series_metadata
                .get("ImageDescription")
                .and_then(|v| {
                    if let MetadataValue::String(s) = v {
                        Some(s.clone())
                    } else {
                        None
                    }
                })
        };
        let Some(desc) = desc else { return };

        let vendor = parse_simplepci_description(&desc);
        if vendor.is_empty() {
            return;
        }

        for series in self.inner.series_list_mut() {
            for (key, value) in &vendor {
                series
                    .metadata
                    .series_metadata
                    .insert(key.clone(), value.clone());
            }
        }
    }

    /// Faithful port of `SimplePCITiffReader.initStandardMetadata()` +
    /// `initMetadataStore()`. Only runs for genuine Hamamatsu/SimplePCI data,
    /// where the first IFD's comment starts with the magic string
    /// `"Created by Hamamatsu Inc."`. The comment carries an INI body (after the
    /// magic-string line and a date line) describing the microscope, capture
    /// device, capture channels and calibration. This populates the
    /// format-specific OME-level scalars (objective magnification/immersion,
    /// camera, binning, per-channel exposure times, physical pixel size,
    /// bits-per-pixel) and adjusts SizeC/imageCount the way Java does.
    fn init_standard_metadata_java(&mut self) {
        let desc = {
            let series = self.inner.series_list();
            series.first().and_then(|s| {
                s.metadata
                    .series_metadata
                    .get("ImageDescription")
                    .and_then(|v| match v {
                        MetadataValue::String(s) => Some(s.clone()),
                        _ => None,
                    })
            })
        };
        let Some(comment) = desc else { return };
        // `SimplePCITiffReader.isThisType`: comment.trim() must start with magic.
        if !comment.trim_start().starts_with(SIMPLEPCI_MAGIC_STRING) {
            return;
        }

        // remove magic string (everything up to and including the first '\n')
        let mut data = match comment.find('\n') {
            Some(i) => comment[i + 1..].to_string(),
            None => return,
        };

        // first line of the remainder is the acquisition date
        let date = match data.find('\n') {
            Some(i) => {
                let d = data[..i].to_string();
                data = data[i + 1..].to_string();
                d
            }
            None => return,
        };
        let date = simplepci_format_date(&date);

        // Java: data.replaceAll("ReadFromDoc", "")
        data = data.replace("ReadFromDoc", "");

        let ini = simplepci_parse_ini(&data);

        // -- MICROSCOPE --
        let mut magnification: Option<f64> = None;
        let mut immersion: Option<String> = None;
        if let Some(table) = ini.table(" MICROSCOPE ") {
            if let Some(objective) = table.get("Objective") {
                // Java: int space = objective.indexOf(' ');
                if let Some(space) = objective.find(' ') {
                    // magnification = parseDouble(objective.substring(0, space - 1))
                    if space >= 1 {
                        magnification = objective[..space - 1].trim().parse::<f64>().ok();
                    }
                    immersion = Some(objective[space + 1..].to_string());
                }
            }
        }

        // -- CAPTURE DEVICE -- (binning / camera / bit depth) is mandatory in Java
        let mut bits_per_pixel: Option<u8> = None;
        let mut binning: Option<String> = None;
        let mut camera_type: Option<String> = None;
        let mut camera_name: Option<String> = None;
        if let Some(table) = ini.table(" CAPTURE DEVICE ") {
            if let Some(b) = table.get("Binning") {
                binning = Some(format!("{b}x{b}"));
            }
            camera_type = table.get("Camera Type").map(|s| s.to_string());
            camera_name = table.get("Camera Name").map(|s| s.to_string());
            if let Some(display_depth) = table.get("Display Depth") {
                bits_per_pixel = display_depth.trim().parse::<u8>().ok();
            } else if let Some(bit_depth) = table.get("Bit Depth") {
                // strip a trailing "-bit" suffix, then parse
                let suffix = "-bit";
                if bit_depth.len() > suffix.len() {
                    let trimmed = &bit_depth[..bit_depth.len() - suffix.len()];
                    bits_per_pixel = trimmed.trim().parse::<u8>().ok();
                }
            }
        }

        let size_c = self
            .inner
            .series_list()
            .first()
            .map(|s| s.metadata.size_c.max(1))
            .unwrap_or(1);

        // -- CAPTURE -- per-channel exposure times (microseconds)
        let mut exposure_times: Vec<Option<f64>> = Vec::new();
        if let Some(table) = ini.table(" CAPTURE ") {
            let mut index = 1u32;
            for _ in 0..size_c {
                if table.get(&format!("c_Filter{index}")).is_some() {
                    exposure_times.push(
                        table
                            .get(&format!("c_Expos{index}"))
                            .and_then(|v| v.trim().parse::<f64>().ok()),
                    );
                }
                index += 1;
            }
        }

        // -- CALIBRATION -- physical pixel size (factor)
        let mut scaling: Option<f64> = None;
        if let Some(table) = ini.table(" CALIBRATION ") {
            scaling = table.get("factor").and_then(|v| v.trim().parse::<f64>().ok());
        }

        // CUSTOM_BITS (TIFF tag 65531) overrides the bit depth, if present.
        if let Some(custom) = self
            .inner
            .ifd(0)
            .and_then(|ifd| ifd.get_u32(SIMPLEPCI_CUSTOM_BITS))
        {
            bits_per_pixel = Some(custom as u8);
        }

        // Apply to metadata: Java sets m.imageCount *= sizeC; m.rgb = false; and
        // the bit depth. Channels become separate planes rather than RGB samples.
        for series in self.inner.series_list_mut() {
            let m = &mut series.metadata;
            if m.size_c > 1 {
                m.is_rgb = false;
                m.image_count = m.image_count.saturating_mul(m.size_c);
            }
            if let Some(bpp) = bits_per_pixel {
                m.bits_per_pixel = bpp;
            }
            // OME-store equivalents, surfaced as series metadata.
            if let Some(d) = &date {
                m.series_metadata.insert(
                    "simplepci.acquisition_date".into(),
                    MetadataValue::String(d.clone()),
                );
            }
            m.series_metadata.insert(
                "simplepci.image_description".into(),
                MetadataValue::String(SIMPLEPCI_MAGIC_STRING.to_string()),
            );
            if let Some(mag) = magnification {
                m.series_metadata.insert(
                    "simplepci.objective_nominal_magnification".into(),
                    MetadataValue::Float(mag),
                );
            }
            if let Some(im) = &immersion {
                m.series_metadata.insert(
                    "simplepci.objective_immersion".into(),
                    MetadataValue::String(im.clone()),
                );
            }
            if let (Some(ct), Some(cn)) = (&camera_type, &camera_name) {
                m.series_metadata.insert(
                    "simplepci.detector_model".into(),
                    MetadataValue::String(format!("{ct} {cn}")),
                );
            }
            if let Some(b) = &binning {
                m.series_metadata
                    .insert("simplepci.binning".into(), MetadataValue::String(b.clone()));
            }
            if let Some(sc) = scaling {
                m.series_metadata.insert(
                    "simplepci.physical_size_x".into(),
                    MetadataValue::Float(sc),
                );
                m.series_metadata.insert(
                    "simplepci.physical_size_y".into(),
                    MetadataValue::Float(sc),
                );
            }
            // Per-plane exposure (seconds): Java divides the µs value by 1e6.
            for (c, exp) in exposure_times.iter().enumerate() {
                if let Some(e) = exp {
                    m.series_metadata.insert(
                        format!("simplepci.exposure_time_s.{c}"),
                        MetadataValue::Float(e / 1_000_000.0),
                    );
                }
            }
            // Flatten the full INI into metadata, mirroring
            // `metadata.putAll(ini.flattenIntoHashMap())`.
            for (section, key, value) in ini.flatten() {
                let flat_key = if section.trim().is_empty() {
                    key.clone()
                } else {
                    format!("{} {}", section.trim(), key)
                };
                m.series_metadata
                    .insert(flat_key, MetadataValue::String(value.clone()));
            }
        }
    }
}

/// `SimplePCITiffReader.MAGIC_STRING`.
const SIMPLEPCI_MAGIC_STRING: &str = "Created by Hamamatsu Inc.";
/// `SimplePCITiffReader.CUSTOM_BITS` (TIFF tag 65531).
const SIMPLEPCI_CUSTOM_BITS: u16 = 65531;

/// One `[section]` of an INI file: ordered key/value pairs. Mirrors
/// `loci.common.IniTable`.
#[derive(Default)]
struct SimplePciIniTable {
    name: String,
    entries: Vec<(String, String)>,
}

impl SimplePciIniTable {
    fn get(&self, key: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

/// Parsed INI file: a list of named tables. Mirrors `loci.common.IniList`.
struct SimplePciIni {
    tables: Vec<SimplePciIniTable>,
}

impl SimplePciIni {
    fn table(&self, name: &str) -> Option<&SimplePciIniTable> {
        self.tables.iter().find(|t| t.name == name)
    }

    /// Mirror of `IniList.flattenIntoHashMap`: every (section, key, value).
    fn flatten(&self) -> Vec<(String, String, String)> {
        let mut out = Vec::new();
        for t in &self.tables {
            for (k, v) in &t.entries {
                out.push((t.name.clone(), k.clone(), v.clone()));
            }
        }
        out
    }
}

/// Faithful port of `loci.common.IniParser.parseINI` with `;` comment delimiter.
/// Section headers are `[name]`; the bracket *contents* (verbatim, including the
/// leading/trailing spaces SimplePCI uses, e.g. `" MICROSCOPE "`) become the
/// table name. Key/value pairs are split on the first `=`.
fn simplepci_parse_ini(data: &str) -> SimplePciIni {
    let mut tables: Vec<SimplePciIniTable> = Vec::new();
    let mut current: Option<SimplePciIniTable> = None;
    for raw in data.lines() {
        // Strip `;` comments.
        let line = match raw.find(';') {
            Some(i) => &raw[..i],
            None => raw,
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(inner) = trimmed
            .strip_prefix('[')
            .and_then(|s| s.strip_suffix(']'))
        {
            if let Some(t) = current.take() {
                tables.push(t);
            }
            current = Some(SimplePciIniTable {
                name: inner.to_string(),
                entries: Vec::new(),
            });
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim().to_string();
            let value = value.trim().to_string();
            if let Some(t) = current.as_mut() {
                t.entries.push((key, value));
            } else {
                current = Some(SimplePciIniTable {
                    name: String::new(),
                    entries: vec![(key, value)],
                });
            }
        }
    }
    if let Some(t) = current.take() {
        tables.push(t);
    }
    SimplePciIni { tables }
}

/// Reformat the SimplePCI acquisition-date string (`EEE, dd MMM yyyy HH:mm:ss z`)
/// into ISO-8601 (`yyyy-MM-dd'T'HH:mm:ss`), mirroring
/// `DateTools.formatDate(date, DATE_FORMAT)`. Returns `None` if it cannot be
/// parsed (Java also returns null in that case).
fn simplepci_format_date(date: &str) -> Option<String> {
    // Expected: "Wed, 21 Mar 2007 14:05:09 GMT"
    let date = date.trim();
    let rest = date.split_once(',').map(|(_, r)| r.trim()).unwrap_or(date);
    let mut parts = rest.split_whitespace();
    let day: u32 = parts.next()?.parse().ok()?;
    let month = match parts.next()? {
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
    let year: u32 = parts.next()?.parse().ok()?;
    let time = parts.next()?;
    let mut tparts = time.split(':');
    let h: u32 = tparts.next()?.parse().ok()?;
    let mi: u32 = tparts.next()?.parse().ok()?;
    let s: u32 = tparts.next()?.parse().ok()?;
    Some(format!(
        "{year:04}-{month:02}-{day:02}T{h:02}:{mi:02}:{s:02}"
    ))
}

impl Default for SimplePciTiffReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for SimplePciTiffReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("tif"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.inner.close()?;
        self.inner.set_id(path)?;
        for series in self.inner.series_list_mut() {
            series.metadata.series_metadata.insert(
                "hcs2.wrapper".to_string(),
                MetadataValue::String("SimplePciTiffReader".to_string()),
            );
        }
        self.enrich_metadata();
        self.init_standard_metadata_java();
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.inner.close()
    }

    fn series_count(&self) -> usize {
        self.inner.series_count()
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.inner.series_count() == 0 {
            return Err(BioFormatsError::NotInitialized);
        }
        self.inner.set_series(s)
    }

    fn series(&self) -> usize {
        self.inner.series()
    }

    fn metadata(&self) -> &ImageMetadata {
        self.inner.metadata()
    }

    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        let (w, h) = {
            let m = self.inner.metadata();
            (m.size_x, m.size_y)
        };
        self.open_bytes_region(p, 0, 0, w, h)
    }

    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        // Faithful port of `SimplePCITiffReader.openBytes`. When the file has a
        // single channel, the raw TIFF plane is returned unchanged. For
        // multi-channel SimplePCI data the underlying IFDs store the channels as
        // interleaved samples within one plane; Java reads that plane via the
        // `MinimalTiffReader` delegate at logical index `no / getSizeC()` and
        // then splits the requested channel out with `ImageTools.splitChannels`.
        let (size_c, pixel_type, is_interleaved, size_z, size_t, dim_order) = {
            let m = self.inner.metadata();
            (
                m.size_c.max(1),
                m.pixel_type,
                m.is_interleaved,
                m.size_z,
                m.size_t,
                m.dimension_order,
            )
        };
        if size_c == 1 {
            return self.inner.open_bytes_region(p, x, y, w, h);
        }

        // delegate.openBytes(no / getSizeC(), x, y, w, h): the raw (RGB-sample)
        // IFD plane. `self.inner` still maps logical plane k -> IFD k, and the
        // raw IFD read returns the full `size_c`-sample interleaved plane.
        let raw = self.inner.open_bytes_region(p / size_c, x, y, w, h)?;

        let bpp = pixel_type.bytes_per_sample();
        // c = getZCTCoords(no)[1]
        let (_z, c, _t) = get_zct_coords(dim_order, size_z, size_c, size_t, p);
        let channel_length = (w as usize) * (h as usize) * bpp;
        Ok(split_channels(
            &raw,
            c as usize,
            size_c as usize,
            bpp,
            false,
            is_interleaved,
            channel_length,
        ))
    }

    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        self.inner.open_thumb_bytes(p)
    }

    fn resolution_count(&self) -> usize {
        self.inner.resolution_count()
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        self.inner.set_resolution(level)
    }
}

/// Faithful port of `loci.formats.ImageTools.splitChannels(array, rtn, index, c,
/// bytes, reverse, interleaved, channelLength)` for the `rtn == null` case used
/// by `SimplePCITiffReader`. Extracts channel `index` from a `c`-channel array.
fn split_channels(
    array: &[u8],
    index: usize,
    c: usize,
    bytes: usize,
    reverse: bool,
    interleaved: bool,
    channel_length: usize,
) -> Vec<u8> {
    if c == 1 {
        return array.to_vec();
    }
    let mut rtn = vec![0u8; array.len() / c];
    let index = if reverse { c - index - 1 } else { index };

    if !interleaved {
        // System.arraycopy(array, channelLength * index, rtn, 0, channelLength)
        let start = channel_length * index;
        let len = channel_length.min(rtn.len());
        if start + len <= array.len() {
            rtn[..len].copy_from_slice(&array[start..start + len]);
        }
    } else {
        let mut next = 0usize;
        let mut i = 0usize;
        while i < array.len() {
            for k in 0..bytes {
                if next < rtn.len() {
                    let src = i + index * bytes + k;
                    if src < array.len() {
                        rtn[next] = array[src];
                    }
                }
                next += 1;
            }
            i += c * bytes;
        }
    }
    rtn
}

fn parse_simplepci_description(desc: &str) -> HashMap<String, MetadataValue> {
    let lower = desc.to_ascii_lowercase();
    if !lower.contains("simplepci") && !lower.contains("simple pci") && !lower.contains("hcimage") {
        return HashMap::new();
    }

    let mut vendor = HashMap::new();
    let software = match (
        lower.contains("simplepci") || lower.contains("simple pci"),
        lower.contains("hcimage"),
    ) {
        (true, true) => "SimplePCI HCImage",
        (true, false) => "SimplePCI",
        (false, true) => "HCImage",
        (false, false) => unreachable!(),
    };
    vendor.insert(
        "simplepci.software".to_string(),
        MetadataValue::String(software.to_string()),
    );

    insert_simplepci_xml_metadata(&mut vendor, desc);

    for line in desc.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('[') || line.starts_with('<') {
            continue;
        }
        let Some((key, value)) = line.split_once('=').or_else(|| line.split_once(':')) else {
            continue;
        };
        let key = simplepci_metadata_key(key);
        let value = value.trim().trim_matches('"');
        if key.is_empty() || value.is_empty() {
            continue;
        }
        insert_parsed_hcs_metadata_value(&mut vendor, format!("simplepci.{key}"), value);
    }

    vendor
}

fn simplepci_metadata_key(key: &str) -> String {
    key.trim()
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>()
        .split('_')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("_")
}

fn insert_parsed_hcs_metadata_value(
    metadata: &mut HashMap<String, MetadataValue>,
    key: String,
    value: &str,
) {
    if let Ok(f) = value.parse::<f64>() {
        metadata.insert(key, MetadataValue::Float(f));
    } else {
        metadata.insert(key, MetadataValue::String(value.to_string()));
    }
}

#[derive(Debug, Clone)]
struct HcsXmlTag {
    name: String,
    attrs: HashMap<String, String>,
    start_offset: usize,
    body_start: usize,
    self_closing: bool,
}

fn hcs_xml_scan_tags(xml: &str) -> Vec<HcsXmlTag> {
    let bytes = xml.as_bytes();
    let mut tags = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        if xml[i..].starts_with("<!--") {
            if let Some(end) = xml[i..].find("-->") {
                i += end + 3;
            } else {
                break;
            }
            continue;
        }
        if bytes.get(i + 1) == Some(&b'/')
            || bytes.get(i + 1) == Some(&b'?')
            || bytes.get(i + 1) == Some(&b'!')
        {
            if let Some(end) = xml[i..].find('>') {
                i += end + 1;
            } else {
                break;
            }
            continue;
        }

        let mut j = i + 1;
        let mut in_quote = 0u8;
        while j < bytes.len() {
            let c = bytes[j];
            if in_quote != 0 {
                if c == in_quote {
                    in_quote = 0;
                }
            } else if c == b'"' || c == b'\'' {
                in_quote = c;
            } else if c == b'>' {
                break;
            }
            j += 1;
        }
        if j >= bytes.len() {
            break;
        }

        let inner = &xml[i + 1..j];
        let self_closing = inner.trim_end().ends_with('/');
        let inner_trim = inner.trim_end().trim_end_matches('/');
        let name_end = inner_trim
            .find(|c: char| c.is_whitespace())
            .unwrap_or(inner_trim.len());
        let name = inner_trim[..name_end].to_string();
        let attrs = hcs_xml_parse_attrs(&inner_trim[name_end..]);
        tags.push(HcsXmlTag {
            name,
            attrs,
            start_offset: i,
            body_start: j + 1,
            self_closing,
        });
        i = j + 1;
    }
    tags
}

fn hcs_xml_parse_attrs(s: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        while i < bytes.len() && (bytes[i] as char).is_whitespace() {
            i += 1;
        }
        let key_start = i;
        while i < bytes.len() && bytes[i] != b'=' && !(bytes[i] as char).is_whitespace() {
            i += 1;
        }
        let key = s[key_start..i].trim().to_string();
        while i < bytes.len() && (bytes[i] as char).is_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b'=' {
            if key.is_empty() {
                break;
            }
            continue;
        }
        i += 1;
        while i < bytes.len() && (bytes[i] as char).is_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let quote = bytes[i];
        if quote == b'"' || quote == b'\'' {
            i += 1;
            let val_start = i;
            while i < bytes.len() && bytes[i] != quote {
                i += 1;
            }
            if !key.is_empty() {
                map.insert(key, hcs_xml_unescape(&s[val_start..i]));
            }
            i += 1;
        } else {
            let val_start = i;
            while i < bytes.len() && !(bytes[i] as char).is_whitespace() {
                i += 1;
            }
            if !key.is_empty() {
                map.insert(key, hcs_xml_unescape(&s[val_start..i]));
            }
        }
    }
    map
}

fn hcs_xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

fn hcs_xml_element_text(xml: &str, tag: &HcsXmlTag) -> Option<String> {
    if tag.self_closing {
        return None;
    }
    let rest = &xml[tag.body_start..];
    let end = rest.find('<')?;
    let text = hcs_xml_unescape(rest[..end].trim());
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

fn hcs_xml_matching_end_offset(xml: &str, tag: &HcsXmlTag) -> Option<usize> {
    if tag.self_closing {
        return Some(tag.body_start);
    }
    let mut i = tag.body_start;
    let mut depth = 1usize;
    while i < xml.len() {
        let rel = xml[i..].find('<')?;
        i += rel;
        let end = xml[i..].find('>')?;
        let inner = &xml[i + 1..i + end].trim();
        if let Some(close_name) = inner.strip_prefix('/') {
            let close_name = close_name.split_whitespace().next().unwrap_or("");
            if close_name.eq_ignore_ascii_case(&tag.name) {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(i + end + 1);
                }
            }
        } else if !inner.starts_with('!') && !inner.starts_with('?') {
            let self_closing = inner.trim_end().ends_with('/');
            let start = inner.trim_end().trim_end_matches('/');
            let name_end = start
                .find(|c: char| c.is_whitespace())
                .unwrap_or(start.len());
            if start[..name_end].eq_ignore_ascii_case(&tag.name) && !self_closing {
                depth += 1;
            }
        }
        i += end + 1;
    }
    None
}

fn hcs_key_suffix(name: &str) -> String {
    let mut suffix = String::new();
    let chars: Vec<char> = name.chars().collect();
    for (i, ch) in chars.iter().copied().enumerate() {
        if ch.is_ascii_uppercase() {
            let prev = i.checked_sub(1).and_then(|idx| chars.get(idx)).copied();
            let next = chars.get(i + 1).copied();
            let starts_new_word = prev
                .is_some_and(|p| p.is_ascii_lowercase() || p.is_ascii_digit())
                || (prev.is_some_and(|p| p.is_ascii_uppercase())
                    && next.is_some_and(|n| n.is_ascii_lowercase()));
            if i > 0 && starts_new_word {
                suffix.push('_');
            }
            suffix.push(ch.to_ascii_lowercase());
        } else if ch == ' ' || ch == '-' {
            suffix.push('_');
        } else {
            suffix.push(ch.to_ascii_lowercase());
        }
    }
    simplepci_metadata_key(&suffix)
}

fn hcs_xml_attr_case_insensitive<'a>(
    attrs: &'a HashMap<String, String>,
    name: &str,
) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
        .filter(|v| !v.trim().is_empty())
}

fn insert_simplepci_xml_metadata(metadata: &mut HashMap<String, MetadataValue>, desc: &str) {
    if !desc.contains('<') {
        return;
    }

    let tags = hcs_xml_scan_tags(desc);
    insert_simplepci_hierarchy_scalar_metadata(metadata, desc, &tags);

    let mut scalar_count = 0usize;
    for tag in tags.iter().take(128) {
        if scalar_count >= 256 {
            break;
        }
        let tag_key = hcs_key_suffix(&tag.name);
        if tag_key.is_empty() {
            continue;
        }

        let mut attr_names: Vec<_> = tag.attrs.keys().map(String::as_str).collect();
        attr_names.sort_unstable_by_key(|name| name.to_ascii_lowercase());
        for attr in attr_names.into_iter().take(32) {
            if scalar_count >= 256 {
                break;
            }
            let Some(value) = hcs_xml_attr_case_insensitive(&tag.attrs, attr) else {
                continue;
            };
            let attr_key = hcs_key_suffix(attr);
            if attr_key.is_empty() {
                continue;
            }
            insert_parsed_hcs_metadata_value(
                metadata,
                format!("simplepci.xml.{tag_key}.{attr_key}"),
                value,
            );
            insert_simplepci_xml_alias(metadata, &tag_key, &attr_key, value);
            scalar_count += 1;
        }

        if scalar_count < 256 {
            if let Some(text) = hcs_xml_element_text(desc, tag) {
                let text: String = text.chars().take(4096).collect();
                insert_parsed_hcs_metadata_value(
                    metadata,
                    format!("simplepci.xml.{tag_key}.text"),
                    &text,
                );
                insert_simplepci_xml_text_alias(metadata, &tag_key, &text);
                scalar_count += 1;
            }
        }
    }

    if scalar_count > 0 {
        metadata.insert(
            "simplepci.xml_scalar_count".into(),
            MetadataValue::Int(scalar_count as i64),
        );
    }
}

fn insert_simplepci_xml_alias(
    metadata: &mut HashMap<String, MetadataValue>,
    tag_key: &str,
    attr_key: &str,
    value: &str,
) {
    let alias = match (tag_key, attr_key) {
        (_, "exposure_time") => Some("exposure_time"),
        (_, "objective_magnification") => Some("objective_magnification"),
        ("objective", "magnification") => Some("objective_magnification"),
        ("channel", "name") | ("wavelength", "channel_name") => Some("channel_name"),
        (_, "channel_name") => Some("channel_name"),
        (_, "wavelength") => Some("wavelength"),
        (_, "well") | ("well", "id") | ("well", "name") => Some("well"),
        (_, "site") | ("site", "id") | ("field", "id") => Some("site"),
        _ => None,
    };
    if let Some(alias) = alias {
        let key = format!("simplepci.{alias}");
        if !metadata.contains_key(&key) {
            insert_parsed_hcs_metadata_value(metadata, key, value);
        }
    }
}

fn insert_simplepci_xml_text_alias(
    metadata: &mut HashMap<String, MetadataValue>,
    tag_key: &str,
    value: &str,
) {
    let alias = match tag_key {
        "exposure_time" => Some("exposure_time"),
        "objective_magnification" => Some("objective_magnification"),
        "channel_name" => Some("channel_name"),
        "wavelength" => Some("wavelength"),
        "well" | "well_id" => Some("well"),
        "site" | "site_id" | "field" | "field_id" => Some("site"),
        _ => None,
    };
    if let Some(alias) = alias {
        let key = format!("simplepci.{alias}");
        if !metadata.contains_key(&key) {
            insert_parsed_hcs_metadata_value(metadata, key, value);
        }
    }
}

fn insert_simplepci_hierarchy_scalar_metadata(
    metadata: &mut HashMap<String, MetadataValue>,
    xml: &str,
    tags: &[HcsXmlTag],
) {
    #[derive(Clone)]
    struct StackNode {
        suffix: String,
        end_offset: usize,
        interesting: bool,
    }

    let mut stack: Vec<StackNode> = Vec::new();
    let mut node_count = 0usize;
    let mut scalar_count = 0usize;

    for tag in tags {
        while stack
            .last()
            .is_some_and(|node| tag.start_offset >= node.end_offset)
        {
            stack.pop();
        }

        let suffix = hcs_key_suffix(&tag.name);
        if suffix.is_empty() {
            continue;
        }
        let interesting = simplepci_is_hierarchy_object_tag(&suffix);
        let in_interesting_path = interesting || stack.iter().any(|node| node.interesting);

        if in_interesting_path && !simplepci_is_xml_root_tag(&suffix) {
            let mut scalars: Vec<(String, String)> = Vec::new();

            let mut attr_names: Vec<_> = tag.attrs.keys().map(String::as_str).collect();
            attr_names.sort_unstable_by_key(|name| name.to_ascii_lowercase());
            for attr in attr_names.into_iter().take(32) {
                if let Some(value) = hcs_xml_attr_case_insensitive(&tag.attrs, attr) {
                    scalars.push((hcs_key_suffix(attr), value.to_string()));
                }
            }

            if let Some(text) = hcs_xml_element_text(xml, tag) {
                scalars.push(("text".into(), text.chars().take(4096).collect()));
            }

            if !scalars.is_empty() && node_count < 64 {
                let mut path: Vec<&str> = stack
                    .iter()
                    .filter(|node| node.interesting)
                    .filter(|node| !simplepci_is_xml_root_tag(&node.suffix))
                    .map(|node| node.suffix.as_str())
                    .collect();
                path.push(&suffix);

                let node_key = format!("simplepci.hierarchy.{node_count}");
                metadata.insert(
                    format!("{node_key}.path"),
                    MetadataValue::String(path.join(".")),
                );
                metadata.insert(
                    format!("{node_key}.type"),
                    MetadataValue::String(suffix.clone()),
                );
                metadata.insert(
                    format!("{node_key}.depth"),
                    MetadataValue::Int(path.len() as i64),
                );

                for (key, value) in scalars {
                    if scalar_count >= 256 {
                        break;
                    }
                    if !key.is_empty() {
                        insert_parsed_hcs_metadata_value(
                            metadata,
                            format!("{node_key}.{key}"),
                            &value,
                        );
                        scalar_count += 1;
                    }
                }
                node_count += 1;
            }
        }

        if !tag.self_closing && stack.len() < 8 {
            let end_offset = hcs_xml_matching_end_offset(xml, tag).unwrap_or(xml.len());
            stack.push(StackNode {
                suffix,
                end_offset,
                interesting,
            });
        }

        if node_count >= 64 || scalar_count >= 256 {
            break;
        }
    }

    if node_count > 0 {
        metadata.insert(
            "simplepci.hierarchy.node_count".into(),
            MetadataValue::Int(node_count as i64),
        );
        metadata.insert(
            "simplepci.hierarchy.scalar_count".into(),
            MetadataValue::Int(scalar_count as i64),
        );
    }
}

fn simplepci_is_xml_root_tag(suffix: &str) -> bool {
    matches!(
        suffix,
        "hc_image" | "h_c_image" | "simplepci" | "simple_pci" | "simple_p_c_i"
    )
}

fn simplepci_is_hierarchy_object_tag(suffix: &str) -> bool {
    simplepci_is_xml_root_tag(suffix)
        || matches!(
            suffix,
            "acquisition"
                | "calibration"
                | "camera"
                | "capture"
                | "channel"
                | "channels"
                | "experiment"
                | "field"
                | "filter"
                | "image"
                | "lens"
                | "microscope"
                | "objective"
                | "plane"
                | "sequence"
                | "site"
                | "stage"
                | "time_point"
                | "wavelength"
                | "well"
                | "xy_stage"
                | "z_stage"
        )
}

// ---------------------------------------------------------------------------
// 3. Ionpath MIBI-TOF
// ---------------------------------------------------------------------------

/// Ionpath MIBI-TOF TIFF (`.tif`/`.tiff`).
///
/// Ported from the upstream Java `IonpathMIBITiffReader` (extends
/// `BaseTiffReader`). Each IFD carries a JSON `ImageDescription` with an
/// `image.type` field. IFDs are regrouped into one series per distinct
/// `image.type`; the only type allowed to span >1 IFD is `"SIMS"`, where each
/// extra IFD adds one channel (sizeC/imageCount). Channel IDs/names come from
/// the SIMS IFDs' `channel.mass` / `channel.target`, and `mibi.*` keys from the
/// first SIMS IFD become series metadata. Detection: SOFTWARE tag starts with
/// `"IonpathMIBI"`.
pub struct IonpathMibiTiffReader {
    inner: crate::tiff::TiffReader,
}

/// `IonpathMIBITiffReader.IONPATH_MIBI_SOFTWARE_PREFIX`.
const IONPATH_MIBI_SOFTWARE_PREFIX: &str = "IonpathMIBI";

impl IonpathMibiTiffReader {
    pub fn new() -> Self {
        IonpathMibiTiffReader {
            inner: crate::tiff::TiffReader::new(),
        }
    }

    /// Mirror of `isThisType(RandomAccessInputStream)`: first IFD SOFTWARE tag
    /// must start with "IonpathMIBI".
    fn is_ionpath_software(&self) -> bool {
        self.inner
            .ifd(0)
            .and_then(|ifd| ifd.get_str(crate::tiff::ifd::tag::SOFTWARE))
            .map(|s| s.starts_with(IONPATH_MIBI_SOFTWARE_PREFIX))
            .unwrap_or(false)
    }

    /// Port of `IonpathMIBITiffReader.initStandardMetadata` +
    /// `initMetadataStore`: regroup the per-IFD series into one series per
    /// `image.type`, collapsing SIMS channels, and attach channel/instrument
    /// metadata. Returns an error matching Java's mandatory-description and
    /// single-non-SIMS-per-type contracts.
    fn init_standard_metadata(&mut self) -> Result<()> {
        let ifd_count = self.inner.ifd_count();
        if ifd_count == 0 {
            return Err(BioFormatsError::Format(
                "Ionpath MIBI: no IFDs".to_string(),
            ));
        }

        // seriesTypes: image.type -> series index (insertion order).
        let mut series_types: Vec<(String, usize)> = Vec::new();
        // For each series index: the IFD indices that compose it.
        let mut series_ifds: Vec<Vec<usize>> = Vec::new();
        // Channel (mass, target) collected from SIMS IFDs in IFD order.
        let mut channel_ids: Vec<String> = Vec::new();
        let mut channel_names: Vec<String> = Vec::new();
        // mibi.* keys from the first SIMS IFD's JSON.
        let mut sims_description: Vec<(String, String)> = Vec::new();

        for i in 0..ifd_count {
            let description = self
                .inner
                .ifd(i)
                .and_then(|ifd| ifd.get_str(crate::tiff::ifd::tag::IMAGE_DESCRIPTION))
                .map(|s| s.to_string());
            let Some(description) = description else {
                return Err(BioFormatsError::Format(
                    "Ionpath MIBI: image description is mandatory.".to_string(),
                ));
            };

            let json: serde_json::Value = serde_json::from_str(&description).map_err(|_| {
                BioFormatsError::Format(
                    "Ionpath MIBI: unexpected format in SIMS description JSON.".to_string(),
                )
            })?;
            let image_type = json
                .get("image.type")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    BioFormatsError::Format(
                        "Ionpath MIBI: unexpected format in SIMS description JSON.".to_string(),
                    )
                })?
                .to_string();

            if image_type == "SIMS" {
                let mass = json.get("channel.mass").and_then(|v| v.as_f64());
                let target = json.get("channel.target").and_then(|v| v.as_str());
                let mass_str = mass.map(|m| format_json_double(m)).unwrap_or_default();
                channel_ids.push(mass_str.clone());
                channel_names.push(match target {
                    Some(t) if t != "null" => t.to_string(),
                    _ => mass_str,
                });
            }

            if let Some((_, idx)) = series_types.iter().find(|(t, _)| t == &image_type) {
                if image_type != "SIMS" {
                    return Err(BioFormatsError::Format(
                        "Ionpath MIBI: only type 'SIMS' can have >1 image per file.".to_string(),
                    ));
                }
                let idx = *idx;
                series_ifds[idx].push(i);
            } else {
                let idx = series_types.len();
                series_types.push((image_type.clone(), idx));
                series_ifds.push(vec![i]);

                if image_type == "SIMS" {
                    if let Some(obj) = json.as_object() {
                        for (key, value) in obj {
                            if key.starts_with("mibi.") {
                                sims_description.push((key.clone(), json_value_to_string(value)));
                            }
                        }
                    }
                }
            }
        }

        // Rebuild the series list. The default TIFF parse produced one series
        // per IFD; clone series[ifd0] of each group as a template (to preserve
        // pixel layout for that IFD), then set ifd_indices to the full group.
        let template = self
            .inner
            .series_list()
            .first()
            .cloned()
            .ok_or(BioFormatsError::NotInitialized)?;

        // Snapshot per-IFD (sizeX, sizeY, pixel type, etc.) from each group's
        // first IFD's own series so dimensions stay correct.
        let mut new_series = Vec::with_capacity(series_types.len());
        for (series_index, (image_type, _)) in series_types.iter().enumerate() {
            let group = &series_ifds[series_index];
            let first_ifd = group[0];
            // Find the original single-IFD series whose ifd_indices == [first_ifd].
            let mut s = self
                .inner
                .series_list()
                .iter()
                .find(|s| s.ifd_indices == [first_ifd])
                .cloned()
                .unwrap_or_else(|| template.clone());

            let channel_count = group.len() as u32;
            s.ifd_indices = group.clone();
            s.plane_ifd_indices = Vec::new();
            let m = &mut s.metadata;
            // Java: SIMS sizeC grows by one per extra IFD; imageCount == sizeC.
            // For RGB (non-SIMS) images sizeC stays as the TIFF's own.
            if image_type == "SIMS" {
                m.size_c = channel_count;
                m.is_rgb = false;
                m.is_indexed = false;
            }
            m.size_z = 1;
            m.size_t = 1;
            m.image_count = m.size_c.max(channel_count).max(1) * m.size_z * m.size_t;
            if image_type == "SIMS" {
                m.image_count = channel_count;
            }
            m.dimension_order = DimensionOrder::XYCZT;

            // Image name: "<file> <type>" (initMetadataStore).
            m.series_metadata.insert(
                "hcs2.wrapper".to_string(),
                MetadataValue::String("IonpathMibiTiffReader".to_string()),
            );
            m.series_metadata.insert(
                "image.type".to_string(),
                MetadataValue::String(image_type.clone()),
            );

            if image_type == "SIMS" {
                for (key, value) in &sims_description {
                    m.series_metadata
                        .insert(key.clone(), MetadataValue::String(value.clone()));
                }
                if let Some((_, instrument)) =
                    sims_description.iter().find(|(k, _)| k == "mibi.instrument")
                {
                    m.series_metadata.insert(
                        "InstrumentID".to_string(),
                        MetadataValue::String(format_ionpath_metadata("Instrument", instrument)),
                    );
                }
                if let Some((_, desc)) =
                    sims_description.iter().find(|(k, _)| k == "mibi.description")
                {
                    m.series_metadata.insert(
                        "ImageDescription".to_string(),
                        MetadataValue::String(desc.clone()),
                    );
                }
                for (j, (id, name)) in channel_ids.iter().zip(channel_names.iter()).enumerate() {
                    m.series_metadata.insert(
                        format!("Channel {j} ID"),
                        MetadataValue::String(format_ionpath_metadata("Channel", id)),
                    );
                    m.series_metadata.insert(
                        format!("Channel {j} Name"),
                        MetadataValue::String(format_ionpath_metadata("Target", name)),
                    );
                }
            }

            new_series.push(s);
        }

        self.inner.replace_series(new_series);
        Ok(())
    }
}

/// `IonpathMIBITiffReader.formatMetadata`: `key:value` with whitespace in the
/// value replaced by underscores.
fn format_ionpath_metadata(key: &str, value: &str) -> String {
    let replaced: String = value
        .chars()
        .map(|c| if c.is_whitespace() { '_' } else { c })
        .collect();
    format!("{key}:{replaced}")
}

/// Render a JSON double the way Java's `Double.toString` would for the mass
/// channel IDs (integers keep no decimal unless fractional).
fn format_json_double(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{:.1}", value)
    } else {
        format!("{value}")
    }
}

fn json_value_to_string(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

impl Default for IonpathMibiTiffReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for IonpathMibiTiffReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("tif") | Some("tiff"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.inner.close()?;
        self.inner.set_id(path)?;
        // Only attempt the Ionpath regroup if the SOFTWARE tag matches; for a
        // plain TIFF that lacks the JSON descriptions we leave the default
        // per-IFD series intact rather than erroring.
        if self.is_ionpath_software() {
            self.init_standard_metadata()?;
        } else {
            for series in self.inner.series_list_mut() {
                series.metadata.series_metadata.insert(
                    "hcs2.wrapper".to_string(),
                    MetadataValue::String("IonpathMibiTiffReader".to_string()),
                );
            }
        }
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.inner.close()
    }

    fn series_count(&self) -> usize {
        self.inner.series_count()
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.inner.series_count() == 0 {
            return Err(BioFormatsError::NotInitialized);
        }
        self.inner.set_series(s)
    }

    fn series(&self) -> usize {
        self.inner.series()
    }

    fn metadata(&self) -> &ImageMetadata {
        self.inner.metadata()
    }

    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        self.inner.open_bytes(p)
    }

    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        self.inner.open_bytes_region(p, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        self.inner.open_thumb_bytes(p)
    }

    fn resolution_count(&self) -> usize {
        self.inner.resolution_count()
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        self.inner.set_resolution(level)
    }
}

// ---------------------------------------------------------------------------
// 4. Beckman Coulter MIAS
// ---------------------------------------------------------------------------
tiff_wrapper! {
    /// Beckman Coulter MIAS TIFF (`.tif`).
    pub struct MiasTiffReader;
    extensions: ["tif"];
}

// ---------------------------------------------------------------------------
// 5. Trestle whole-slide
// ---------------------------------------------------------------------------

/// Trestle whole-slide TIFF (`.tif`).
///
/// Ported from the upstream Java `TrestleReader`. Pixel I/O is delegated to the
/// inner `TiffReader`; this wrapper additionally translates
/// `TrestleReader.initStandardMetadata`, which parses the first IFD's comment
/// (a `;`-separated list of `key=value` pairs) into global metadata.
pub struct TrestleReader {
    inner: crate::tiff::TiffReader,
    /// `overlaps[coreIndex*2 + {0,1}]` — the per-tile X/Y pixel overlaps parsed
    /// from the first IFD comment's `OverlapsXY` entry. Indexed by core (=
    /// resolution) index, mirroring Java's `overlaps[getCoreIndex()*2 + …]`.
    overlaps: Vec<i64>,
    /// IFD index backing each resolution level, in level order (Java
    /// `ifds.get(getCoreIndex())`). Empty when the regroup did not run (single
    /// IFD), in which case pixel reads delegate straight to the inner reader.
    level_ifd: Vec<usize>,
}

impl TrestleReader {
    pub fn new() -> Self {
        TrestleReader {
            inner: crate::tiff::TiffReader::new(),
            overlaps: Vec::new(),
            level_ifd: Vec::new(),
        }
    }

    /// Mirror of `TrestleReader.isThisType(RandomAccessInputStream)`: the first
    /// IFD's COPYRIGHT tag must contain "Trestle Corp.".
    fn is_trestle_copyright(&self) -> bool {
        // TIFF COPYRIGHT tag (33432); no named constant in `tiff::ifd::tag`.
        const COPYRIGHT: u16 = 33432;
        self.inner
            .ifd(0)
            .and_then(|ifd| ifd.get_str(COPYRIGHT))
            .map(|c| c.contains("Trestle Corp."))
            .unwrap_or(false)
    }

    /// Mirror of the `addGlobalMeta(key, value)` loop in
    /// `TrestleReader.initStandardMetadata`. The first IFD comment is split on
    /// `;`, and every `key=value` fragment is stored.
    fn init_standard_metadata(&mut self) {
        if !self.is_trestle_copyright() {
            return;
        }
        let comment = {
            let series = self.inner.series_list();
            series.first().and_then(|s| {
                s.metadata
                    .series_metadata
                    .get("ImageDescription")
                    .and_then(|v| match v {
                        MetadataValue::String(s) => Some(s.clone()),
                        _ => None,
                    })
            })
        };
        let Some(comment) = comment else { return };

        let mut parsed: Vec<(String, MetadataValue)> = Vec::new();
        let mut overlaps: Option<Vec<i64>> = None;
        for v in comment.split(';') {
            let Some(eq) = v.find('=') else { continue };
            let key = v[..eq].trim();
            let value = v[eq + 1..].trim();
            if key.is_empty() {
                continue;
            }
            parsed.push((
                key.to_string(),
                MetadataValue::String(value.to_string()),
            ));
            // Java: if key == "OverlapsXY", split the value on ' ' and parse ints.
            if key == "OverlapsXY" {
                let vals: Vec<i64> = value
                    .split(' ')
                    .filter(|s| !s.is_empty())
                    .filter_map(|s| s.parse::<i64>().ok())
                    .collect();
                overlaps = Some(vals);
            }
        }
        if parsed.is_empty() {
            return;
        }
        for series in self.inner.series_list_mut() {
            for (key, value) in &parsed {
                series
                    .metadata
                    .series_metadata
                    .insert(key.clone(), value.clone());
            }
        }

        // Mirror the core-metadata rebuild in `TrestleReader.initStandardMetadata`:
        // every main IFD becomes one resolution level of a single pyramid series.
        // SizeX/SizeY are reduced by the per-tile overlaps; everything past the
        // first IFD is flagged as a (thumbnail) sub-resolution.
        self.regroup_resolutions(overlaps.as_deref());
    }

    /// Faithful port of the core-metadata loop in
    /// `TrestleReader.initStandardMetadata`. Each main IFD is one resolution
    /// level; `overlaps[index*2 + {0,1}]` are the per-tile X/Y overlaps used to
    /// shrink each level's `SizeX`/`SizeY`. The result is a single
    /// multi-resolution series (`resolutionCount = #IFDs`).
    fn regroup_resolutions(&mut self, overlaps: Option<&[i64]>) {
        let ifd_count = self.inner.ifd_count();
        if ifd_count <= 1 {
            return; // single IFD: leave the plain TIFF series untouched
        }

        // Compute the overlap-adjusted size/flags for each IFD.
        struct Level {
            size_x: u32,
            size_y: u32,
            size_c: u32,
            is_rgb: bool,
        }
        let mut levels: Vec<Level> = Vec::with_capacity(ifd_count);
        for index in 0..ifd_count {
            let Some(ifd) = self.inner.ifd(index) else {
                return;
            };
            let samples = ifd.samples_per_pixel() as u32;
            let is_rgb =
                samples > 1 || matches!(ifd.photometric(), crate::tiff::ifd::Photometric::Rgb);
            let image_width = ifd.image_width().unwrap_or(0);
            let image_length = ifd.image_length().unwrap_or(0);
            // getTilesPerRow/Column = ceil(image dim / tile dim); minus 1.
            let tiles_per_row = match ifd.tile_width() {
                Some(tw) if tw > 0 => image_width.div_ceil(tw),
                _ => 1,
            };
            let tiles_per_col = match ifd.tile_length() {
                Some(tl) if tl > 0 => image_length.div_ceil(tl),
                _ => 1,
            };
            let num_tile_cols = tiles_per_row.saturating_sub(1) as i64;
            let num_tile_rows = tiles_per_col.saturating_sub(1) as i64;
            let overlap_x = overlaps
                .and_then(|o| o.get(index * 2).copied())
                .unwrap_or(0);
            let overlap_y = overlaps
                .and_then(|o| o.get(index * 2 + 1).copied())
                .unwrap_or(0);
            let size_x = (image_width as i64 - num_tile_cols * overlap_x).max(0) as u32;
            let size_y = (image_length as i64 - num_tile_rows * overlap_y).max(0) as u32;
            levels.push(Level {
                size_x,
                size_y,
                size_c: if is_rgb { samples } else { 1 },
                is_rgb,
            });
        }

        // Build a single pyramid series. Level 0 is the main resolution; the
        // remaining IFDs are sub-resolutions (and thumbnails).
        let series = self.inner.series_list();
        let Some(template) = series.first().cloned() else {
            return;
        };
        let mut main = template;
        // Each existing series maps to one IFD in order.
        let ifd_for_series: Vec<usize> = series
            .iter()
            .filter_map(|s| s.ifd_indices.first().copied())
            .collect();
        if ifd_for_series.len() != ifd_count {
            // Series<->IFD mapping isn't 1:1; don't risk a bad regroup.
            return;
        }

        let l0 = &levels[0];
        main.ifd_indices = vec![ifd_for_series[0]];
        main.metadata.size_x = l0.size_x;
        main.metadata.size_y = l0.size_y;
        main.metadata.size_z = 1;
        main.metadata.size_t = 1;
        main.metadata.size_c = l0.size_c;
        main.metadata.is_rgb = l0.is_rgb;
        main.metadata.image_count = 1;
        main.metadata.is_interleaved = false;
        main.metadata.dimension_order = DimensionOrder::XYCZT;
        main.metadata.resolution_count = ifd_count as u32;
        main.sub_resolutions = ifd_for_series[1..].iter().map(|&i| vec![i]).collect();

        // Record the per-level IFD mapping and the (per-core-index) overlaps so
        // `openBytes` can replicate Java's overlap-aware `tiffParser.getSamples`.
        self.level_ifd = ifd_for_series.clone();
        self.overlaps = overlaps.map(|o| o.to_vec()).unwrap_or_default();

        self.inner.replace_series(vec![main]);
    }
}

impl Default for TrestleReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for TrestleReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("tif"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.inner.close()?;
        self.overlaps.clear();
        self.level_ifd.clear();
        self.inner.set_id(path)?;
        for series in self.inner.series_list_mut() {
            series.metadata.series_metadata.insert(
                "hcs2.wrapper".to_string(),
                MetadataValue::String("TrestleReader".to_string()),
            );
        }
        self.init_standard_metadata();
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.overlaps.clear();
        self.level_ifd.clear();
        self.inner.close()
    }

    fn series_count(&self) -> usize {
        self.inner.series_count()
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.inner.series_count() == 0 {
            return Err(BioFormatsError::NotInitialized);
        }
        self.inner.set_series(s)
    }

    fn series(&self) -> usize {
        self.inner.series()
    }

    fn metadata(&self) -> &ImageMetadata {
        self.inner.metadata()
    }

    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        let (w, h) = {
            let m = self.inner.metadata();
            (m.size_x, m.size_y)
        };
        self.open_bytes_region(p, 0, 0, w, h)
    }

    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        // Java `TrestleReader.openBytes`:
        //   if (core.size() == 1 && core.size(0) == 1) return super.openBytes(...);
        //   else tiffParser.getSamples(ifd, buf, x, y, w, h, overlapX, overlapY);
        //
        // The single-series/single-resolution case (the regroup did not run, so
        // `level_ifd` is empty) is a plain TIFF read. Otherwise we must remove
        // the per-tile overlap. The shared TIFF engine's `getSamples` has no
        // overlap parameter, so we reconstruct it here: read the full stored IFD
        // plane (overlaps still present) and copy each tile's non-overlapping
        // (tileWidth-overlapX) x (tileLength-overlapY) portion into the output,
        // replicating the tile-placement arithmetic of `TiffParser.getSamples`.
        if self.level_ifd.is_empty() {
            return self.inner.open_bytes_region(p, x, y, w, h);
        }
        let core_index = self.inner.resolution();
        let ifd_index = match self.level_ifd.get(core_index) {
            Some(&i) => i,
            None => return self.inner.open_bytes_region(p, x, y, w, h),
        };
        let overlap_x = self
            .overlaps
            .get(core_index * 2)
            .copied()
            .unwrap_or(0)
            .max(0) as u32;
        let overlap_y = self
            .overlaps
            .get(core_index * 2 + 1)
            .copied()
            .unwrap_or(0)
            .max(0) as u32;

        // No overlap declared: behaves identically to a plain region read.
        if overlap_x == 0 && overlap_y == 0 {
            return self.inner.open_bytes_region(p, x, y, w, h);
        }

        // Geometry of the stored (overlap-inclusive) IFD plane.
        let (raw_w, raw_h, tile_w, tile_h, bytes_per_sample, eff_channels) = {
            let ifd = self
                .inner
                .ifd(ifd_index)
                .ok_or(BioFormatsError::PlaneOutOfRange(p))?;
            let raw_w = ifd.image_width().unwrap_or(0);
            let raw_h = ifd.image_length().unwrap_or(0);
            // TiffParser: tileLength <= 0 -> height; tileWidth defaults likewise.
            let tile_w = ifd.tile_width().filter(|&t| t > 0).unwrap_or(raw_w);
            let tile_h = ifd.tile_length().filter(|&t| t > 0).unwrap_or(raw_h);
            let samples = ifd.samples_per_pixel().max(1) as u32;
            let planar = ifd.planar_configuration() == 2;
            let eff_channels = if planar { 1 } else { samples };
            // Trestle pixel data is 8-bit per sample in practice; derive from the
            // metadata pixel type to stay faithful to BitsPerSample[0]/8.
            let bps = self.inner.metadata().pixel_type.bytes_per_sample() as u32;
            (raw_w, raw_h, tile_w, tile_h, bps.max(1), eff_channels)
        };

        // Decode the full stored plane once (interleaved samples, stored dims).
        let plane = self
            .inner
            .read_physical_ifd_region(ifd_index, 0, 0, raw_w, raw_h)?;

        let pixel = (bytes_per_sample * eff_channels) as i64;
        let out_w = w as i64;
        let out_h = h as i64;
        let mut buf = vec![0u8; (out_w * out_h * pixel).max(0) as usize];

        let tile_w = tile_w as i64;
        let tile_h = tile_h as i64;
        let overlap_x = overlap_x as i64;
        let overlap_y = overlap_y as i64;
        let raw_w_i = raw_w as i64;
        let raw_h_i = raw_h as i64;
        let step_x = (tile_w - overlap_x).max(1);
        let step_y = (tile_h - overlap_y).max(1);
        let x = x as i64;
        let y = y as i64;
        let end_x = x + out_w;
        let end_y = y + out_h;
        // src plane stride in bytes (interleaved samples across the row).
        let src_row_len = raw_w_i * pixel;
        let out_row_len = out_w * pixel;

        // Java iterates numTileRows x numTileCols (= ceil(raw/tile)).
        let num_tile_rows = (raw_h_i + tile_h - 1) / tile_h;
        let num_tile_cols = (raw_w_i + tile_w - 1) / tile_w;

        for row in 0..num_tile_rows {
            // first row is shortened by overlapY (Java tileBounds.height).
            let tb_h = if row == 0 { tile_h - overlap_y } else { tile_h };
            for col in 0..num_tile_cols {
                let tb_w = if col == 0 { tile_w - overlap_x } else { tile_w };
                let tb_x = col * step_x;
                let tb_y = row * step_y;

                if tb_x > x + out_w {
                    break;
                }
                // intersects(imageBounds, tileBounds)
                if tb_x >= end_x || tb_y >= end_y || tb_x + tb_w <= x || tb_y + tb_h <= y {
                    continue;
                }

                let tile_x = tb_x.max(x);
                let tile_y = tb_y.max(y);
                let real_x = tile_x.rem_euclid(step_x);
                let real_y = tile_y.rem_euclid(step_y);

                let mut twidth = (end_x - tile_x).min(tile_w - real_x);
                if twidth <= 0 {
                    twidth = (end_x - tile_x).max(tile_w - real_x);
                }
                let mut theight = (end_y - tile_y).min(tile_h - real_y);
                if theight <= 0 {
                    theight = (end_y - tile_y).max(tile_h - real_y);
                }

                let copy = pixel * twidth;
                // Source within the stored plane: the tile's pixel (real_x,real_y)
                // lives at stored column (col*tileW + real_x), stored row
                // (row*tileH + real_y).
                let src_col = col * tile_w + real_x;
                let src_row0 = row * tile_h + real_y;
                let mut dest = pixel * (tile_x - x) + out_row_len * (tile_y - y);

                for tr in 0..theight {
                    let src_row = src_row0 + tr;
                    if src_row >= raw_h_i {
                        break;
                    }
                    let src = src_row * src_row_len + src_col * pixel;
                    let (s, d) = (src, dest);
                    if s >= 0 && d >= 0 {
                        let (s, d, c) = (s as usize, d as usize, copy.max(0) as usize);
                        // clamp the copy length to both buffers and the row.
                        let max_src = plane.len().saturating_sub(s);
                        let max_dst = buf.len().saturating_sub(d);
                        let n = c.min(max_src).min(max_dst);
                        if n > 0 {
                            buf[d..d + n].copy_from_slice(&plane[s..s + n]);
                        }
                    }
                    dest += out_row_len;
                }
            }
        }
        Ok(buf)
    }

    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        self.inner.open_thumb_bytes(p)
    }

    fn resolution_count(&self) -> usize {
        self.inner.resolution_count()
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        self.inner.set_resolution(level)
    }
}

// ---------------------------------------------------------------------------
// 6. TissueFAXS
// ---------------------------------------------------------------------------
/// TissueFAXS (TissueGnostics): an `.aqproj` project file plus one or more
/// `.tfcyto` SQLite databases.
///
/// Partial port of `TissueFAXSReader`. Detection is faithful — Java declares
/// suffixes `{"aqproj", "tfcyto"}` with `suffixSufficient = true` — but the
/// reader body is **not** ported: unlike the other readers in this module,
/// TissueFAXS is not a vendor-TIFF format. Pixels and all structural metadata
/// live inside SQLite `.tfcyto` databases (tiles stored as JPEG/JPEG-XR BLOBs in
/// `images`/`correction_images` tables, region/FOV/channel geometry in JSON), so
/// the shared TIFF engine cannot supply them. Reading requires a SQLite driver
/// (no crate dependency exists) plus JPEG-XR tile decode and the multi-
/// resolution FOV-stitching pipeline from `openBytes`/`copyRegionToBuffer`.
///
/// The real reader is implemented behind the optional `tissuefaxs` cargo
/// feature (which pulls in `rusqlite`). With the feature **off**, `set_id`
/// reports `UnsupportedFormat`.
pub struct TissueFaxsReader {
    initialized: bool,
    /// Flattened "core" list, mirroring Java's `core`: one entry per resolution
    /// per pyramid, optionally followed by a correction-image entry.
    core: Vec<ImageMetadata>,
    /// Parsed scan regions (one or more per pyramid; see `ScanRegion`).
    regions: Vec<ScanRegion>,
    /// Resolved `.tfcyto` database file paths.
    pixels_files: Vec<PathBuf>,
    /// Indexes into `core` that begin a new series (full-resolution images +
    /// correction images), in order. Used to map series→core index.
    series_starts: Vec<usize>,
    /// Number of resolutions in each series (1 for correction images).
    series_resolutions: Vec<usize>,
    /// Index into `regions` that supplies the descriptive metadata for each
    /// series (the pyramid's first region; same region for its correction
    /// image). Used by `ome_metadata` to mirror Java `initFile`'s store loop.
    series_regions: Vec<usize>,
    current_series: usize,
    current_resolution: usize,
}

/// One field-of-view rectangle (Java `loci.common.Region`).
#[derive(Clone, Copy, Debug, Default)]
#[cfg_attr(not(feature = "tissuefaxs"), allow(dead_code))]
struct Region {
    x: i64,
    y: i64,
    width: i64,
    height: i64,
}

#[cfg_attr(not(feature = "tissuefaxs"), allow(dead_code))]
impl Region {
    fn new(x: i64, y: i64, width: i64, height: i64) -> Self {
        Region {
            x,
            y,
            width,
            height,
        }
    }

    /// Mirrors Java `Region.intersection`.
    fn intersection(&self, other: &Region) -> Region {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let w = (self.x + self.width).min(other.x + other.width) - x;
        let h = (self.y + self.height).min(other.y + other.height) - y;
        Region::new(x, y, w.max(0), h.max(0))
    }
}

/// Java inner class `Channel`.
#[derive(Clone, Debug, Default)]
#[cfg_attr(not(feature = "tissuefaxs"), allow(dead_code))]
struct Channel {
    #[allow(dead_code)]
    id: i64,
    name: Option<String>,
    #[allow(dead_code)]
    color: i64,
    ex_wave: i64,
    em_wave: i64,
}

#[cfg_attr(not(feature = "tissuefaxs"), allow(dead_code))]
impl Channel {
    /// Java `Channel.getColor`. The DB returns ARGB; the OME model is RGBA.
    /// Returns the packed RGBA integer (the value OME-XML stores as `Color`).
    fn get_color(&self) -> i32 {
        let color = self.color;
        let alpha = ((color >> 24) & 0xff) as i64;
        let red = ((color >> 16) & 0xff) as i64;
        let green = ((color >> 8) & 0xff) as i64;
        let blue = (color & 0xff) as i64;
        // ome.xml.model.primitives.Color packs as (r<<24)|(g<<16)|(b<<8)|a.
        ((red << 24) | (green << 16) | (blue << 8) | alpha) as i32
    }
}

/// Java inner class `ScanRegion`.
#[cfg_attr(not(feature = "tissuefaxs"), allow(dead_code))]
struct ScanRegion {
    file: PathBuf,
    region_metadata: serde_json::Value,
    id: i64,

    full_resolution_core_index: usize,
    correction_image_core_index: Option<usize>,
    resolutions: Vec<i64>,

    tile_size_x: i64,
    tile_size_y: i64,
    overlap_x: i64,
    overlap_y: i64,
    scale_factor: i64,
    tile_range_x: [i64; 2],
    tile_range_y: [i64; 2],
    z_steps: Vec<i64>,
    timepoint: i64,
    channels: Vec<Channel>,
    fovs: HashMap<String, Region>,
    correction_image_ids: HashMap<String, i64>,

    tma_x: Option<i64>,
    tma_y: Option<i64>,
}

#[cfg_attr(not(feature = "tissuefaxs"), allow(dead_code))]
impl ScanRegion {
    fn new() -> Self {
        ScanRegion {
            file: PathBuf::new(),
            region_metadata: serde_json::Value::Null,
            id: 0,
            full_resolution_core_index: 0,
            correction_image_core_index: None,
            resolutions: Vec::new(),
            tile_size_x: 0,
            tile_size_y: 0,
            overlap_x: 0,
            overlap_y: 0,
            scale_factor: 0,
            tile_range_x: [i64::MAX, 0],
            tile_range_y: [i64::MAX, 0],
            z_steps: Vec::new(),
            timepoint: 0,
            channels: Vec::new(),
            fovs: HashMap::new(),
            correction_image_ids: HashMap::new(),
            tma_x: None,
            tma_y: None,
        }
    }

    /// Java `ScanRegion.parseJSON`.
    fn parse_json(&mut self) {
        let m = &self.region_metadata;
        self.tile_size_x = json_get_int(m, "ImageWidth");
        self.tile_size_y = json_get_int(m, "ImageHeight");
        self.overlap_x = json_get_int(m, "OverlapWidth");
        self.overlap_y = json_get_int(m, "OverlapHeight");
        self.scale_factor = json_get_int(m, "CacheStep");

        if m.get("LocationOnTMABlockX").is_some() {
            self.tma_x = Some(json_get_int(m, "LocationOnTMABlockX"));
        }
        if m.get("LocationOnTMABlockY").is_some() {
            self.tma_y = Some(json_get_int(m, "LocationOnTMABlockY"));
        }
    }
}

/// Helper: read an integer field from JSON (matches Java `JSONObject.getInt`,
/// which coerces numeric strings).
#[cfg_attr(not(feature = "tissuefaxs"), allow(dead_code))]
fn json_get_int(value: &serde_json::Value, key: &str) -> i64 {
    match value.get(key) {
        Some(serde_json::Value::Number(n)) => {
            n.as_i64().unwrap_or_else(|| n.as_f64().unwrap_or(0.0) as i64)
        }
        Some(serde_json::Value::String(s)) => s.trim().parse::<i64>().unwrap_or(0),
        _ => 0,
    }
}

/// Helper: read a string field from JSON (matches Java `JSONObject.getString`,
/// returning `None` for a missing/null key).
#[cfg_attr(not(feature = "tissuefaxs"), allow(dead_code))]
fn json_get_string(value: &serde_json::Value, key: &str) -> Option<String> {
    match value.get(key) {
        Some(serde_json::Value::String(s)) => Some(s.clone()),
        Some(serde_json::Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}

/// Helper: read a floating-point field from JSON (matches Java
/// `JSONObject.getDouble`, which coerces numeric strings).
#[cfg_attr(not(feature = "tissuefaxs"), allow(dead_code))]
fn json_get_double(value: &serde_json::Value, key: &str) -> Option<f64> {
    match value.get(key) {
        Some(serde_json::Value::Number(n)) => n.as_f64(),
        Some(serde_json::Value::String(s)) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

impl TissueFaxsReader {
    pub fn new() -> Self {
        TissueFaxsReader {
            initialized: false,
            core: Vec::new(),
            regions: Vec::new(),
            pixels_files: Vec::new(),
            series_starts: Vec::new(),
            series_resolutions: Vec::new(),
            series_regions: Vec::new(),
            current_series: 0,
            current_resolution: 0,
        }
    }

    /// Current flattened core index (full-resolution core + resolution offset).
    fn core_index(&self) -> usize {
        self.series_starts
            .get(self.current_series)
            .copied()
            .unwrap_or(0)
            + self.current_resolution
    }
}

impl Default for TissueFaxsReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for TissueFaxsReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        // Java: suffixes {"aqproj", "tfcyto"}, suffixSufficient = true.
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("aqproj") | Some("tfcyto"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    #[cfg(not(feature = "tissuefaxs"))]
    fn set_id(&mut self, _path: &Path) -> Result<()> {
        self.initialized = false;
        Err(BioFormatsError::UnsupportedFormat(
            "TissueFAXS (.aqproj/.tfcyto) requires the 'tissuefaxs' cargo feature \
             (SQLite-backed reader): cargo build --features tissuefaxs"
                .into(),
        ))
    }

    #[cfg(feature = "tissuefaxs")]
    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        self.init_file(path)?;
        self.initialized = true;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.initialized = false;
        self.core.clear();
        self.regions.clear();
        self.pixels_files.clear();
        self.series_starts.clear();
        self.series_resolutions.clear();
        self.series_regions.clear();
        self.current_series = 0;
        self.current_resolution = 0;
        Ok(())
    }

    fn series_count(&self) -> usize {
        self.series_starts.len()
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if !self.initialized {
            return Err(BioFormatsError::NotInitialized);
        }
        if s >= self.series_starts.len() {
            return Err(BioFormatsError::SeriesOutOfRange(s));
        }
        self.current_series = s;
        self.current_resolution = 0;
        Ok(())
    }

    fn series(&self) -> usize {
        self.current_series
    }

    fn metadata(&self) -> &ImageMetadata {
        if !self.initialized {
            return crate::common::reader::uninitialized_metadata();
        }
        self.core
            .get(self.core_index())
            .unwrap_or_else(|| crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        if !self.initialized {
            return Err(BioFormatsError::NotInitialized);
        }
        let (w, h) = {
            let m = self.metadata();
            (m.size_x, m.size_y)
        };
        self.open_bytes_region(p, 0, 0, w, h)
    }

    #[cfg(not(feature = "tissuefaxs"))]
    fn open_bytes_region(
        &mut self,
        _p: u32,
        _x: u32,
        _y: u32,
        _w: u32,
        _h: u32,
    ) -> Result<Vec<u8>> {
        Err(BioFormatsError::NotInitialized)
    }

    #[cfg(feature = "tissuefaxs")]
    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        if !self.initialized {
            return Err(BioFormatsError::NotInitialized);
        }
        self.open_bytes_impl(p, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, _p: u32) -> Result<Vec<u8>> {
        Err(BioFormatsError::NotInitialized)
    }

    fn resolution_count(&self) -> usize {
        self.series_resolutions
            .get(self.current_series)
            .copied()
            .unwrap_or(1)
            .max(1)
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if !self.initialized {
            return Err(BioFormatsError::NotInitialized);
        }
        if level >= self.resolution_count() {
            return Err(BioFormatsError::SeriesOutOfRange(level));
        }
        self.current_resolution = level;
        Ok(())
    }

    fn resolution(&self) -> usize {
        self.current_resolution
    }

    #[cfg(feature = "tissuefaxs")]
    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        if !self.initialized {
            return None;
        }
        Some(self.build_ome_metadata())
    }
}

// ---------------------------------------------------------------------------
// TissueFAXS: SQLite-backed reader body (feature-gated)
// ---------------------------------------------------------------------------
#[cfg(feature = "tissuefaxs")]
mod tissuefaxs_impl {
    use super::*;
    use crate::common::ome_metadata::{
        create_lsid, OmeImage, OmeInstrument, OmeMetadata, OmeObjective,
    };
    use rusqlite::{Connection, OpenFlags};

    /// Per-region overlap range for the tile range.
    const I32_MAX: i64 = i32::MAX as i64;

    /// Java `FormatTools.getPhysicalSizeX`/`getPhysicalSizeY`: a physical size is
    /// only valid when present, finite, and strictly positive.
    fn get_physical_size(value: Option<f64>) -> Option<f64> {
        match value {
            Some(v) if v.is_finite() && v > 0.0 => Some(v),
            _ => None,
        }
    }
    // per specification, wavelengths outside this range should be ignored
    const WAVE_MIN: i64 = 300;
    const WAVE_MAX: i64 = 800;

    /// Bytes per pixel for the current pixel type (Java
    /// `FormatTools.getBytesPerPixel`).
    fn bytes_per_pixel(pt: PixelType) -> usize {
        pt.bytes_per_sample()
    }

    /// Number of RGB samples per pixel for a core entry (Java
    /// `getRGBChannelCount`).
    fn rgb_channel_count(m: &ImageMetadata) -> usize {
        if m.is_rgb {
            m.size_c as usize
        } else {
            1
        }
    }

    impl TissueFaxsReader {
        /// Java `initFile`.
        pub(super) fn init_file(&mut self, id: &Path) -> Result<()> {
            self.current_id_setup(id)?;
            self.find_db_files(id)?;
            self.core.clear();

            for file_index in 0..self.pixels_files.len() {
                let file = self.pixels_files[file_index].clone();
                let mut m = ImageMetadata {
                    pixel_type: PixelType::Uint8,
                    is_little_endian: true,
                    dimension_order: DimensionOrder::XYCZT,
                    size_z: 1,
                    size_c: 0,
                    size_t: 1,
                    resolution_count: 1,
                    ..Default::default()
                };

                let start_region_index = self.regions.len();

                let conn = open_connection(&file)?;
                // mirror Java's try/catch(SQLException) around init: log + continue
                if let Err(e) = self.init_one_db(&conn, &file, start_region_index, &mut m) {
                    eprintln!("TissueFAXS: failed to initialize {}: {e}", file.display());
                }

                // m.sizeZ = number of z steps of the first region
                m.size_z = self.regions[start_region_index].z_steps.len().max(1) as u32;

                // m.sizeT = max timepoint + 1 over this file's regions
                let mut size_t = 1i64;
                for r in &self.regions[start_region_index..] {
                    size_t = size_t.max(r.timepoint + 1);
                }
                m.size_t = size_t as u32;

                m.image_count = m.size_z * m.size_c.max(1) * m.size_t;

                // TODO (Java): bad assumption in general?
                if m.size_c == 1 && m.pixel_type == PixelType::Uint8 {
                    m.size_c = 3;
                    m.is_rgb = true;
                    m.is_interleaved = true;
                }
                m.image_count = m.size_z
                    * (if m.is_rgb { 1 } else { m.size_c.max(1) })
                    * m.size_t;

                let res_count = m.resolution_count.max(1);
                m.resolution_count = res_count;

                self.core.push(m.clone());
                let start = &self.regions[start_region_index];
                let scale_factor = start.scale_factor;
                let correction = start.correction_image_core_index.is_some();
                let tile_size_x = start.tile_size_x;
                let tile_size_y = start.tile_size_y;

                for r in 1..res_count {
                    let mut res = m.clone();
                    let scale = (scale_factor as f64).powi(r as i32) as u32;
                    let scale = scale.max(1);
                    res.size_x /= scale;
                    res.size_y /= scale;
                    res.resolution_count = 1;
                    res.image_count = res.size_z
                        * (if res.is_rgb { 1 } else { res.size_c.max(1) })
                        * res.size_t;
                    self.core.push(res);
                }

                if correction {
                    let mut corr = m.clone();
                    corr.size_x = tile_size_x as u32;
                    corr.size_y = tile_size_y as u32;
                    corr.pixel_type = PixelType::Float32;
                    corr.resolution_count = 1;
                    corr.is_little_endian = true;
                    corr.image_count = corr.size_z
                        * (if corr.is_rgb { 1 } else { corr.size_c.max(1) })
                        * corr.size_t;
                    self.core.push(corr);
                }
            }

            // Build series mapping over the flattened core list.
            self.build_series_map();

            self.current_series = 0;
            self.current_resolution = 0;
            Ok(())
        }

        /// Records the current file as a `.tfcyto` path placeholder; the real
        /// resolution happens in `find_db_files`. (Java `super.initFile`.)
        fn current_id_setup(&mut self, _id: &Path) -> Result<()> {
            Ok(())
        }

        /// Body of `initFile`'s per-database loop. Separated so SQL errors can
        /// be caught and logged like Java's `catch (SQLException)`.
        fn init_one_db(
            &mut self,
            conn: &Connection,
            file: &Path,
            start_region_index: usize,
            m: &mut ImageMetadata,
        ) -> rusqlite::Result<()> {
            // region query (expect one row per timepoint)
            {
                let mut stmt =
                    conn.prepare("SELECT id, data, is_timelapse FROM region ORDER BY id")?;
                let mut rows = stmt.query([])?;
                while let Some(row) = rows.next()? {
                    let region_id: i64 = row.get(0)?;
                    let json: String = row.get(1)?;
                    let is_timelapse: bool = row.get::<_, i64>(2)? != 0;

                    let mut region = ScanRegion::new();
                    region.id = region_id;
                    region.file = file.to_path_buf();

                    // expect trailing whitespace/line breaks in AcquisitionSettings
                    let json = json.trim().replace("\r\n", "_");
                    region.region_metadata = serde_json::from_str(&json)
                        .unwrap_or(serde_json::Value::Null);

                    region.parse_json();

                    if is_timelapse {
                        region.timepoint = (self.regions.len() - start_region_index) as i64;
                    } else if self.regions.len() != start_region_index {
                        // part of the full resolution in a TMA
                        region.resolutions.push(0);
                    }
                    self.regions.push(region);
                }
            }

            // per-region FOV / z-step / resolution / correction-image setup
            for region_index in start_region_index..self.regions.len() {
                let region_db_id = self.regions[region_index].id;

                // fovs
                {
                    let mut stmt = conn.prepare(
                        "SELECT row, column, stitch_rectangle_x, stitch_rectangle_y, \
                         stitch_rectangle_w, stitch_rectangle_h FROM fovs WHERE region_id=?",
                    )?;
                    let mut rows = stmt.query([region_db_id])?;
                    while let Some(row) = rows.next()? {
                        let r: i64 = row.get(0)?;
                        let c: i64 = row.get(1)?;
                        let x: f64 = row.get(2)?;
                        let y: f64 = row.get(3)?;
                        let w: f64 = row.get(4)?;
                        let h: f64 = row.get(5)?;
                        let cur = &mut self.regions[region_index];
                        cur.tile_range_y[0] = cur.tile_range_y[0].min(r);
                        cur.tile_range_y[1] = cur.tile_range_y[1].max(r);
                        cur.tile_range_x[0] = cur.tile_range_x[0].min(c);
                        cur.tile_range_x[1] = cur.tile_range_x[1].max(c);
                        let fov = Region::new(x as i64, y as i64, w as i64, h as i64);
                        cur.fovs.insert(format!("{r}-{c}"), fov);
                    }
                }

                // z steps
                {
                    let mut stmt = conn.prepare(
                        "SELECT DISTINCT is_zstack,z_position FROM images WHERE region=? \
                         ORDER BY is_zstack,z_position",
                    )?;
                    let mut rows = stmt.query([region_db_id])?;
                    let mut tmp_z = Vec::new();
                    while let Some(row) = rows.next()? {
                        let _is_z: bool = row.get::<_, i64>(0)? != 0;
                        let z_pos: i64 = row.get(1)?;
                        tmp_z.push(z_pos);
                    }
                    self.regions[region_index].z_steps = tmp_z;
                }

                self.regions[region_index].full_resolution_core_index = self.core.len();

                if !self.regions[region_index].resolutions.is_empty() {
                    continue;
                }

                let (x_tiles, y_tiles, tsx, tsy, ox, oy) = {
                    let cur = &self.regions[region_index];
                    (
                        cur.tile_range_x[1] - cur.tile_range_x[0] + 1,
                        cur.tile_range_y[1] - cur.tile_range_y[0] + 1,
                        cur.tile_size_x,
                        cur.tile_size_y,
                        cur.overlap_x,
                        cur.overlap_y,
                    )
                };
                m.size_x = (x_tiles * (tsx - ox)).max(0) as u32;
                m.size_y = (y_tiles * (tsy - oy)).max(0) as u32;

                // max/min level → resolutionCount + resolutions list
                {
                    let mut stmt = conn.prepare(
                        "SELECT level FROM images WHERE region=? ORDER BY level DESC",
                    )?;
                    let mut rows = stmt.query([region_db_id])?;
                    let mut max = 0i64;
                    let mut min = I32_MAX;
                    let mut first = true;
                    while let Some(row) = rows.next()? {
                        let level: i64 = row.get(0)?;
                        if first {
                            max = level;
                            first = false;
                        }
                        if level >= m.resolution_count as i64 {
                            m.resolution_count = (level + 1) as u32;
                        }
                        min = level;
                    }
                    let cur = &mut self.regions[region_index];
                    let mut r = min;
                    while r <= max {
                        cur.resolutions.push(r);
                        r += 1;
                    }
                }

                // correction images (best-effort; older data lacks these tables)
                self.init_correction_images(conn, region_index, m.resolution_count as usize);
            }

            // channels
            {
                let mut stmt = conn.prepare(
                    "SELECT DISTINCT id, name, color, save_16bit, excitation_wavelength, \
                     emission_wavelength FROM channels ORDER BY id",
                )?;
                let mut rows = stmt.query([])?;
                while let Some(row) = rows.next()? {
                    m.size_c += 1;

                    let mut ch = Channel::default();
                    ch.id = row.get(0)?;
                    ch.name = row.get::<_, Option<String>>(1)?;
                    ch.color = row.get::<_, Option<i64>>(2)?.unwrap_or(0);

                    let save16: bool = row.get::<_, Option<i64>>(3)?.unwrap_or(0) != 0;
                    if save16 {
                        m.pixel_type = PixelType::Uint16;
                    }
                    ch.ex_wave = row.get::<_, Option<i64>>(4)?.unwrap_or(0);
                    ch.em_wave = row.get::<_, Option<i64>>(5)?.unwrap_or(0);

                    self.regions[start_region_index].channels.push(ch);
                }
            }

            Ok(())
        }

        /// Java: the two correction-image queries inside the inner `try`.
        fn init_correction_images(
            &mut self,
            conn: &Connection,
            region_index: usize,
            resolution_count: usize,
        ) {
            let region_db_id = self.regions[region_index].id;
            let full_res = self.regions[region_index].full_resolution_core_index;

            let mut found: Vec<(String, i64, usize)> = Vec::new();
            let q1 = conn.prepare(
                "SELECT correction_images.id, channel_zstack.channel_id, channel_zstack.position \
                 FROM correction_images JOIN channel_zstack \
                 ON correction_images.id = channel_zstack.cor_img_id \
                 WHERE channel_zstack.region_id=?",
            );
            if let Ok(mut stmt) = q1 {
                if let Ok(mut rows) = stmt.query([region_db_id]) {
                    while let Ok(Some(row)) = rows.next() {
                        let correction_id: i64 = row.get(0).unwrap_or(0);
                        let channel: i64 = row.get(1).unwrap_or(0);
                        let z: i64 = row.get(2).unwrap_or(0);
                        found.push((
                            format!("{}-{}", channel - 1, z - 1),
                            correction_id,
                            full_res + resolution_count,
                        ));
                    }
                }
            }
            let q2 = conn.prepare(
                "SELECT correction_images.id, channels.id \
                 FROM correction_images JOIN channels \
                 ON correction_images.id = channels.cor_img_id \
                 WHERE channels.region_id=?",
            );
            if let Ok(mut stmt) = q2 {
                if let Ok(mut rows) = stmt.query([region_db_id]) {
                    while let Ok(Some(row)) = rows.next() {
                        let correction_id: i64 = row.get(0).unwrap_or(0);
                        let channel_id: i64 = row.get(1).unwrap_or(0);
                        found.push((
                            format!("{}-0", channel_id - 1),
                            correction_id,
                            full_res + resolution_count,
                        ));
                    }
                }
            }

            let cur = &mut self.regions[region_index];
            for (key, id, core_idx) in found {
                cur.correction_image_core_index = Some(core_idx);
                cur.correction_image_ids.insert(key, id);
            }
        }

        /// Build `series_starts` / `series_resolutions` from the flattened
        /// `core` list and per-region resolution counts. Mirrors the non-
        /// flattened series enumeration in Java's `initFile` metadata loop.
        fn build_series_map(&mut self) {
            self.series_starts.clear();
            self.series_resolutions.clear();
            self.series_regions.clear();

            let mut populated: Vec<usize> = Vec::new();
            let mut i = 0usize;
            while i < self.regions.len() {
                let full_res = self.regions[i].full_resolution_core_index;
                if populated.contains(&full_res) {
                    i += 1;
                    continue;
                }
                populated.push(full_res);

                let res_count = self.core[full_res].resolution_count.max(1) as usize;
                self.series_starts.push(full_res);
                self.series_resolutions.push(res_count);
                self.series_regions.push(i);

                if let Some(corr) = self.regions[i].correction_image_core_index {
                    self.series_starts.push(corr);
                    self.series_resolutions.push(1);
                    self.series_regions.push(i);
                }

                // advance past remaining TMA regions for this pyramid: skip
                // sizeT regions (Java: i += core.get(fullRes).sizeT)
                let size_t = self.core[full_res].size_t.max(1) as usize;
                i += size_t;
            }
        }

        /// Java `initFile` metadata-store loop (lines ~441-519): populate the
        /// descriptive OME store from the per-region AcquisitionSettings JSON.
        ///
        /// One OME `Image` is emitted per series in `series_starts` order, which
        /// mirrors Java's non-flattened `nextImage` enumeration (objective +
        /// full-resolution image, optionally followed by a correction image).
        pub(super) fn build_ome_metadata(&self) -> OmeMetadata {
            // store = makeFilterMetadata(); MetadataTools.populatePixels(store).
            // Build the per-series baseline (channel counts / samples-per-pixel)
            // from each series' full-resolution core entry.
            let mut ome = OmeMetadata::default();
            for &core_idx in &self.series_starts {
                let m = &self.core[core_idx];
                let mut img = OmeImage::default();
                // populatePixels: one channel per (non-RGB) C, samples-per-pixel.
                let mut tmp = OmeMetadata::default();
                let _ = tmp.populate_pixels(m, 0);
                img.channels = tmp.images.into_iter().next().unwrap_or_default().channels;
                ome.images.push(img);
            }

            // store.setInstrumentID(createLSID("Instrument", 0), 0).
            let instrument = create_lsid("Instrument", &[0]);
            let mut inst = OmeInstrument {
                id: Some(instrument.clone()),
                ..Default::default()
            };

            // for (int i=0, index=0; i<regions.size(); index++)  — replayed over
            // the already-computed series map (series_regions holds each
            // pyramid's first region; correction-image series reuse it).
            //
            // `index` advances once per distinct pyramid (objective slot); the
            // correction-image series does not consume a new objective.
            let mut objective_index = 0usize;
            let mut series = 0usize;
            while series < self.series_starts.len() {
                let region_idx = self.series_regions[series];
                let region = &self.regions[region_idx];
                let rm = &region.region_metadata;
                let full_res = region.full_resolution_core_index;

                // Objective:0:index
                let objective_id = create_lsid("Objective", &[0, objective_index]);
                let mut objective = OmeObjective {
                    id: Some(objective_id.clone()),
                    ..Default::default()
                };
                objective.lens_na = json_get_double(rm, "ObjectiveLensNA");
                objective.immersion =
                    Self::get_immersion(json_get_string(rm, "ObjectiveImmersion").as_deref());
                objective.nominal_magnification =
                    json_get_double(rm, "ObjectiveNominalMagnification");
                objective.model = json_get_string(rm, "ObjectiveName");
                inst.objectives.push(objective);

                // Full-resolution image (current `series`).
                {
                    let img = &mut ome.images[series];
                    img.name = json_get_string(rm, "Name");
                    img.instrument_ref = Some(0);
                    img.objective_ref = Some(objective_index);

                    img.physical_size_x =
                        get_physical_size(json_get_double(rm, "PhysicalSizeX"));
                    img.physical_size_y =
                        get_physical_size(json_get_double(rm, "PhysicalSizeY"));

                    let mode = Self::get_acquisition_mode(
                        json_get_string(rm, "AcquisitionMode").as_deref(),
                    );
                    let is_rgb = self.core[full_res].is_rgb;

                    for (c, ch) in region.channels.iter().enumerate() {
                        if c >= img.channels.len() {
                            break;
                        }
                        let och = &mut img.channels[c];
                        och.name = ch.name.clone();
                        if mode.is_some() {
                            och.acquisition_mode = mode.clone();
                        }
                        if ch.em_wave >= WAVE_MIN && ch.em_wave <= WAVE_MAX {
                            och.emission_wavelength = Some(ch.em_wave as f64);
                        }
                        if ch.ex_wave >= WAVE_MIN && ch.ex_wave <= WAVE_MAX {
                            och.excitation_wavelength = Some(ch.ex_wave as f64);
                        }
                        // don't set a channel color for brightfield data
                        // the channel color is expected to be white in that case
                        if !is_rgb {
                            och.color = Some(ch.get_color());
                        }
                    }
                }
                series += 1;

                // Optional correction image: the next series reuses this
                // objective and is named "<Name> Correction Image".
                if region.correction_image_core_index.is_some()
                    && series < self.series_starts.len()
                    && self.series_regions[series] == region_idx
                {
                    let name = json_get_string(rm, "Name").unwrap_or_default();
                    let corr = &mut ome.images[series];
                    corr.name = Some(format!("{name} Correction Image"));
                    corr.instrument_ref = Some(0);
                    corr.objective_ref = Some(objective_index);
                    series += 1;
                }

                objective_index += 1;
            }

            ome.instruments.push(inst);
            ome
        }

        /// Java `MetadataTools.getImmersion` (enum lookup). The OME model stores
        /// the immersion as a controlled-vocabulary string; this crate keeps the
        /// raw value, mirroring the other readers in this module.
        fn get_immersion(value: Option<&str>) -> Option<String> {
            value.map(|s| s.to_string())
        }

        /// Java `MetadataTools.getAcquisitionMode` (enum lookup); see
        /// `get_immersion` for the string-passthrough rationale.
        fn get_acquisition_mode(value: Option<&str>) -> Option<String> {
            value.map(|s| s.to_string())
        }

        /// Java `findDBFiles`.
        fn find_db_files(&mut self, id: &Path) -> Result<()> {
            let lower = id
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_ascii_lowercase());
            if lower.as_deref() == Some("tfcyto") {
                self.pixels_files.push(id.to_path_buf());
                return Ok(());
            }

            let dir = id.parent().unwrap_or_else(|| Path::new("."));
            let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
                .map_err(BioFormatsError::Io)?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .collect();
            entries.sort();
            for slide in entries {
                let name = slide
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_string();
                if name.starts_with("Slide ") && slide.is_dir() {
                    let mut db_files: Vec<PathBuf> = std::fs::read_dir(&slide)
                        .map_err(BioFormatsError::Io)?
                        .filter_map(|e| e.ok().map(|e| e.path()))
                        .collect();
                    db_files.sort();
                    for db in db_files {
                        let ext = db
                            .extension()
                            .and_then(|e| e.to_str())
                            .map(|e| e.to_ascii_lowercase());
                        if ext.as_deref() == Some("tfcyto") {
                            self.pixels_files.push(db);
                        }
                    }
                }
            }
            Ok(())
        }

        // -- openBytes pipeline --

        /// Java `openBytes(no, buf, x, y, w, h)`.
        pub(super) fn open_bytes_impl(
            &mut self,
            no: u32,
            x: u32,
            y: u32,
            w: u32,
            h: u32,
        ) -> Result<Vec<u8>> {
            let core_index = self.core_index();
            let m = self.core[core_index].clone();
            let bpp = bytes_per_pixel(m.pixel_type);
            let pixel = bpp * rgb_channel_count(&m);
            let buf_len = (w as usize) * (h as usize) * pixel;
            let mut buf = vec![0u8; buf_len];

            let zct = self.get_zct_coords(no, &m);
            let plane_regions = self.get_all_regions(zct[2], core_index);
            let level = self.current_resolution;

            let dest = Region::new(x as i64, y as i64, w as i64, h as i64);

            for region_idx in plane_regions {
                if self.is_correction_image(region_idx, core_index) {
                    self.copy_correction_image_to_buffer(region_idx, &dest, zct, &mut buf, &m)?;
                } else {
                    self.copy_region_to_buffer(region_idx, level, &dest, zct, &mut buf, &m)?;
                }
            }
            Ok(buf)
        }

        /// Compute Z/C/T from the plane index for the current core's dim order
        /// (XYCZT, optionally RGB). Java `FormatTools.getZCTCoords`.
        fn get_zct_coords(&self, no: u32, m: &ImageMetadata) -> [usize; 3] {
            let c_count = if m.is_rgb { 1 } else { m.size_c.max(1) } as usize;
            let z = m.size_z.max(1) as usize;
            let no = no as usize;
            // XYCZT
            let c = no % c_count;
            let z_idx = (no / c_count) % z;
            let t = no / (c_count * z);
            [z_idx, c, t]
        }

        /// Java `getAllRegions(t)` — returns indexes into `self.regions`.
        fn get_all_regions(&self, t: usize, index: usize) -> Vec<usize> {
            let t = t as i64;
            let mut plane = Vec::new();
            for i in (0..self.regions.len()).rev() {
                let r = &self.regions[i];
                if r.timepoint == t
                    && r.correction_image_core_index == Some(index)
                {
                    plane.push(i);
                    continue;
                }
                if r.timepoint == t && r.full_resolution_core_index <= index {
                    let res = (index - r.full_resolution_core_index) as i64;
                    if r.resolutions.contains(&res) {
                        plane.push(i);
                    }
                }
            }
            plane
        }

        /// Java `getRegion()` (t=0). Used by `getOptimalTileWidth/Height` and
        /// `getSeriesUsedFiles` in Java; retained here as a faithful port even
        /// though those optional trait methods are not yet wired up.
        #[allow(dead_code)]
        pub(super) fn get_region(&self) -> Result<usize> {
            self.get_region_t(0)
        }

        /// Java `getRegion(t)` — returns an index into `self.regions`.
        #[allow(dead_code)]
        fn get_region_t(&self, t: i64) -> Result<usize> {
            let index = self.core_index();
            for i in (0..self.regions.len()).rev() {
                let r = &self.regions[i];
                if r.timepoint == t && r.correction_image_core_index == Some(index) {
                    return Ok(i);
                }
                if r.timepoint == t && r.full_resolution_core_index <= index {
                    let res = (index - r.full_resolution_core_index) as i64;
                    if r.resolutions.contains(&res) {
                        return Ok(i);
                    }
                }
            }
            Err(BioFormatsError::Format(format!(
                "Could not find ScanRegion (core index {index}, t={t})"
            )))
        }

        /// Java `isCorrectionImage`.
        fn is_correction_image(&self, region_idx: usize, core_index: usize) -> bool {
            match self.regions[region_idx].correction_image_core_index {
                None => false,
                Some(c) => core_index == c,
            }
        }

        /// Java `getCodecOptions` (only the fields the codecs need here).
        fn codec_options(&self, region_idx: usize, _m: &ImageMetadata) -> (usize, usize) {
            let r = &self.regions[region_idx];
            (r.tile_size_x as usize, r.tile_size_y as usize)
        }

        /// Java `getCodec` + decompress, returning raw tile bytes.
        fn decode_tile(
            &self,
            compression: i64,
            data: &[u8],
            _width: usize,
            _height: usize,
        ) -> Result<Vec<u8>> {
            match compression {
                0 | 1 => Ok(data.to_vec()), // PassthroughCodec
                6 => crate::common::codec::decompress_jpegxr(data),
                7 => crate::common::codec::decompress_jpeg(data),
                other => Err(BioFormatsError::UnsupportedFormat(format!(
                    "Unsupported TissueFAXS tile compression: {other}"
                ))),
            }
        }

        /// Java `splitFOVs`.
        fn split_fovs(
            &self,
            region_idx: usize,
            tile: &[u8],
            scale: usize,
            m: &ImageMetadata,
        ) -> Vec<Vec<u8>> {
            let r = &self.regions[region_idx];
            let bpp = bytes_per_pixel(m.pixel_type);
            let channels = rgb_channel_count(m);
            let pixel = bpp * channels;

            let tile_size_x = r.tile_size_x as usize;
            let tile_size_y = r.tile_size_y as usize;
            let src_width = tile_size_x * pixel;
            let dest_width = (tile_size_x / scale) * pixel;
            let dest_height = tile_size_y / scale;

            let mut fovs: Vec<Vec<u8>> = Vec::with_capacity(scale * scale);
            for fov in 0..(scale * scale) {
                let fov_row = fov / scale;
                let fov_col = fov % scale;
                // Java allocates destWidth*destHeight*pixel but copies destWidth
                // bytes per row for destHeight rows; destWidth already includes
                // `pixel`, so the usable region is destWidth*destHeight.
                let mut out = vec![0u8; dest_width * dest_height * pixel];
                for row in 0..dest_height {
                    let src_offset =
                        (((fov_row * dest_height) + row) * src_width) + (fov_col * dest_width);
                    let dest_offset = row * dest_width;
                    if src_offset + dest_width <= tile.len()
                        && dest_offset + dest_width <= out.len()
                    {
                        out[dest_offset..dest_offset + dest_width]
                            .copy_from_slice(&tile[src_offset..src_offset + dest_width]);
                    }
                }
                fovs.push(out);
            }
            fovs
        }

        /// Java `copyRegion`.
        #[allow(clippy::too_many_arguments)]
        fn copy_region(
            &self,
            src_region: &Region,
            src: &[u8],
            dest_region: &Region,
            dest: &mut [u8],
            tile_width: i64,
            m: &ImageMetadata,
        ) {
            let intersection = src_region.intersection(dest_region);
            let bpp = bytes_per_pixel(m.pixel_type) as i64;
            let pixel = bpp * rgb_channel_count(m) as i64;
            let output_row_len = dest_region.width * pixel;
            let intersection_x = (dest_region.x - src_region.x).max(0);
            let row_len = pixel * intersection.width.min(src_region.width);

            let output_row = intersection.y - dest_region.y;
            let output_col = intersection.x - dest_region.x;
            let output_offset = output_row * output_row_len + output_col * pixel;

            if row_len <= 0 {
                return;
            }
            for copy_row in 0..intersection.height {
                let real_row = copy_row + intersection.y - src_region.y;
                let input_offset = pixel * (real_row * tile_width + intersection_x);
                let dst_start = output_offset + copy_row * output_row_len;
                let (i0, d0, len) = (input_offset, dst_start, row_len);
                if i0 < 0 || d0 < 0 {
                    continue;
                }
                let (i0, d0, len) = (i0 as usize, d0 as usize, len as usize);
                if i0 + len <= src.len() && d0 + len <= dest.len() {
                    dest[d0..d0 + len].copy_from_slice(&src[i0..i0 + len]);
                }
            }
        }

        /// Java `copyRegionToBuffer`.
        #[allow(clippy::too_many_arguments)]
        fn copy_region_to_buffer(
            &self,
            region_idx: usize,
            level: usize,
            dest: &Region,
            zct: [usize; 3],
            buf: &mut [u8],
            m: &ImageMetadata,
        ) -> Result<()> {
            let (region_id, scale_factor, overlap_x, overlap_y, tma_x, tma_y, file, z_step) = {
                let r = &self.regions[region_idx];
                (
                    r.id,
                    r.scale_factor,
                    r.overlap_x,
                    r.overlap_y,
                    r.tma_x,
                    r.tma_y,
                    r.file.clone(),
                    *r.z_steps.get(zct[0]).unwrap_or(&0),
                )
            };
            let scale = (scale_factor as f64).powi(level as i32) as i64;
            let scale = scale.max(1);
            let scaled_overlap_x = overlap_x / scale;
            let scaled_overlap_y = overlap_y / scale;
            let (opt_w, opt_h) = self.codec_options(region_idx, m);
            let opt_w = opt_w as i64;
            let opt_h = opt_h as i64;

            let conn = open_connection(&file)?;

            // list of tiles for this plane
            let tile_coords: Vec<(i64, i64)> = {
                let mut stmt = conn
                    .prepare(
                        "SELECT row, column FROM images WHERE region=? AND level=? AND \
                         channel=? AND is_zstack=? AND z_position=? ORDER BY row,column",
                    )
                    .map_err(sql_err)?;
                let params = rusqlite::params![
                    region_id,
                    level as i64,
                    zct[1] as i64 + 1,
                    if zct[0] > 0 { 1i64 } else { 0i64 },
                    z_step,
                ];
                let mut rows = stmt.query(params).map_err(sql_err)?;
                let mut out = Vec::new();
                while let Some(row) = rows.next().map_err(sql_err)? {
                    out.push((row.get(0).map_err(sql_err)?, row.get(1).map_err(sql_err)?));
                }
                out
            };

            let (tile_range_x0, tile_range_y0) = {
                let r = &self.regions[region_idx];
                (r.tile_range_x[0], r.tile_range_y[0])
            };

            for (row, column) in tile_coords {
                let mut region_row = row;
                let mut region_column = column;
                if let Some(tx) = tma_x {
                    region_column += tx * scale_factor;
                }
                if let Some(ty) = tma_y {
                    region_row += ty * scale_factor;
                }

                let relative_column =
                    region_column - (tile_range_x0 as f64 / scale as f64).floor() as i64;
                let relative_row =
                    region_row - (tile_range_y0 as f64 / scale as f64).floor() as i64;

                let mut pixel_column = relative_column * opt_w;
                let mut pixel_row = relative_row * opt_h;

                let count = (scale * scale) as usize;
                let mut fov_positions: Vec<Option<Region>> = vec![None; count];

                if level == 0 {
                    let fov = self.regions[region_idx]
                        .fovs
                        .get(&format!("{row}-{column}"))
                        .copied()
                        .unwrap_or_default();
                    pixel_row -= fov.y;
                    pixel_column -= fov.x;
                    if tma_x.is_none() || tma_y.is_none() {
                        pixel_row -= relative_row * overlap_y;
                        pixel_column -= relative_column * overlap_x;
                    } else {
                        pixel_row -= region_row * overlap_y;
                        pixel_column -= region_column * overlap_x;
                    }
                    fov_positions[0] = Some(Region::new(pixel_column, pixel_row, opt_w, opt_h));
                } else {
                    for f in 0..count {
                        let fov_row = row * scale + (f as i64 / scale);
                        let fov_column = column * scale + (f as i64 % scale);
                        if let Some(fov) = self.regions[region_idx]
                            .fovs
                            .get(&format!("{fov_row}-{fov_column}"))
                            .copied()
                        {
                            let relative_fov_row = fov_row - tile_range_y0;
                            let relative_fov_column = fov_column - tile_range_x0;
                            let xx = (relative_fov_column * opt_w / scale)
                                - (fov.x / scale)
                                - (relative_fov_column * scaled_overlap_x);
                            let yy = (relative_fov_row * opt_h / scale)
                                - (fov.y / scale)
                                - (relative_fov_row * scaled_overlap_y);
                            fov_positions[f] =
                                Some(Region::new(xx, yy, opt_w / scale, opt_h / scale));
                        }
                    }
                }

                let mut fovs: Option<Vec<Vec<u8>>> = None;
                for f in 0..count {
                    let fp = match fov_positions[f] {
                        Some(r) => r,
                        None => continue,
                    };
                    let intersection = fp.intersection(dest);
                    if intersection.width > 0 && intersection.height > 0 {
                        if fovs.is_none() {
                            // fetch + decompress this tile
                            let mut stmt = conn
                                .prepare(
                                    "SELECT data, compression FROM images WHERE region=? AND \
                                     level=? AND channel=? AND is_zstack=? AND z_position=? AND \
                                     row=? AND column=?",
                                )
                                .map_err(sql_err)?;
                            let params = rusqlite::params![
                                region_id,
                                level as i64,
                                zct[1] as i64 + 1,
                                if zct[0] > 0 { 1i64 } else { 0i64 },
                                z_step,
                                row,
                                column,
                            ];
                            let mut rows = stmt.query(params).map_err(sql_err)?;
                            if let Some(r) = rows.next().map_err(sql_err)? {
                                let data: Vec<u8> = r.get(0).map_err(sql_err)?;
                                let compression: i64 = r.get(1).map_err(sql_err)?;
                                let tile = self.decode_tile(
                                    compression,
                                    &data,
                                    opt_w as usize,
                                    opt_h as usize,
                                )?;
                                // applyTransformation is a no-op in Java
                                if level == 0 {
                                    fovs = Some(vec![tile]);
                                } else {
                                    fovs =
                                        Some(self.split_fovs(region_idx, &tile, scale as usize, m));
                                }
                            } else {
                                return Err(BioFormatsError::Format(format!(
                                    "Could not get tile for row={row}, column={column}"
                                )));
                            }
                        }

                        if let Some(fovs_ref) = &fovs {
                            if let Some(src) = fovs_ref.get(f) {
                                self.copy_region(
                                    &fp,
                                    src,
                                    dest,
                                    buf,
                                    self.regions[region_idx].tile_size_x / scale,
                                    m,
                                );
                            }
                        }
                    }
                }
            }
            Ok(())
        }

        /// Java `copyCorrectionImageToBuffer`.
        fn copy_correction_image_to_buffer(
            &self,
            region_idx: usize,
            dest: &Region,
            zct: [usize; 3],
            buf: &mut [u8],
            m: &ImageMetadata,
        ) -> Result<()> {
            if self.regions[region_idx].timepoint != zct[2] as i64 {
                return Ok(());
            }
            let file = self.regions[region_idx].file.clone();
            let conn = open_connection(&file)?;

            let correction_id = self.regions[region_idx]
                .correction_image_ids
                .get(&format!("{}-{}", zct[1], zct[0]))
                .or_else(|| {
                    self.regions[region_idx]
                        .correction_image_ids
                        .get(&format!("{}-0", zct[1]))
                })
                .copied();
            let correction_id = match correction_id {
                Some(id) => id,
                None => return Ok(()),
            };

            let (opt_w, opt_h) = self.codec_options(region_idx, m);

            let mut stmt = conn
                .prepare("SELECT compression, data FROM correction_images WHERE id=?")
                .map_err(sql_err)?;
            let mut rows = stmt.query([correction_id]).map_err(sql_err)?;
            if let Some(row) = rows.next().map_err(sql_err)? {
                let compression: i64 = row.get(0).map_err(sql_err)?;
                let data: Vec<u8> = row.get(1).map_err(sql_err)?;
                let mut data = self.decode_tile(compression, &data, opt_w, opt_h)?;
                let bpp = bytes_per_pixel(m.pixel_type);

                // found 4 channels, remove extras to match channel count
                if data.len() == opt_w * opt_h * bpp * 4 {
                    let src_stride = bpp * 4;
                    let dest_stride = bpp * rgb_channel_count(m);
                    let mut tmp = vec![0u8; opt_w * opt_h * dest_stride];
                    if m.is_interleaved {
                        for pix in 0..(opt_w * opt_h) {
                            let s = pix * src_stride;
                            let d = pix * dest_stride;
                            if s + dest_stride <= data.len() && d + dest_stride <= tmp.len() {
                                tmp[d..d + dest_stride]
                                    .copy_from_slice(&data[s..s + dest_stride]);
                            }
                        }
                    } else {
                        let n = tmp.len().min(data.len());
                        tmp[..n].copy_from_slice(&data[..n]);
                    }
                    data = tmp;
                }

                let correction = Region::new(0, 0, opt_w as i64, opt_h as i64);
                self.copy_region(&correction, &data, dest, buf, opt_w as i64, m);
            }
            Ok(())
        }
    }

    fn sql_err(e: rusqlite::Error) -> BioFormatsError {
        BioFormatsError::Format(format!("TissueFAXS SQLite error: {e}"))
    }

    /// Java `openConnection` (read-only SQLite connection).
    fn open_connection(file: &Path) -> Result<Connection> {
        Connection::open_with_flags(
            file,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
        )
        .map_err(|e| {
            BioFormatsError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("Could not read from database {}: {e}", file.display()),
            ))
        })
    }

    // silence unused-constant warnings when only part of the module is exercised
    #[allow(dead_code)]
    const _WAVE_RANGE: (i64, i64) = (WAVE_MIN, WAVE_MAX);
}

// ---------------------------------------------------------------------------
// 7. Mikroscan
// ---------------------------------------------------------------------------

/// Mikroscan TIFF (`.tif`/`.tiff`).
///
/// Ported from the upstream Java `MikroscanTiffReader`, which extends
/// `SVSReader`. The only behavior `MikroscanTiffReader` adds over `SVSReader`
/// is the `isThisType(name, open)` override: a valid TIFF whose first IFD's
/// `IMAGE_DESCRIPTION` starts with `"Mikroscan Image"`. All series/pyramid
/// assembly is inherited from `SVSReader` (here: `regroup_as_svs_pyramid`).
pub struct MikroscanTiffReader {
    inner: crate::tiff::TiffReader,
}

/// `MikroscanTiffReader.MIKROSCAN_IMAGE_DESCRIPTION_PREFIX`.
const MIKROSCAN_IMAGE_DESCRIPTION_PREFIX: &str = "Mikroscan Image";

impl MikroscanTiffReader {
    pub fn new() -> Self {
        MikroscanTiffReader {
            inner: crate::tiff::TiffReader::new(),
        }
    }

    /// Mirror of `MikroscanTiffReader.isThisType(String, boolean)`: the first
    /// IFD's IMAGE_DESCRIPTION must start with "Mikroscan Image".
    fn is_mikroscan_description(&self) -> bool {
        self.inner
            .ifd(0)
            .and_then(|ifd| ifd.get_str(crate::tiff::ifd::tag::IMAGE_DESCRIPTION))
            .map(|d| d.starts_with(MIKROSCAN_IMAGE_DESCRIPTION_PREFIX))
            .unwrap_or(false)
    }
}

impl Default for MikroscanTiffReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for MikroscanTiffReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("tif") | Some("tiff"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.inner.close()?;
        self.inner.set_id(path)?;
        // SVSReader stores its pyramid as the main IFD chain; regroup into a
        // single multi-resolution series (+ label/macro), mirroring SVSReader.
        let _ = self.inner.regroup_as_svs_pyramid();
        let detected = self.is_mikroscan_description();
        for series in self.inner.series_list_mut() {
            series.metadata.series_metadata.insert(
                "hcs2.wrapper".to_string(),
                MetadataValue::String("MikroscanTiffReader".to_string()),
            );
            if detected {
                series.metadata.series_metadata.insert(
                    "mikroscan.detected".to_string(),
                    MetadataValue::Bool(true),
                );
            }
        }
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.inner.close()
    }

    fn series_count(&self) -> usize {
        self.inner.series_count()
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.inner.series_count() == 0 {
            return Err(BioFormatsError::NotInitialized);
        }
        self.inner.set_series(s)
    }

    fn series(&self) -> usize {
        self.inner.series()
    }

    fn metadata(&self) -> &ImageMetadata {
        self.inner.metadata()
    }

    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        self.inner.open_bytes(p)
    }

    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        self.inner.open_bytes_region(p, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        self.inner.open_thumb_bytes(p)
    }

    fn resolution_count(&self) -> usize {
        self.inner.resolution_count()
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        self.inner.set_resolution(level)
    }

    fn resolution(&self) -> usize {
        self.inner.resolution()
    }
}

// ===========================================================================
// HCS index-file readers (parse index, assemble plate/well/field, delegate
// pixel I/O to TiffReader)
// ===========================================================================

/// Placement of one source tile within a reconstructed (stitched/montaged)
/// plane. The tile is read from `filename` (IFD `file_index`); a sub-rectangle
/// of the source `(src_x, src_y, src_w, src_h)` is copied into the destination
/// plane at offset `(dst_x, dst_y)`.
///
/// For a plain 1:1 plane there is a single `Tile` with `dst_x = dst_y = 0`,
/// `src_x = src_y = 0` and `src_w/src_h` set to the source dimensions (or 0,
/// meaning "use the whole source plane").
#[derive(Clone)]
struct Tile {
    filename: PathBuf,
    file_index: u32,
    /// Sub-rectangle within the source TIFF plane. `src_w == 0 || src_h == 0`
    /// means "use the whole source plane" (the common 1:1 case).
    src_x: u32,
    src_y: u32,
    src_w: u32,
    src_h: u32,
    /// Destination offset within the reconstructed plane.
    dst_x: u32,
    dst_y: u32,
}

/// Reference to a single image plane: the set of source tiles that make it up.
///
/// Simple readers use exactly one whole-plane tile; CellVoyager (multi-tile
/// area stitching) and BD Pathway (montage field splitting) use cropped /
/// offset tiles.
#[derive(Clone, Default)]
struct PlaneRef {
    tiles: Vec<Tile>,
}

impl PlaneRef {
    /// A 1:1 plane backed by the whole source plane of `filename`.
    fn whole(filename: PathBuf, file_index: u32) -> Self {
        PlaneRef {
            tiles: vec![Tile {
                filename,
                file_index,
                src_x: 0,
                src_y: 0,
                src_w: 0,
                src_h: 0,
                dst_x: 0,
                dst_y: 0,
            }],
        }
    }
}

/// Compute the plane index for (z, c, t) given dimension order and sizes.
///
/// Mirrors `loci.formats.FormatTools.getIndex`.
fn get_index(
    order: DimensionOrder,
    size_z: u32,
    size_c: u32,
    size_t: u32,
    z: u32,
    c: u32,
    t: u32,
) -> u32 {
    let (s0, s1) = match order {
        DimensionOrder::XYZCT => (size_z, size_c),
        DimensionOrder::XYZTC => (size_z, size_t),
        DimensionOrder::XYCZT => (size_c, size_z),
        DimensionOrder::XYCTZ => (size_c, size_t),
        DimensionOrder::XYTZC => (size_t, size_z),
        DimensionOrder::XYTCZ => (size_t, size_c),
    };
    // value of the three dims in the order they vary (fastest first)
    let (v0, v1, v2) = match order {
        DimensionOrder::XYZCT => (z, c, t),
        DimensionOrder::XYZTC => (z, t, c),
        DimensionOrder::XYCZT => (c, z, t),
        DimensionOrder::XYCTZ => (c, t, z),
        DimensionOrder::XYTZC => (t, z, c),
        DimensionOrder::XYTCZ => (t, c, z),
    };
    v0 + v1 * s0 + v2 * s0 * s1
}

/// Decompose `index` into (z, c, t) given dimension order and sizes.
/// Mirrors `loci.formats.FormatTools.getZCTCoords`.
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

/// Generic assembled-HCS reader state shared by the index-based readers.
///
/// Each parser produces a list of per-series `ImageMetadata` plus a parallel
/// list of per-series plane references. Pixel I/O is delegated to a
/// `TiffReader` opened on the referenced file.
struct HcsAssembly {
    series: Vec<ImageMetadata>,
    /// `planes[series][plane_index]` -> reference to the backing TIFF.
    planes: Vec<Vec<PlaneRef>>,
    current_series: usize,
    tiff_reader: crate::tiff::TiffReader,
    tiff_loaded_path: Option<PathBuf>,
}

impl HcsAssembly {
    fn new() -> Self {
        HcsAssembly {
            series: Vec::new(),
            planes: Vec::new(),
            current_series: 0,
            tiff_reader: crate::tiff::TiffReader::new(),
            tiff_loaded_path: None,
        }
    }

    fn meta(&self) -> Result<&ImageMetadata> {
        self.series
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)
    }

    fn plane_bytes(meta: &ImageMetadata) -> usize {
        meta.size_x as usize * meta.size_y as usize * meta.pixel_type.bytes_per_sample()
    }

    /// Ensure the backing TIFF for `path` is loaded, then position it at `file_index`.
    fn ensure_loaded(&mut self, path: &Path) -> Result<()> {
        let need_load = self
            .tiff_loaded_path
            .as_deref()
            .map(|p| p != path)
            .unwrap_or(true);
        if need_load {
            let _ = self.tiff_reader.close();
            self.tiff_reader.set_id(path)?;
            self.tiff_loaded_path = Some(path.to_path_buf());
        }
        Ok(())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta()?.clone();
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let nbytes = Self::plane_bytes(&meta);
        let plane = self
            .planes
            .get(self.current_series)
            .and_then(|p| p.get(plane_index as usize))
            .cloned()
            .unwrap_or_default();

        if plane.tiles.is_empty() {
            // Missing plane: return a blank (fill 0) buffer, like Java's Arrays.fill.
            return Ok(vec![0u8; nbytes]);
        }

        let bps = meta.pixel_type.bytes_per_sample();
        let dst_w = meta.size_x as usize;
        let dst_h = meta.size_y as usize;
        let dst_row = dst_w * bps;

        // Fast path: a single whole-plane tile placed at the origin (the common
        // 1:1 case). Read the whole source plane and pad/truncate as before.
        if plane.tiles.len() == 1 {
            let t = &plane.tiles[0];
            if t.dst_x == 0
                && t.dst_y == 0
                && t.src_x == 0
                && t.src_y == 0
                && t.src_w == 0
                && t.src_h == 0
            {
                self.ensure_loaded(&t.filename)?;
                let buf = self.tiff_reader.open_bytes(t.file_index)?;
                if buf.len() == nbytes {
                    return Ok(buf);
                }
                let mut out = vec![0u8; nbytes];
                let n = buf.len().min(nbytes);
                out[..n].copy_from_slice(&buf[..n]);
                return Ok(out);
            }
        }

        // General path: composite each tile's sub-rectangle into the plane.
        let mut out = vec![0u8; nbytes];
        for t in &plane.tiles {
            self.ensure_loaded(&t.filename)?;
            // Source region: explicit crop, or the whole source plane.
            let (sx, sy, sw, sh) = if t.src_w == 0 || t.src_h == 0 {
                let sm = self.tiff_reader.metadata();
                (0, 0, sm.size_x, sm.size_y)
            } else {
                (t.src_x, t.src_y, t.src_w, t.src_h)
            };
            // Clip to the destination plane.
            let dx = t.dst_x as usize;
            let dy = t.dst_y as usize;
            if dx >= dst_w || dy >= dst_h {
                continue;
            }
            let copy_w = (sw as usize).min(dst_w - dx);
            let copy_h = (sh as usize).min(dst_h - dy);
            if copy_w == 0 || copy_h == 0 {
                continue;
            }
            let region = self.tiff_reader.open_bytes_region(
                t.file_index,
                sx,
                sy,
                copy_w as u32,
                copy_h as u32,
            )?;
            let src_row = copy_w * bps;
            let expected = src_row * copy_h;
            if region.len() < expected {
                return Err(BioFormatsError::Format(format!(
                    "HCS companion tile {} returned {} bytes for a {} byte region",
                    t.filename.display(),
                    region.len(),
                    expected
                )));
            }
            for row in 0..copy_h {
                let s = row * src_row;
                let d = (dy + row) * dst_row + dx * bps;
                if d + src_row > out.len() {
                    break;
                }
                out[d..d + src_row].copy_from_slice(&region[s..s + src_row]);
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
        let meta = self.meta()?;
        crop_full_plane("BD Pathway", &full, meta, 1, x, y, w, h)
    }

    fn validate(&self, format_name: &str) -> Result<()> {
        if self.series.is_empty() {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "{format_name}: no series assembled"
            )));
        }
        if self.planes.len() != self.series.len() {
            return Err(BioFormatsError::Format(format!(
                "{format_name}: series/plane table length mismatch"
            )));
        }

        let mut saw_payload = false;
        for (series_index, meta) in self.series.iter().enumerate() {
            if meta.size_x == 0
                || meta.size_y == 0
                || meta.size_z == 0
                || meta.size_c == 0
                || meta.size_t == 0
            {
                return Err(BioFormatsError::Format(format!(
                    "{format_name}: series {series_index} has non-positive dimensions"
                )));
            }
            let expected = meta
                .size_z
                .checked_mul(meta.size_c)
                .and_then(|v| v.checked_mul(meta.size_t))
                .ok_or_else(|| {
                    BioFormatsError::Format(format!(
                        "{format_name}: series {series_index} plane count overflows"
                    ))
                })?;
            if meta.image_count != expected {
                return Err(BioFormatsError::Format(format!(
                    "{format_name}: series {series_index} image_count {} does not match dimensions {expected}",
                    meta.image_count
                )));
            }
            let planes = self.planes.get(series_index).ok_or_else(|| {
                BioFormatsError::Format(format!("{format_name}: missing plane table"))
            })?;
            if planes.len() < expected as usize {
                return Err(BioFormatsError::Format(format!(
                    "{format_name}: series {series_index} has {} plane slots for {expected} planes",
                    planes.len()
                )));
            }
            for (plane_index, plane) in planes.iter().take(expected as usize).enumerate() {
                for tile in &plane.tiles {
                    saw_payload = true;
                    let mut tr = crate::tiff::TiffReader::new();
                    tr.set_id(&tile.filename).map_err(|e| {
                        BioFormatsError::Format(format!(
                            "{format_name}: companion TIFF {} could not be initialized: {e}",
                            tile.filename.display()
                        ))
                    })?;
                    let tm = tr.metadata();
                    if tm.size_x == 0 || tm.size_y == 0 || tm.image_count == 0 {
                        return Err(BioFormatsError::Format(format!(
                            "{format_name}: companion TIFF {} has invalid image metadata",
                            tile.filename.display()
                        )));
                    }
                    if tile.file_index >= tm.image_count {
                        return Err(BioFormatsError::Format(format!(
                            "{format_name}: plane {plane_index} references TIFF page {} in {} but only {} page(s) are available",
                            tile.file_index,
                            tile.filename.display(),
                            tm.image_count
                        )));
                    }
                    let src_w = if tile.src_w == 0 {
                        tm.size_x
                    } else {
                        tile.src_w
                    };
                    let src_h = if tile.src_h == 0 {
                        tm.size_y
                    } else {
                        tile.src_h
                    };
                    let src_end_x = tile.src_x.checked_add(src_w).ok_or_else(|| {
                        BioFormatsError::Format(format!(
                            "{format_name}: source tile X range overflows for {}",
                            tile.filename.display()
                        ))
                    })?;
                    let src_end_y = tile.src_y.checked_add(src_h).ok_or_else(|| {
                        BioFormatsError::Format(format!(
                            "{format_name}: source tile Y range overflows for {}",
                            tile.filename.display()
                        ))
                    })?;
                    if src_end_x > tm.size_x || src_end_y > tm.size_y {
                        return Err(BioFormatsError::Format(format!(
                            "{format_name}: source tile region {}x{} at {},{} exceeds companion TIFF {} dimensions {}x{}",
                            src_w,
                            src_h,
                            tile.src_x,
                            tile.src_y,
                            tile.filename.display(),
                            tm.size_x,
                            tm.size_y
                        )));
                    }
                    let _ = tr.close();
                }
            }
        }
        if !saw_payload {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "{format_name}: index does not reference any readable companion TIFF payload"
            )));
        }
        Ok(())
    }
}

/// Build an `ImageMetadata` for an assembled HCS series.
#[allow(clippy::too_many_arguments)]
fn make_series_meta(
    size_x: u32,
    size_y: u32,
    size_z: u32,
    size_c: u32,
    size_t: u32,
    pixel_type: PixelType,
    bits: u8,
    little_endian: bool,
    order: DimensionOrder,
    format: &str,
) -> ImageMetadata {
    let mut meta_map = HashMap::new();
    meta_map.insert(
        "format".to_string(),
        MetadataValue::String(format.to_string()),
    );
    ImageMetadata {
        size_x,
        size_y,
        size_z,
        size_c,
        size_t,
        pixel_type,
        bits_per_pixel: bits,
        image_count: size_z * size_c * size_t,
        dimension_order: order,
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
    }
}

/// Probe a TIFF for (size_x, size_y, pixel_type, bits, little_endian).
/// Returns `None` if the file cannot be opened.
fn probe_tiff(path: &Path) -> Option<(u32, u32, PixelType, u8, bool)> {
    let mut tr = crate::tiff::TiffReader::new();
    if tr.set_id(path).is_ok() {
        let m = tr.metadata();
        let out = (
            m.size_x,
            m.size_y,
            m.pixel_type,
            m.bits_per_pixel,
            m.is_little_endian,
        );
        let _ = tr.close();
        Some(out)
    } else {
        None
    }
}

/// Macro generating the full `FormatReader` impl that delegates pixel I/O to an
/// inner `HcsAssembly`. Detection (`is_this_type_by_name`) and parsing
/// (`set_id`) bodies are supplied by each reader.
///
/// `detect` is a `fn(&Path) -> bool`; `parse` is a `fn(&Path) -> Result<HcsAssembly>`.
macro_rules! impl_assembled_reader {
    ($name:ident, detect = $detect:expr, parse = $parse:expr) => {
        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl FormatReader for $name {
            fn is_this_type_by_name(&self, path: &Path) -> bool {
                let detect: fn(&Path) -> bool = $detect;
                detect(path)
            }

            fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
                false
            }

            fn set_id(&mut self, path: &Path) -> Result<()> {
                let parse: fn(&Path) -> Result<HcsAssembly> = $parse;
                self.asm = HcsAssembly::new();
                let asm = parse(path)?;
                asm.validate(stringify!($name))?;
                self.asm = asm;
                Ok(())
            }

            fn close(&mut self) -> Result<()> {
                self.asm = HcsAssembly::new();
                Ok(())
            }

            fn series_count(&self) -> usize {
                self.asm.series.len()
            }

            fn set_series(&mut self, s: usize) -> Result<()> {
                if self.asm.series.is_empty() {
                    Err(BioFormatsError::NotInitialized)
                } else if s >= self.asm.series.len() {
                    Err(BioFormatsError::SeriesOutOfRange(s))
                } else {
                    self.asm.current_series = s;
                    Ok(())
                }
            }

            fn series(&self) -> usize {
                self.asm.current_series
            }

            fn metadata(&self) -> &ImageMetadata {
                self.asm
                    .series
                    .get(self.asm.current_series)
                    .unwrap_or(crate::common::reader::uninitialized_metadata())
            }

            fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
                self.asm.open_bytes(plane_index)
            }

            fn open_bytes_region(
                &mut self,
                plane_index: u32,
                x: u32,
                y: u32,
                w: u32,
                h: u32,
            ) -> Result<Vec<u8>> {
                self.asm.open_bytes_region(plane_index, x, y, w, h)
            }

            fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
                let meta = self.asm.meta()?;
                let tw = meta.size_x.min(256);
                let th = meta.size_y.min(256);
                let tx = (meta.size_x - tw) / 2;
                let ty = (meta.size_y - th) / 2;
                self.asm.open_bytes_region(plane_index, tx, ty, tw, th)
            }
        }
    };
}

// ---------------------------------------------------------------------------
// 8. BD Biosciences Pathway (.exp — INI-style Experiment file)
// ---------------------------------------------------------------------------

/// BD Biosciences Pathway HCS reader (`.exp`).
///
/// Ported from the upstream Java `BDReader`. Reads the INI-style
/// `Experiment.exp` plus `.plt`/`.xyz`/`.dye` companion files, scans `Well NN`
/// directories for `<channel> - nNNNNNN.tif` images, and assembles one series
/// per well × field. Montaged acquisitions store several fields packed into a
/// single TIFF, which are split out in `open_bytes`.
pub struct BdReader {
    asm: HcsAssembly,
}

impl BdReader {
    pub fn new() -> Self {
        BdReader {
            asm: HcsAssembly::new(),
        }
    }
}

impl_assembled_reader!(
    BdReader,
    detect = |path| {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("exp"))
    },
    parse = bd::parse
);

// ---------------------------------------------------------------------------
// 9. PerkinElmer Columbus (.xml — MeasurementIndex.ColumbusIDX.xml)
// ---------------------------------------------------------------------------

/// PerkinElmer Columbus HCS reader (`.xml`).
///
/// Ported from the upstream Java `ColumbusReader`. Parses the
/// `MeasurementIndex.ColumbusIDX.xml` plate index plus per-timepoint
/// `*.columbusidx.xml` image lists, and assembles one series per well × field.
pub struct ColumbusReader {
    asm: HcsAssembly,
}

impl ColumbusReader {
    pub fn new() -> Self {
        ColumbusReader {
            asm: HcsAssembly::new(),
        }
    }
}

impl_assembled_reader!(
    ColumbusReader,
    detect = |path| {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        if !matches!(ext.as_deref(), Some("xml")) {
            return false;
        }
        // Columbus index files are named MeasurementIndex.ColumbusIDX.xml; also
        // accept any .xml whose content carries the Columbus magic string.
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if name == "measurementindex.columbusidx.xml" || name.ends_with("columbusidx.xml") {
            return true;
        }
        if let Ok(data) = std::fs::read(path) {
            let snippet = std::str::from_utf8(&data[..data.len().min(1024)]).unwrap_or("");
            return snippet.contains("ColumbusMeasurementIndex");
        }
        false
    },
    parse = columbus::parse
);

// ---------------------------------------------------------------------------
// 10. PerkinElmer Operetta (.xml — Index.idx.xml)
// ---------------------------------------------------------------------------

/// PerkinElmer Operetta HCS reader (`.xml`).
///
/// Ported from the upstream Java `OperettaReader`. Parses `Index.idx.xml`
/// (Harmony/Operetta/Phenix) and assembles one series per well × field with
/// per-plane Z/C/T → TIFF mapping.
pub struct OperettaReader {
    asm: HcsAssembly,
}

impl OperettaReader {
    pub fn new() -> Self {
        OperettaReader {
            asm: HcsAssembly::new(),
        }
    }
}

impl_assembled_reader!(
    OperettaReader,
    detect = |path| {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        if !matches!(ext.as_deref(), Some("xml")) {
            return false;
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if matches!(
            name.as_str(),
            "index.idx.xml" | "index.ref.xml" | "index.xml"
        ) {
            return true;
        }
        if let Ok(data) = std::fs::read(path) {
            let snippet = std::str::from_utf8(&data[..data.len().min(1024)]).unwrap_or("");
            return snippet.contains("Harmony") || snippet.contains("Operett");
        }
        false
    },
    parse = operetta::parse
);

// ---------------------------------------------------------------------------
// 11. Olympus ScanR (.xml — experiment_descriptor.xml)
// ---------------------------------------------------------------------------

/// Olympus ScanR HCS reader (`.xml`).
///
/// Ported from the upstream Java `ScanrReader`. Parses
/// `experiment_descriptor.xml`, derives plate/well/field/channel/Z/T geometry,
/// then matches the `data/` TIFF filenames (`...W#####...P#####...Z#####...T#####...<channel>...`)
/// into one series per well × field.
pub struct ScanrReader {
    asm: HcsAssembly,
}

impl ScanrReader {
    pub fn new() -> Self {
        ScanrReader {
            asm: HcsAssembly::new(),
        }
    }
}

impl_assembled_reader!(
    ScanrReader,
    detect = |path| {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        if !matches!(ext.as_deref(), Some("xml")) {
            return false;
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        name == "experiment_descriptor.xml"
    },
    parse = scanr::parse
);

// ---------------------------------------------------------------------------
// 12. Yokogawa CellVoyager (.mes, .mlf, MeasurementResult.xml)
// ---------------------------------------------------------------------------

/// Yokogawa CellVoyager HCS reader (`.mes`, `.mlf`, `MeasurementResult.xml`).
///
/// Port of the upstream Java `CellVoyagerReader`. Parses
/// `MeasurementResult.xml` for channel/well/field/timepoint geometry and
/// stitches each area's `Image/W#F###T####Z##C#.tif` field tiles on the fly,
/// pasting each tile at its computed pixel offset (see module docs).
pub struct CellVoyagerReader {
    asm: HcsAssembly,
}

impl CellVoyagerReader {
    pub fn new() -> Self {
        CellVoyagerReader {
            asm: HcsAssembly::new(),
        }
    }
}

impl_assembled_reader!(
    CellVoyagerReader,
    detect = |path| {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        if matches!(ext.as_deref(), Some("mes") | Some("mlf")) {
            return true;
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        name == "measurementresult.xml"
    },
    parse = cellvoyager::parse
);

// ---------------------------------------------------------------------------
// 14. GE InCell 3000 (.frm — RLE-compressed binary frame)
// ---------------------------------------------------------------------------

/// GE InCell 3000 reader (`.frm`).
///
/// Ported from the upstream Java `InCell3000Reader`. A `.frm` file is a single
/// RLE-compressed 16-bit frame with a small binary header (NOT an XML index).
/// The XDCE-based GE InCell datasets are handled by `crate::formats::incell`.
pub struct InCell3000Reader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    pixels_offset: u64,
}

impl InCell3000Reader {
    pub fn new() -> Self {
        InCell3000Reader {
            path: None,
            meta: None,
            pixels_offset: 0,
        }
    }
}

impl Default for InCell3000Reader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for InCell3000Reader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("frm") | Some("xdce"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        // `.xdce` is the InCell XML index, handled by incell::InCellReader. This
        // reader only decodes the binary `.frm` frame; for an `.xdce` it falls
        // through with the historical "no TIFF" rejection so the registry's
        // companion-less rejection contract is preserved.
        let is_xdce = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("xdce"))
            .unwrap_or(false);
        if is_xdce {
            return Err(BioFormatsError::Format(
                "GE InCell 3000: no TIFF image files found referenced in index".to_string(),
            ));
        }

        // Header layout (little-endian), per Java InCell3000Reader.initFile:
        //   int16 pixelsOffset
        //   int16 sizeX
        //   int16 nLines  -> numPlanes = nLines % 32; sizeY = (nLines - numPlanes)/numPlanes
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        if data.len() < 6 {
            return Err(BioFormatsError::Format(
                "InCell 3000: file too small for header".to_string(),
            ));
        }
        let rd16 = |off: usize| i16::from_le_bytes([data[off], data[off + 1]]) as i64;
        let pixels_offset = rd16(0);
        let size_x = rd16(2);
        let n_lines = rd16(4);
        let num_planes = n_lines.rem_euclid(32);
        let size_y = if num_planes != 0 {
            (n_lines - num_planes) / num_planes
        } else {
            0
        };
        if size_x <= 0 || size_y <= 0 {
            return Err(BioFormatsError::Format(format!(
                "InCell 3000: invalid dimensions {size_x}x{size_y}"
            )));
        }

        let mut meta_map = HashMap::new();
        meta_map.insert(
            "format".to_string(),
            MetadataValue::String("InCell 3000".to_string()),
        );
        self.meta = Some(ImageMetadata {
            size_x: size_x as u32,
            size_y: size_y as u32,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type: PixelType::Uint16,
            bits_per_pixel: 16,
            image_count: 1,
            dimension_order: DimensionOrder::XYCZT,
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
        });
        self.pixels_offset = pixels_offset.max(0) as u64;
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.pixels_offset = 0;
        Ok(())
    }

    fn series_count(&self) -> usize {
        usize::from(self.meta.is_some())
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.meta.is_none() {
            Err(BioFormatsError::NotInitialized)
        } else if s != 0 {
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

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let size_x = meta.size_x as usize;
        let size_y = meta.size_y as usize;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;

        // Decompress the custom RLE stream, mirroring Java openBytes.
        // totalElements is measured in BYTES (sizeX*sizeY*2) in the Java code.
        let total_bytes = size_x
            .checked_mul(size_y)
            .and_then(|v| v.checked_mul(2))
            .ok_or_else(|| BioFormatsError::Format("InCell 3000 plane size overflows".into()))?;
        let mut out: Vec<u8> = Vec::with_capacity(total_bytes);
        let mut pos = self.pixels_offset as usize;
        let rd16 = |buf: &[u8], off: usize| -> Option<u16> {
            if off + 2 <= buf.len() {
                Some(u16::from_le_bytes([buf[off], buf[off + 1]]))
            } else {
                None
            }
        };
        while out.len() < total_bytes {
            let Some(pixel) = rd16(&data, pos) else { break };
            pos += 2;
            if pixel as i64 > 32768 {
                let count = (pixel as i64 - 32768) as usize;
                let Some(start_value) = rd16(&data, pos) else {
                    break;
                };
                pos += 2;
                let fp = pos;
                for i in 0..count {
                    let off = fp + 2 * (i / 3);
                    let Some(raw) = rd16(&data, off) else { break };
                    let int_ofs = if i % 3 != 0 { raw >> 5 } else { raw };
                    let temp_val = (start_value as i64 + (int_ofs as i64 & 31)) as u16;
                    out.extend_from_slice(&temp_val.to_le_bytes());
                    if out.len() >= total_bytes {
                        break;
                    }
                }
                // advance over the packed run: ceil(count/3) shorts
                let consumed = 2 * count.div_ceil(3);
                pos = fp + consumed;
            } else {
                out.extend_from_slice(&pixel.to_le_bytes());
            }
        }
        if out.len() < total_bytes {
            return Err(BioFormatsError::InvalidData(format!(
                "InCell 3000 decoded {} bytes, expected {total_bytes}",
                out.len()
            )));
        } else if out.len() > total_bytes {
            out.truncate(total_bytes);
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
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        crop_full_plane("RCPNL", &full, meta, 1, x, y, w, h)
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

// ---------------------------------------------------------------------------
// 15. RCPNL (.rcpnl — Rarecyte multi-page OME-TIFF tile scan)
// ---------------------------------------------------------------------------

/// RCPNL reader (`.rcpnl`).
///
/// Rarecyte `.rcpnl` files are multi-image (OME-)TIFFs. Upstream Bio-Formats
/// reads them via the generic OME-TIFF reader; there is no dedicated Java
/// `RcpnlReader`. We therefore delegate directly to `TiffReader`, which already
/// exposes the per-IFD series and OME metadata.
pub struct RcpnlReader {
    inner: crate::tiff::TiffReader,
}

impl RcpnlReader {
    pub fn new() -> Self {
        RcpnlReader {
            inner: crate::tiff::TiffReader::new(),
        }
    }
}

impl Default for RcpnlReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for RcpnlReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("rcpnl"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.inner.set_id(path)
    }

    fn close(&mut self) -> Result<()> {
        self.inner.close()
    }

    fn series_count(&self) -> usize {
        self.inner.series_count()
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        self.inner.set_series(s)
    }

    fn series(&self) -> usize {
        self.inner.series()
    }

    fn metadata(&self) -> &ImageMetadata {
        self.inner.metadata()
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        self.inner.open_bytes(plane_index)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        self.inner.open_bytes_region(plane_index, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        self.inner.open_thumb_bytes(plane_index)
    }

    fn resolution_count(&self) -> usize {
        self.inner.resolution_count()
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        self.inner.set_resolution(level)
    }
}

// ---------------------------------------------------------------------------
// 13. Tecan plate reader (.asc — tab-separated plate data)
// ---------------------------------------------------------------------------

/// Tecan plate reader (`.asc`).
///
/// Reads a tab-separated `.asc` text file containing plate reader measurements.
/// Each row corresponds to a plate row and each column to a plate column. Values
/// are stored as `Float32` pixel data in a 2-D image.
pub struct TecanReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    pixel_data: Vec<u8>,
}

impl TecanReader {
    pub fn new() -> Self {
        TecanReader {
            path: None,
            meta: None,
            pixel_data: Vec::new(),
        }
    }
}

impl Default for TecanReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for TecanReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("asc"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let text = std::fs::read_to_string(path).map_err(BioFormatsError::Io)?;
        let mut rows: Vec<Vec<f32>> = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            // Tecan .asc files are tab-separated; also accept spaces.
            let mut cells: Vec<f32> = Vec::new();
            for cell in line
                .split(|c: char| c == '\t' || c == ' ')
                .filter(|s| !s.is_empty())
            {
                let value = cell.trim().parse::<f64>().map_err(|_| {
                    BioFormatsError::Format(format!("Tecan: non-numeric cell {cell:?}"))
                })?;
                cells.push(value as f32);
            }
            if !cells.is_empty() {
                rows.push(cells);
            }
        }
        if rows.is_empty() {
            return Err(BioFormatsError::Format(
                "Tecan: .asc file contains no numeric data".to_string(),
            ));
        }
        let height = rows.len() as u32;
        let width = rows[0].len();
        if rows.iter().any(|row| row.len() != width) {
            return Err(BioFormatsError::Format(
                "Tecan: .asc rows have inconsistent column counts".to_string(),
            ));
        }
        let width = width as u32;
        // Build Float32 pixel buffer (row-major).
        let mut pixel_data = Vec::with_capacity((width * height * 4) as usize);
        for row in &rows {
            for &val in row {
                pixel_data.extend_from_slice(&val.to_le_bytes());
            }
        }
        let mut series_metadata = HashMap::new();
        series_metadata.insert(
            "format".to_string(),
            MetadataValue::String("Tecan".to_string()),
        );
        series_metadata.insert("plate_rows".to_string(), MetadataValue::Int(height as i64));
        series_metadata.insert(
            "plate_columns".to_string(),
            MetadataValue::Int(width as i64),
        );

        self.path = Some(path.to_path_buf());
        self.pixel_data = pixel_data;
        self.meta = Some(ImageMetadata {
            size_x: width,
            size_y: height,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type: PixelType::Float32,
            bits_per_pixel: 32,
            image_count: 1,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: true,
            resolution_count: 1,
            series_metadata,
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        });
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.pixel_data.clear();
        Ok(())
    }

    fn series_count(&self) -> usize {
        usize::from(self.meta.is_some())
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.meta.is_none() {
            Err(BioFormatsError::NotInitialized)
        } else if s != 0 {
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

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        Ok(self.pixel_data.clone())
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        crop_full_plane("Tecan", &self.pixel_data, meta, 1, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn resolution_count(&self) -> usize {
        1
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if level != 0 {
            Err(BioFormatsError::Format(format!(
                "resolution {} out of range",
                level
            )))
        } else {
            Ok(())
        }
    }
}

// ===========================================================================
// Shared XML parsing helpers for the index-based HCS readers
// ===========================================================================

mod xmlutil {
    use quick_xml::events::{BytesEnd, BytesStart, Event};
    use quick_xml::Reader as XmlReader;

    /// Get an attribute value by (case-sensitive) local name.
    pub fn attr(e: &BytesStart, name: &str) -> Option<String> {
        for a in e.attributes().flatten() {
            if a.key.as_ref() == name.as_bytes() {
                return Some(String::from_utf8_lossy(&a.value).to_string());
            }
        }
        None
    }

    /// Anything that exposes a qualified element name (`BytesStart`/`BytesEnd`).
    pub trait HasName {
        fn qname_bytes(&self) -> Vec<u8>;
    }
    impl HasName for BytesStart<'_> {
        fn qname_bytes(&self) -> Vec<u8> {
            self.name().as_ref().to_vec()
        }
    }
    impl HasName for BytesEnd<'_> {
        fn qname_bytes(&self) -> Vec<u8> {
            self.name().as_ref().to_vec()
        }
    }

    /// The local element name (after any namespace prefix) as an owned String.
    pub fn local_name<E: HasName>(e: &E) -> String {
        let full = e.qname_bytes();
        let local = match full.iter().position(|&b| b == b':') {
            Some(i) => &full[i + 1..],
            None => &full[..],
        };
        String::from_utf8_lossy(local).to_string()
    }

    /// Run a SAX-style callback over an XML string. (Currently unused by the
    /// readers, which run their own stateful passes; retained as a utility.)
    #[allow(dead_code)]
    pub fn walk<S, T, E>(xml: &str, mut on_start: S, mut on_text: T, mut on_end: E)
    where
        S: FnMut(&str, &BytesStart),
        T: FnMut(&str),
        E: FnMut(&str),
    {
        let mut reader = XmlReader::from_str(xml);
        reader.config_mut().trim_text(false);
        let mut buf_text = String::new();
        loop {
            match reader.read_event() {
                Ok(Event::Start(ref e)) => {
                    buf_text.clear();
                    let ln = local_name(e);
                    on_start(&ln, e);
                }
                Ok(Event::Empty(ref e)) => {
                    let ln = local_name(e);
                    on_start(&ln, e);
                    on_text("");
                    on_end(&ln);
                }
                Ok(Event::Text(ref t)) => {
                    if let Ok(s) = t.unescape() {
                        buf_text.push_str(&s);
                    }
                }
                Ok(Event::CData(ref t)) => {
                    buf_text.push_str(&String::from_utf8_lossy(t.as_ref()));
                }
                Ok(Event::End(ref e)) => {
                    let ln = local_name(e);
                    on_text(&buf_text);
                    buf_text.clear();
                    on_end(&ln);
                }
                Ok(Event::Eof) => break,
                Err(_) => break,
                _ => {}
            }
        }
    }
}

/// Java FormatTools.getWellName(row, col): row letter(s) + 1-based column.
fn well_name(row: i32, col: i32) -> String {
    // Row 0 -> 'A', 25 -> 'Z', 26 -> 'AA', etc.
    let mut r = row;
    let mut letters = String::new();
    loop {
        let rem = (r % 26) as u8;
        letters.insert(0, (b'A' + rem) as char);
        r = r / 26 - 1;
        if r < 0 {
            break;
        }
    }
    // Java FormatTools.getWellName zero-pads the 1-based column to a minimum
    // of 2 digits (FormatTools.java:1372-1376): "A1" -> "A01".
    format!("{}{:02}", letters, col + 1)
}

// ===========================================================================
// Operetta parser (Index.idx.xml)  -- port of OperettaReader.initFile
// ===========================================================================

mod operetta {
    use super::*;
    use std::collections::HashMap as Map;

    #[derive(Clone, Default)]
    struct Plane {
        filename: Option<PathBuf>,
        row: i32,
        col: i32,
        field: i32,
        z: i32,
        t: i32,
        c: i32,
        x: u32,
        y: u32,
        // Per-plane scalar metadata, mirroring OperettaReader.Plane.
        channel_name: Option<String>,
        /// ImageResolutionX/Y in micrometers (Java stores metres * 1e6).
        resolution_x: Option<f64>,
        resolution_y: Option<f64>,
        /// PositionX/Y/Z as Length in metres (Java `Length(meters, METRE)`).
        position_x: Option<f64>,
        position_y: Option<f64>,
        position_z: Option<f64>,
        /// MainEmission/ExcitationWavelength (nm).
        em_wavelength: Option<f64>,
        ex_wavelength: Option<f64>,
        /// ObjectiveMagnification / ObjectiveNA.
        magnification: Option<f64>,
        lens_na: Option<f64>,
        /// ExposureTime / MeasurementTimeOffset in seconds.
        exposure_time: Option<f64>,
        delta_t: Option<f64>,
        /// AbsTime timestamp text (Java `Timestamp`).
        absolute_time: Option<String>,
        /// AcquisitionType / ChannelType strings.
        acq_type: Option<String>,
        channel_type: Option<String>,
        /// OrientationMatrix parsed from `[a b c][d e f][g h i]`.
        orientation_matrix: Option<Vec<Vec<f64>>>,
    }

    #[derive(Clone, Default)]
    struct Channel {
        channel_id: i32,
        x: u32,
        y: u32,
        // V6 layout stores common metadata once per channel; copied into each
        // plane via `Channel::copy` (OperettaReader.Channel).
        channel_name: Option<String>,
        acq_type: Option<String>,
        channel_type: Option<String>,
        resolution_x: Option<f64>,
        resolution_y: Option<f64>,
        em_wavelength: Option<f64>,
        ex_wavelength: Option<f64>,
        magnification: Option<f64>,
        lens_na: Option<f64>,
        exposure_time: Option<f64>,
        orientation_matrix: Option<Vec<Vec<f64>>>,
    }

    impl Channel {
        /// Copy data from this Channel to the given Plane
        /// (OperettaReader.Channel.copy). Skipped if the channel looks empty.
        fn copy(&self, p: &mut Plane) {
            // don't copy if it looks like this is an empty channel
            if self.channel_id < 0 || self.x == 0 || self.y == 0 {
                return;
            }
            p.channel_name = self.channel_name.clone();
            p.acq_type = self.acq_type.clone();
            p.channel_type = self.channel_type.clone();
            p.resolution_x = self.resolution_x;
            p.resolution_y = self.resolution_y;
            p.x = self.x;
            p.y = self.y;
            p.em_wavelength = self.em_wavelength;
            p.ex_wavelength = self.ex_wavelength;
            p.magnification = self.magnification;
            p.lens_na = self.lens_na;
            p.exposure_time = self.exposure_time;
            p.orientation_matrix = self.orientation_matrix.clone();
        }
    }

    /// Plate-level scalars captured by `OperettaHandler.endElement`
    /// (`Name`, `PlateTypeName`, `PlateID`, `MeasurementID`). Mirrors the
    /// handler's `plateName`/`plateDescription`/`plateID`/`measurementID`.
    #[derive(Clone, Default)]
    struct PlateInfo {
        plate_name: Option<String>,
        plate_description: Option<String>,
        plate_identifier: Option<String>,
        measurement_id: Option<String>,
    }

    pub fn parse(path: &Path) -> Result<HcsAssembly> {
        let xml = std::fs::read_to_string(path).map_err(BioFormatsError::Io)?;
        let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
        // "Images" directory may need to be located; Operetta URLs are relative
        // to the directory containing Index.idx.xml.
        let images_dir = locate_images_dir(&dir);

        let mut planes: Vec<Plane> = Vec::new();
        let mut channels: Map<i32, Channel> = Map::new();
        let mut plate_rows = 0i32;
        let mut plate_cols = 0i32;
        let mut plate_info = PlateInfo::default();

        // Parser state. A single stateful SAX pass populates `planes`/`channels`.
        let mut active_plane: Option<Plane> = None;
        let mut active_channel: Option<Channel> = None;
        let mut active_channel_id: i32 = 0;
        // `isHarmony` (from EvaluationInputData/@xmlns) and `InstrumentType`
        // govern the PositionZ source element and whether applyMatrix runs.
        let mut is_harmony = false;
        let mut instrument_type: Option<String> = None;

        let mut current_name = String::new();
        let mut reader = quick_xml::Reader::from_str(&xml);
        reader.config_mut().trim_text(false);
        let mut text_buf = String::new();
        loop {
            match reader.read_event() {
                Ok(quick_xml::events::Event::Start(ref e)) => {
                    text_buf.clear();
                    current_name = super::xmlutil::local_name(e);
                    handle_start(
                        &current_name,
                        e,
                        &mut active_plane,
                        &mut active_channel,
                        &mut active_channel_id,
                        &mut channels,
                        &mut is_harmony,
                    );
                }
                Ok(quick_xml::events::Event::Empty(ref e)) => {
                    let name = super::xmlutil::local_name(e);
                    handle_start(
                        &name,
                        e,
                        &mut active_plane,
                        &mut active_channel,
                        &mut active_channel_id,
                        &mut channels,
                        &mut is_harmony,
                    );
                    handle_end(
                        &name,
                        "",
                        &mut active_plane,
                        &mut active_channel,
                        &mut channels,
                        &mut planes,
                        &mut plate_rows,
                        &mut plate_cols,
                        &mut plate_info,
                        &dir,
                        &images_dir,
                        is_harmony,
                        &mut instrument_type,
                    );
                    current_name.clear();
                }
                Ok(quick_xml::events::Event::Text(ref t)) => {
                    if let Ok(s) = t.unescape() {
                        text_buf.push_str(&s);
                    }
                }
                Ok(quick_xml::events::Event::CData(ref t)) => {
                    text_buf.push_str(&String::from_utf8_lossy(t.as_ref()));
                }
                Ok(quick_xml::events::Event::End(ref e)) => {
                    let name = super::xmlutil::local_name(e);
                    handle_end(
                        &current_name,
                        &text_buf,
                        &mut active_plane,
                        &mut active_channel,
                        &mut channels,
                        &mut planes,
                        &mut plate_rows,
                        &mut plate_cols,
                        &mut plate_info,
                        &dir,
                        &images_dir,
                        is_harmony,
                        &mut instrument_type,
                    );
                    // handle_end with element close ('Image'/'Entry') uses qName
                    handle_close(
                        &name,
                        &mut active_plane,
                        &mut active_channel,
                        &channels,
                        &mut planes,
                        &instrument_type,
                    );
                    current_name.clear();
                    text_buf.clear();
                }
                Ok(quick_xml::events::Event::Eof) => break,
                Err(_) => break,
                _ => {}
            }
        }

        if planes.is_empty() {
            return Err(BioFormatsError::Format(
                "PerkinElmer Operetta: no image planes found in index".to_string(),
            ));
        }

        // Collect unique coordinate sets (mirrors initFile).
        let mut rows = unique_sorted(planes.iter().map(|p| p.row));
        let cols = unique_sorted(planes.iter().map(|p| p.col));
        let fields = unique_sorted(planes.iter().map(|p| p.field));
        let zs = unique_sorted(planes.iter().map(|p| p.z));
        let cs = unique_sorted(planes.iter().map(|p| p.c));
        let ts = unique_sorted(planes.iter().map(|p| p.t));
        rows.dedup();

        let mut unique_wells: Vec<String> = Vec::new();
        for p in &planes {
            let w = super::well_name(p.row, p.col);
            if !unique_wells.contains(&w) {
                unique_wells.push(w);
            }
        }

        let size_z = zs.len().max(1) as u32;
        let size_c = cs.len().max(1) as u32;
        let size_t = ts.len().max(1) as u32;
        let n_planes = (size_z * size_c * size_t) as usize;
        let series_count = unique_wells.len() * fields.len().max(1);

        // hashToPlane keyed by row:col:field:c:z:t
        let mut hash: Map<String, Plane> = Map::new();
        for p in &planes {
            let key = format!("{}:{}:{}:{}:{}:{}", p.row, p.col, p.field, p.c, p.z, p.t);
            hash.insert(key, p.clone());
        }

        // Build planes[series][plane] in dimension order XYCZT
        // (Java nested loop: for t { for z { for c { nextPlane++ } } } => C fastest).
        let mut series_planes: Vec<Vec<Option<Plane>>> = vec![vec![None; n_planes]; series_count];
        let mut next_series = 0usize;
        for &r in &rows {
            for &cc in &cols {
                let well = super::well_name(r, cc);
                if !unique_wells.contains(&well) {
                    continue;
                }
                for &f in &fields {
                    let mut next_plane = 0usize;
                    for &t in &ts {
                        for &z in &zs {
                            for &ch in &cs {
                                let key = format!("{}:{}:{}:{}:{}:{}", r, cc, f, ch, z, t);
                                if let Some(p) = hash.get(&key) {
                                    if next_series < series_count && next_plane < n_planes {
                                        series_planes[next_series][next_plane] = Some(p.clone());
                                    }
                                }
                                next_plane += 1;
                            }
                        }
                    }
                    next_series += 1;
                }
            }
        }

        // Determine pixel type / size from the first valid TIFF found.
        let mut size_x = planes[0].x.max(1);
        let mut size_y = planes[0].y.max(1);
        let mut pixel_type = PixelType::Uint16;
        let mut bits = 16u8;
        let mut little_endian = true;
        'find: for sp in &series_planes {
            for p in sp.iter().flatten() {
                if let Some(f) = &p.filename {
                    if let Some((sx, sy, pt, b, le)) = super::probe_tiff(f) {
                        // Ignore uint32 (PerkinElmer flags these as invalid).
                        if pt != PixelType::Uint32 {
                            size_x = sx.max(p.x);
                            size_y = sy.max(p.y);
                            pixel_type = pt;
                            bits = b;
                            little_endian = le;
                            break 'find;
                        }
                    }
                }
            }
        }

        // Assemble HcsAssembly.
        let mut series = Vec::with_capacity(series_count);
        let mut asm_planes: Vec<Vec<PlaneRef>> = Vec::with_capacity(series_count);
        for sp in &series_planes {
            // per-series XY: use first non-null plane's stored dims if present
            let (sx, sy) = sp
                .iter()
                .flatten()
                .find(|p| p.x > 0 && p.y > 0)
                .map(|p| (p.x.max(size_x), p.y.max(size_y)))
                .unwrap_or((size_x, size_y));
            let mut meta = super::make_series_meta(
                sx.max(1),
                sy.max(1),
                size_z,
                size_c,
                size_t,
                pixel_type,
                bits,
                little_endian,
                DimensionOrder::XYCZT,
                "PerkinElmer Operetta",
            );
            // OperettaReader.initStandardMetadata addGlobalMeta(...) plate scalars.
            if let Some(v) = &plate_info.plate_name {
                meta.series_metadata
                    .insert("Plate name".to_string(), MetadataValue::String(v.clone()));
            }
            if let Some(v) = &plate_info.plate_description {
                meta.series_metadata.insert(
                    "Plate description".to_string(),
                    MetadataValue::String(v.clone()),
                );
            }
            if let Some(v) = &plate_info.plate_identifier {
                meta.series_metadata
                    .insert("Plate ID".to_string(), MetadataValue::String(v.clone()));
            }
            if let Some(v) = &plate_info.measurement_id {
                meta.series_metadata.insert(
                    "Measurement ID".to_string(),
                    MetadataValue::String(v.clone()),
                );
            }
            // Per-channel + per-plane scalars (OperettaReader.populateMetadataStore).
            project_series_metadata(&mut meta, sp, size_c, size_z);
            series.push(meta);
            asm_planes.push(
                sp.iter()
                    .map(|p| match p {
                        Some(p) => match p.filename.clone() {
                            Some(f) => PlaneRef::whole(f, 0),
                            None => PlaneRef::default(),
                        },
                        None => PlaneRef::default(),
                    })
                    .collect(),
            );
        }

        let mut asm = HcsAssembly::new();
        asm.series = series;
        asm.planes = asm_planes;
        Ok(asm)
    }

    fn locate_images_dir(dir: &Path) -> PathBuf {
        // The XML's parent is usually the Images directory itself.
        if dir
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.eq_ignore_ascii_case("images"))
            .unwrap_or(false)
        {
            return dir.to_path_buf();
        }
        // Otherwise look for an "Images" subdirectory.
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir()
                    && p.file_name()
                        .and_then(|n| n.to_str())
                        .map(|n| n.eq_ignore_ascii_case("images"))
                        .unwrap_or(false)
                {
                    return p;
                }
            }
        }
        dir.to_path_buf()
    }

    fn resolve_url(value: &str, dir: &Path, images_dir: &Path) -> Option<PathBuf> {
        if value.is_empty() {
            return None;
        }
        if value.starts_with("http") {
            return Some(PathBuf::from(value));
        }
        let direct = dir.join(value);
        if direct.exists() {
            return Some(direct);
        }
        let via_images = images_dir.join(value);
        if via_images.exists() {
            return Some(via_images);
        }
        // Default to the images-dir candidate even if it doesn't exist yet, so
        // assembly can proceed and open_bytes can blank-fill missing planes.
        Some(via_images)
    }

    fn handle_start(
        name: &str,
        e: &quick_xml::events::BytesStart,
        active_plane: &mut Option<Plane>,
        active_channel: &mut Option<Channel>,
        active_channel_id: &mut i32,
        channels: &mut Map<i32, Channel>,
        is_harmony: &mut bool,
    ) {
        match name {
            "Image" => {
                if super::xmlutil::attr(e, "id").is_none() {
                    *active_plane = Some(Plane::default());
                }
            }
            "Entry" => {
                if let Some(cid) = super::xmlutil::attr(e, "ChannelID") {
                    if let Ok(cid) = cid.trim().parse::<i32>() {
                        *active_channel_id = cid;
                        let ch = Channel {
                            channel_id: cid,
                            ..Default::default()
                        };
                        channels.insert(cid, ch.clone());
                        *active_channel = Some(ch);
                    }
                }
            }
            "EvaluationInputData" => {
                // isHarmony = xmlns.indexOf(HARMONY_MAGIC) > 0
                if let Some(xmlns) = super::xmlutil::attr(e, "xmlns") {
                    *is_harmony = xmlns.find("Harmony").map(|i| i > 0).unwrap_or(false);
                }
            }
            _ => {}
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_end(
        current_name: &str,
        value: &str,
        active_plane: &mut Option<Plane>,
        active_channel: &mut Option<Channel>,
        channels: &mut Map<i32, Channel>,
        _planes: &mut Vec<Plane>,
        plate_rows: &mut i32,
        plate_cols: &mut i32,
        plate_info: &mut PlateInfo,
        dir: &Path,
        images_dir: &Path,
        is_harmony: bool,
        instrument_type: &mut Option<String>,
    ) {
        let v = value.trim();
        match current_name {
            "InstrumentType" => {
                *instrument_type = Some(v.to_string());
            }
            "PlateRows" => {
                if let Ok(n) = v.parse::<i32>() {
                    *plate_rows = n;
                }
            }
            "PlateColumns" => {
                if let Ok(n) = v.parse::<i32>() {
                    *plate_cols = n;
                }
            }
            // OperettaHandler.endElement plate-level scalars. `Name` is the
            // last-seen value (handler keeps no nesting guard); `PlateTypeName`
            // is the plate description.
            "Name" => {
                plate_info.plate_name = Some(v.to_string());
            }
            "PlateTypeName" => {
                plate_info.plate_description = Some(v.to_string());
            }
            "PlateID" => {
                plate_info.plate_identifier = Some(v.to_string());
            }
            "MeasurementID" => {
                plate_info.measurement_id = Some(v.to_string());
            }
            _ => {}
        }

        // Channel/plane dimension fields.
        if active_plane.is_some() || active_channel.is_some() {
            match current_name {
                "ImageSizeX" => {
                    if let Ok(x) = v.parse::<u32>() {
                        if let Some(p) = active_plane.as_mut() {
                            p.x = x;
                        } else if let Some(c) = active_channel.as_mut() {
                            c.x = x;
                            if let Some(stored) = channels.get_mut(&c.channel_id) {
                                stored.x = x;
                            }
                        }
                    }
                }
                "ImageSizeY" => {
                    if let Ok(y) = v.parse::<u32>() {
                        if let Some(p) = active_plane.as_mut() {
                            p.y = y;
                        } else if let Some(c) = active_channel.as_mut() {
                            c.y = y;
                            if let Some(stored) = channels.get_mut(&c.channel_id) {
                                stored.y = y;
                            }
                        }
                    }
                }
                "ChannelName" => {
                    if let Some(p) = active_plane.as_mut() {
                        p.channel_name = Some(v.to_string());
                    } else if let Some(c) = active_channel.as_mut() {
                        c.channel_name = Some(v.to_string());
                        if let Some(stored) = channels.get_mut(&c.channel_id) {
                            stored.channel_name = Some(v.to_string());
                        }
                    }
                }
                "ImageResolutionX" => {
                    // resolution stored in meters -> micrometers (Java * 1e6).
                    if let Ok(r) = v.parse::<f64>() {
                        let res = r * 1_000_000.0;
                        if let Some(p) = active_plane.as_mut() {
                            p.resolution_x = Some(res);
                        } else if let Some(c) = active_channel.as_mut() {
                            c.resolution_x = Some(res);
                            if let Some(stored) = channels.get_mut(&c.channel_id) {
                                stored.resolution_x = Some(res);
                            }
                        }
                    }
                }
                "ImageResolutionY" => {
                    if let Ok(r) = v.parse::<f64>() {
                        let res = r * 1_000_000.0;
                        if let Some(p) = active_plane.as_mut() {
                            p.resolution_y = Some(res);
                        } else if let Some(c) = active_channel.as_mut() {
                            c.resolution_y = Some(res);
                            if let Some(stored) = channels.get_mut(&c.channel_id) {
                                stored.resolution_y = Some(res);
                            }
                        }
                    }
                }
                "ObjectiveMagnification" => {
                    if let Ok(mag) = v.parse::<f64>() {
                        if let Some(p) = active_plane.as_mut() {
                            p.magnification = Some(mag);
                        } else if let Some(c) = active_channel.as_mut() {
                            c.magnification = Some(mag);
                            if let Some(stored) = channels.get_mut(&c.channel_id) {
                                stored.magnification = Some(mag);
                            }
                        }
                    }
                }
                "ObjectiveNA" => {
                    if let Ok(na) = v.parse::<f64>() {
                        if let Some(p) = active_plane.as_mut() {
                            p.lens_na = Some(na);
                        } else if let Some(c) = active_channel.as_mut() {
                            c.lens_na = Some(na);
                            if let Some(stored) = channels.get_mut(&c.channel_id) {
                                stored.lens_na = Some(na);
                            }
                        }
                    }
                }
                "MainEmissionWavelength" => {
                    if let Ok(w) = v.parse::<f64>() {
                        if let Some(p) = active_plane.as_mut() {
                            p.em_wavelength = Some(w);
                        } else if let Some(c) = active_channel.as_mut() {
                            c.em_wavelength = Some(w);
                            if let Some(stored) = channels.get_mut(&c.channel_id) {
                                stored.em_wavelength = Some(w);
                            }
                        }
                    }
                }
                "MainExcitationWavelength" => {
                    if let Ok(w) = v.parse::<f64>() {
                        if let Some(p) = active_plane.as_mut() {
                            p.ex_wavelength = Some(w);
                        } else if let Some(c) = active_channel.as_mut() {
                            c.ex_wavelength = Some(w);
                            if let Some(stored) = channels.get_mut(&c.channel_id) {
                                stored.ex_wavelength = Some(w);
                            }
                        }
                    }
                }
                "ExposureTime" => {
                    // Time in seconds (Java `Time(value, SECOND)`).
                    if let Ok(t) = v.parse::<f64>() {
                        if let Some(p) = active_plane.as_mut() {
                            p.exposure_time = Some(t);
                        } else if let Some(c) = active_channel.as_mut() {
                            c.exposure_time = Some(t);
                            if let Some(stored) = channels.get_mut(&c.channel_id) {
                                stored.exposure_time = Some(t);
                            }
                        }
                    }
                }
                "AcquisitionType" => {
                    if let Some(p) = active_plane.as_mut() {
                        p.acq_type = Some(v.to_string());
                    } else if let Some(c) = active_channel.as_mut() {
                        c.acq_type = Some(v.to_string());
                        if let Some(stored) = channels.get_mut(&c.channel_id) {
                            stored.acq_type = Some(v.to_string());
                        }
                    }
                }
                "ChannelType" => {
                    if let Some(p) = active_plane.as_mut() {
                        p.channel_type = Some(v.to_string());
                    } else if let Some(c) = active_channel.as_mut() {
                        c.channel_type = Some(v.to_string());
                        if let Some(stored) = channels.get_mut(&c.channel_id) {
                            stored.channel_type = Some(v.to_string());
                        }
                    }
                }
                "OrientationMatrix" => {
                    if let Some(matrix) = parse_orientation_matrix(v) {
                        if let Some(p) = active_plane.as_mut() {
                            p.orientation_matrix = Some(matrix.clone());
                        } else if let Some(c) = active_channel.as_mut() {
                            c.orientation_matrix = Some(matrix.clone());
                            if let Some(stored) = channels.get_mut(&c.channel_id) {
                                stored.orientation_matrix = Some(matrix);
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        // Plane-only fields.
        if let Some(p) = active_plane.as_mut() {
            match current_name {
                "URL" => {
                    if let Some(f) = resolve_url(v, dir, images_dir) {
                        p.filename = Some(f);
                    }
                }
                "Row" => {
                    if let Ok(n) = v.parse::<i32>() {
                        p.row = n - 1;
                    }
                }
                "Col" => {
                    if let Ok(n) = v.parse::<i32>() {
                        p.col = n - 1;
                    }
                }
                "FieldID" => {
                    if let Ok(n) = v.parse::<i32>() {
                        p.field = n;
                    }
                }
                "PlaneID" => {
                    if let Ok(n) = v.parse::<i32>() {
                        p.z = n;
                    }
                }
                "TimepointID" => {
                    if let Ok(n) = v.parse::<i32>() {
                        p.t = n;
                    }
                }
                "ChannelID" => {
                    if let Ok(n) = v.parse::<i32>() {
                        p.c = n;
                    }
                }
                "PositionX" => {
                    // position stored in meters (Java `Length(meters, METRE)`).
                    if let Ok(m) = v.parse::<f64>() {
                        p.position_x = Some(m);
                    }
                }
                "PositionY" => {
                    if let Ok(m) = v.parse::<f64>() {
                        p.position_y = Some(m);
                    }
                }
                // AbsPositionZ (non-Harmony) or PositionZ (Harmony).
                "AbsPositionZ" if !is_harmony => {
                    if let Ok(m) = v.parse::<f64>() {
                        p.position_z = Some(m);
                    }
                }
                "PositionZ" if is_harmony => {
                    if let Ok(m) = v.parse::<f64>() {
                        p.position_z = Some(m);
                    }
                }
                "MeasurementTimeOffset" => {
                    if let Ok(t) = v.parse::<f64>() {
                        p.delta_t = Some(t);
                    }
                }
                "AbsTime" => {
                    if !v.is_empty() {
                        p.absolute_time = Some(v.to_string());
                    }
                }
                _ => {}
            }
        }
    }

    /// Parse an `OrientationMatrix` value of the form `[a b c][d e f][g h i]`.
    /// Mirrors OperettaReader.endElement: split on ']', strip '[' and ','.
    /// Returns `None` unless the matrix has at least 3 rows with the expected
    /// minimum widths (Java's `matrix.length > 2 && ...` guard).
    fn parse_orientation_matrix(value: &str) -> Option<Vec<Vec<f64>>> {
        let rows: Vec<&str> = value.split(']').collect();
        let mut matrix: Vec<Vec<f64>> = Vec::with_capacity(rows.len());
        for row in &rows {
            let cleaned = row.replace('[', "").replace(',', " ");
            let cleaned = cleaned.trim();
            let vals: Vec<f64> = cleaned
                .split(' ')
                .map(|s| s.trim().parse::<f64>().unwrap_or(f64::NAN))
                .collect();
            matrix.push(vals);
        }
        if matrix.len() > 2
            && !matrix[0].is_empty()
            && matrix[1].len() > 1
            && matrix[2].len() > 2
        {
            Some(matrix)
        } else {
            None
        }
    }

    fn handle_close(
        qname: &str,
        active_plane: &mut Option<Plane>,
        active_channel: &mut Option<Channel>,
        channels: &Map<i32, Channel>,
        planes: &mut Vec<Plane>,
        instrument_type: &Option<String>,
    ) {
        match qname {
            "Image" => {
                if let Some(mut p) = active_plane.take() {
                    // V6 layout: copy common per-channel metadata into the plane
                    // (OperettaReader: `channels.get(c).copy(activePlane)`).
                    if let Some(c) = channels.get(&p.c) {
                        c.copy(&mut p);
                    }
                    // applyMatrix unless the instrument is a Phenix.
                    if instrument_type.as_deref() != Some("Phenix") {
                        apply_matrix(&mut p);
                    }
                    planes.push(p);
                }
            }
            "Entry" => {
                *active_channel = None;
            }
            _ => {}
        }
    }

    /// Apply `orientation_matrix` to (position_x, position_y, position_z),
    /// mirroring OperettaReader.Plane.applyMatrix. Positions stay in metres.
    fn apply_matrix(p: &mut Plane) {
        let (Some(px), Some(py), Some(pz), Some(matrix)) = (
            p.position_x,
            p.position_y,
            p.position_z,
            p.orientation_matrix.as_ref(),
        ) else {
            return;
        };
        let v = [px, py, pz];
        let mut new_values = [0.0f64; 3];
        for (row, mrow) in matrix.iter().enumerate().take(3) {
            for (col, &m) in mrow.iter().enumerate() {
                if col < v.len() {
                    new_values[row] += m * v[col];
                } else {
                    new_values[row] += m;
                }
            }
        }
        p.position_x = Some(new_values[0]);
        p.position_y = Some(new_values[1]);
        p.position_z = Some(new_values[2]);
    }

    fn unique_sorted<I: Iterator<Item = i32>>(it: I) -> Vec<i32> {
        let mut v: Vec<i32> = Vec::new();
        for x in it {
            if !v.contains(&x) {
                v.push(x);
            }
        }
        v.sort_unstable();
        v
    }

    /// Translate emission wavelength (nm) to an OME `Color` name, mirroring
    /// OperettaReader.getColor (PerkinElmer photocell colour bands).
    fn color_for_wavelength(em: Option<f64>) -> Option<&'static str> {
        let em = em?;
        Some(if em < 450.0 {
            "magenta" // violet
        } else if em < 500.0 {
            "blue"
        } else if em < 570.0 {
            "green"
        } else if em < 590.0 {
            "yellow"
        } else if em < 610.0 {
            "orange"
        } else {
            "red"
        })
    }

    /// Project per-channel + per-plane scalar metadata into one series'
    /// metadata map, mirroring OperettaReader.populateMetadataStore
    /// (channel name/acquisition mode/contrast/colour/emission/excitation,
    /// plane positions/exposure/deltaT, physical sizes + Z step).
    fn project_series_metadata(
        meta: &mut ImageMetadata,
        sp: &[Option<Plane>],
        size_c: u32,
        size_z: u32,
    ) {
        let m = &mut meta.series_metadata;
        let n = sp.len();

        // Objective magnification / NA from the first plane (planes[0][0]).
        if let Some(Some(p0)) = sp.first() {
            if let Some(mag) = p0.magnification {
                m.insert(
                    "operetta.ObjectiveMagnification".to_string(),
                    MetadataValue::Float(mag),
                );
            }
            if let Some(na) = p0.lens_na {
                m.insert("operetta.ObjectiveNA".to_string(), MetadataValue::Float(na));
            }
        }

        // Per-channel metadata. Java picks planes[i][c]; if null, advances by
        // size_c to find the next plane acquired for that channel.
        let mut first: Option<&Plane> = sp.first().and_then(|p| p.as_ref());
        for c in 0..size_c as usize {
            let mut plane: Option<&Plane> = sp.get(c).and_then(|p| p.as_ref());
            if plane.is_none() {
                let mut start = c;
                while plane.is_none() && start < n {
                    plane = sp.get(start).and_then(|p| p.as_ref());
                    start += size_c as usize;
                }
            }
            if let Some(p) = plane {
                if first.is_none() {
                    first = Some(p);
                }
                if let Some(name) = &p.channel_name {
                    m.insert(
                        format!("operetta.Channel{c}.Name"),
                        MetadataValue::String(name.clone()),
                    );
                }
                if let Some(acq) = &p.acq_type {
                    m.insert(
                        format!("operetta.Channel{c}.AcquisitionMode"),
                        MetadataValue::String(acq.clone()),
                    );
                }
                if let Some(ct) = &p.channel_type {
                    m.insert(
                        format!("operetta.Channel{c}.ContrastMethod"),
                        MetadataValue::String(ct.clone()),
                    );
                }
                if let Some(color) = color_for_wavelength(p.em_wavelength) {
                    m.insert(
                        format!("operetta.Channel{c}.Color"),
                        MetadataValue::String(color.to_string()),
                    );
                }
                if let Some(em) = p.em_wavelength {
                    if em > 0.0 {
                        m.insert(
                            format!("operetta.Channel{c}.EmissionWavelength"),
                            MetadataValue::Float(em),
                        );
                    }
                }
                if let Some(ex) = p.ex_wavelength {
                    if ex > 0.0 {
                        m.insert(
                            format!("operetta.Channel{c}.ExcitationWavelength"),
                            MetadataValue::Float(ex),
                        );
                    }
                }
            }
        }

        // Per-plane positions / exposure / deltaT, tracking the last plane at
        // the maximum Z (Java tracks `last` for the Z-step computation below).
        let mut last: Option<&Plane> = None;
        for (p_idx, slot) in sp.iter().enumerate() {
            if let Some(p) = slot {
                if let Some(x) = p.position_x {
                    m.insert(
                        format!("operetta.Plane{p_idx}.PositionX"),
                        MetadataValue::Float(x),
                    );
                }
                if let Some(y) = p.position_y {
                    m.insert(
                        format!("operetta.Plane{p_idx}.PositionY"),
                        MetadataValue::Float(y),
                    );
                }
                if let Some(z) = p.position_z {
                    m.insert(
                        format!("operetta.Plane{p_idx}.PositionZ"),
                        MetadataValue::Float(z),
                    );
                }
                if let Some(exp) = p.exposure_time {
                    m.insert(
                        format!("operetta.Plane{p_idx}.ExposureTime"),
                        MetadataValue::Float(exp),
                    );
                }
                if let Some(dt) = p.delta_t {
                    m.insert(
                        format!("operetta.Plane{p_idx}.DeltaT"),
                        MetadataValue::Float(dt),
                    );
                }
                // getZCTCoords(p)[0] == sizeZ - 1 : C is fastest, then Z.
                let z_coord = (p_idx as u32 / size_c) % size_z;
                if z_coord == size_z.saturating_sub(1) {
                    last = Some(p);
                }
            }
        }

        // Physical pixel sizes from `first` (PhysicalSizeX/Y in micrometers),
        // plus the average Z step (positions are in metres -> micrometers).
        if let Some(first) = first {
            if let Some(x) = first.resolution_x {
                m.insert("operetta.PhysicalSizeX".to_string(), MetadataValue::Float(x));
            }
            if let Some(y) = first.resolution_y {
                m.insert("operetta.PhysicalSizeY".to_string(), MetadataValue::Float(y));
            }
            if size_z > 1 {
                if let (Some(last), Some(first_z)) = (last, first.position_z) {
                    if let Some(last_z) = last.position_z {
                        // metres -> micrometers, then average over the Z range.
                        let first_um = first_z * 1_000_000.0;
                        let last_um = last_z * 1_000_000.0;
                        let avg = (last_um - first_um) / (size_z as f64 - 1.0);
                        m.insert(
                            "operetta.PhysicalSizeZ".to_string(),
                            MetadataValue::Float(avg),
                        );
                    }
                }
            }
        }
    }
}

// ===========================================================================
// Columbus parser (MeasurementIndex.ColumbusIDX.xml)  -- port of ColumbusReader
// ===========================================================================

mod columbus {
    use super::*;
    use std::collections::HashMap as Map;

    #[derive(Clone, Default)]
    struct Plane {
        file: Option<PathBuf>,
        file_index: u32,
        row: i32,
        col: i32,
        field: i32,
        timepoint: i32,
        channel: i32,
        z: i32,
        // Scalar metadata mirrored from ColumbusReader.Plane (parseImageXML).
        channel_name: Option<String>,
        /// MeasurementTimeOffset, in seconds (Java `Plane.deltaT`).
        delta_t: Option<f64>,
        /// AbsTime timestamp text (Java parses this to epoch seconds for deltaT).
        abs_time: Option<String>,
        /// MainEmissionWavelength (nm).
        em_wavelength: Option<f64>,
        /// MainExcitationWavelength (nm).
        ex_wavelength: Option<f64>,
        /// ImageResolutionX/Y in micrometers (Java `Plane.sizeX`/`sizeY`).
        physical_size_x: Option<f64>,
        physical_size_y: Option<f64>,
        /// PositionX/Y/Z in micrometers (Java `Plane.positionX/Y/Z`).
        position_x: Option<f64>,
        position_y: Option<f64>,
        position_z: Option<f64>,
        /// Per-channel colour (Java `Plane.channelColor`). ColumbusReader parses
        /// `ChannelColor` but leaves the field unset, deferring to the emission
        /// wavelength for colour, so this stays `None` to match.
        channel_color: Option<u32>,
        /// Owning well-sample index, assigned during metadata projection
        /// (Java `Plane.series`).
        series: i32,
    }

    pub fn parse(path: &Path) -> Result<HcsAssembly> {
        // Resolve to the actual ColumbusIDX index file if a sibling was given.
        let xml_path = find_index(path).unwrap_or_else(|| path.to_path_buf());
        let parent = xml_path.parent().unwrap_or(Path::new(".")).to_path_buf();

        let main_xml = std::fs::read_to_string(&xml_path).map_err(BioFormatsError::Io)?;
        let measurement = parse_measurement_index(&main_xml);
        let plate_rows = measurement.plate_rows;
        let plate_cols = measurement.plate_cols;
        let image_refs = &measurement.refs;

        // The per-image XML lists may live in timepoint subdirectories, or be
        // referenced directly. Discover all *.columbusidx.xml under the parent.
        let mut image_xmls: Vec<(PathBuf, i32)> = Vec::new();
        let mut timepoint_dirs: Vec<PathBuf> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&parent) {
            let mut dirs: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect();
            dirs.sort();
            for d in &dirs {
                timepoint_dirs.push(d.clone());
            }
            for (ti, d) in dirs.iter().enumerate() {
                if let Ok(sub) = std::fs::read_dir(d) {
                    for f in sub.flatten() {
                        let p = f.path();
                        if is_columbus_idx(&p) {
                            image_xmls.push((p, ti as i32));
                        }
                    }
                }
            }
        }
        // Also accept references named in the measurement index itself.
        for r in image_refs {
            let cand = parent.join(r);
            if is_columbus_idx(&cand) && !image_xmls.iter().any(|(p, _)| p == &cand) {
                image_xmls.push((cand, 0));
            }
        }
        // Fallback: ColumbusIDX files directly in parent.
        if image_xmls.is_empty() {
            if let Ok(entries) = std::fs::read_dir(&parent) {
                for f in entries.flatten() {
                    let p = f.path();
                    if is_columbus_idx(&p) && p != xml_path {
                        image_xmls.push((p, 0));
                    }
                }
            }
        }

        let mut planes: Vec<Plane> = Vec::new();
        let mut acquisition_date: Option<String> = None;
        for (p, t) in &image_xmls {
            if let Some(date) = parse_image_xml(p, *t, &mut planes) {
                if acquisition_date.is_none() {
                    acquisition_date = Some(date);
                }
            }
        }

        if planes.is_empty() {
            return Err(BioFormatsError::Format(
                "PerkinElmer Columbus: no image planes found in index".to_string(),
            ));
        }

        // Sort planes by (row, col, field, t, c, z).
        planes.sort_by(|a, b| {
            a.row
                .cmp(&b.row)
                .then(a.col.cmp(&b.col))
                .then(a.field.cmp(&b.field))
                .then(a.timepoint.cmp(&b.timepoint))
                .then(a.channel.cmp(&b.channel))
                .then(a.z.cmp(&b.z))
        });

        // Java ColumbusReader uses the raw getPlateColumns() for the sample
        // index (ColumbusReader.java:316,375), with no minimum-of-1 clamp.
        let cols_for_sample = plate_cols;
        let mut unique_samples: Vec<i32> = Vec::new();
        let mut unique_rows: Vec<i32> = Vec::new();
        let mut unique_cols: Vec<i32> = Vec::new();
        let mut n_fields = 0i32;
        let mut size_c = 0i32;
        let mut size_t = 0i32;
        let mut size_z = 0i32;
        for p in &planes {
            let sample = p.row * cols_for_sample + p.col;
            if !unique_samples.contains(&sample) {
                unique_samples.push(sample);
            }
            if !unique_rows.contains(&p.row) {
                unique_rows.push(p.row);
            }
            if !unique_cols.contains(&p.col) {
                unique_cols.push(p.col);
            }
            n_fields = n_fields.max(p.field + 1);
            size_c = size_c.max(p.channel + 1);
            size_t = size_t.max(p.timepoint + 1);
            size_z = size_z.max(p.z + 1);
        }
        let size_c = size_c.max(1) as u32;
        let size_t = size_t.max(1) as u32;
        let size_z = size_z.max(1) as u32;
        let n_fields = n_fields.max(1);
        let order = DimensionOrder::XYCTZ;
        let n_planes = (size_z * size_c * size_t) as usize;

        // Probe the first plane's TIFF for pixel parameters.
        let mut size_x = 1u32;
        let mut size_y = 1u32;
        let mut pixel_type = PixelType::Uint16;
        let mut bits = 16u8;
        let mut little_endian = true;
        for p in &planes {
            if let Some(f) = &p.file {
                if let Some((sx, sy, pt, b, le)) = super::probe_tiff(f) {
                    size_x = sx;
                    size_y = sy;
                    pixel_type = pt;
                    bits = b;
                    little_endian = le;
                    break;
                }
            }
        }

        // Build wellSample index order: for each unique row, col (if sample present),
        // then field.
        let series_count = unique_samples.len() * n_fields as usize;
        let mut series = Vec::with_capacity(series_count);
        let mut asm_planes: Vec<Vec<PlaneRef>> = Vec::with_capacity(series_count);

        let mut well_sample = 0i32;
        for &row in &unique_rows {
            for &col in &unique_cols {
                if !unique_samples.contains(&(row * cols_for_sample + col)) {
                    continue;
                }
                for field in 0..n_fields {
                    let mut sp = vec![PlaneRef::default(); n_planes];
                    for t in 0..size_t {
                        for c in 0..size_c {
                            for z in 0..size_z {
                                if let Some(p) = planes.iter_mut().find(|p| {
                                    p.row == row
                                        && p.col == col
                                        && p.field == field
                                        && p.timepoint == t as i32
                                        && p.channel == c as i32
                                        && p.z == z as i32
                                }) {
                                    // Java assigns p.series = wellSample as each
                                    // plane is projected into the store.
                                    p.series = well_sample;
                                    let idx =
                                        super::get_index(order, size_z, size_c, size_t, z, c, t)
                                            as usize;
                                    if idx < n_planes {
                                        if let Some(f) = p.file.clone() {
                                            sp[idx] = PlaneRef::whole(f, p.file_index);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    let mut meta = super::make_series_meta(
                        size_x,
                        size_y,
                        size_z,
                        size_c,
                        size_t,
                        pixel_type,
                        bits,
                        little_endian,
                        order,
                        "PerkinElmer Columbus",
                    );
                    project_series_metadata(
                        &mut meta,
                        &planes,
                        row,
                        col,
                        field,
                        size_c,
                        acquisition_date.as_deref(),
                    );
                    // MeasurementHandler.endElement addGlobalMeta(...) keys
                    // (ScreenName, PlateName, PlateType, Measurement, ...).
                    for (k, v) in &measurement.global {
                        meta.series_metadata.insert(k.clone(), v.clone());
                    }
                    series.push(meta);
                    asm_planes.push(sp);
                    well_sample += 1;
                }
            }
        }
        let _ = plate_rows;

        let mut asm = HcsAssembly::new();
        asm.series = series;
        asm.planes = asm_planes;
        Ok(asm)
    }

    fn is_columbus_idx(p: &Path) -> bool {
        p.is_file()
            && p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.to_ascii_lowercase().ends_with("columbusidx.xml"))
                .unwrap_or(false)
    }

    fn find_index(name: &Path) -> Option<PathBuf> {
        const XML_FILE: &str = "MeasurementIndex.ColumbusIDX.xml";
        // If the given file is itself the index, use it.
        if name
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.eq_ignore_ascii_case(XML_FILE))
            .unwrap_or(false)
        {
            return Some(name.to_path_buf());
        }
        let parent = name.parent()?;
        let cand = parent.join(XML_FILE);
        if cand.exists() {
            return Some(cand);
        }
        if let Some(grand) = parent.parent() {
            let cand = grand.join(XML_FILE);
            if cand.exists() {
                return Some(cand);
            }
        }
        None
    }

    /// Parse the top-level measurement index for plate dims + referenced files.
    /// Captured scalars from the `MeasurementIndex.ColumbusIDX.xml` index,
    /// mirroring `ColumbusReader.MeasurementHandler`.
    #[derive(Default)]
    pub(super) struct MeasurementInfo {
        pub(super) plate_rows: i32,
        pub(super) plate_cols: i32,
        refs: Vec<String>,
        /// `addGlobalMeta(currentName, value)` for every element in the index.
        pub(super) global: Vec<(String, MetadataValue)>,
    }

    pub(super) fn parse_measurement_index(xml: &str) -> MeasurementInfo {
        let mut info = MeasurementInfo::default();
        let mut cur = String::new();
        let mut reader = quick_xml::Reader::from_str(xml);
        reader.config_mut().trim_text(false);
        let mut text = String::new();
        loop {
            match reader.read_event() {
                Ok(quick_xml::events::Event::Start(ref e)) => {
                    cur = super::xmlutil::local_name(e);
                    text.clear();
                }
                Ok(quick_xml::events::Event::Text(ref t)) => {
                    if let Ok(s) = t.unescape() {
                        text.push_str(&s);
                    }
                }
                Ok(quick_xml::events::Event::End(_)) => {
                    // MeasurementHandler.endElement: addGlobalMeta(currentName,
                    // value) for every element, where `value` is the
                    // accumulated character data (untrimmed in Java).
                    if !cur.is_empty() {
                        info.global.push((
                            cur.clone(),
                            MetadataValue::String(text.clone()),
                        ));
                    }
                    let v = text.trim();
                    match cur.as_str() {
                        "PlateRows" => {
                            if let Ok(n) = v.parse() {
                                info.plate_rows = n;
                            }
                        }
                        "PlateColumns" => {
                            if let Ok(n) = v.parse() {
                                info.plate_cols = n;
                            }
                        }
                        "Reference" => {
                            if !v.is_empty() {
                                info.refs.push(v.to_string());
                            }
                        }
                        _ => {}
                    }
                    cur.clear();
                    text.clear();
                }
                Ok(quick_xml::events::Event::Eof) => break,
                Err(_) => break,
                _ => {}
            }
        }
        info
    }

    /// Project the per-image scalar metadata captured by `parse_image_xml` into
    /// one series' metadata map, mirroring the OME store writes in
    /// `ColumbusReader.initFile` (acquisition date, well-sample/plane positions,
    /// physical sizes, and per-channel name + emission/excitation wavelengths).
    fn project_series_metadata(
        meta: &mut ImageMetadata,
        planes: &[Plane],
        row: i32,
        col: i32,
        field: i32,
        size_c: u32,
        acquisition_date: Option<&str>,
    ) {
        let find = |t: i32, c: i32, z: i32| -> Option<&Plane> {
            planes.iter().find(|p| {
                p.row == row
                    && p.col == col
                    && p.field == field
                    && p.timepoint == t
                    && p.channel == c
                    && p.z == z
            })
        };
        let m = &mut meta.series_metadata;

        if let Some(date) = acquisition_date {
            // store.setImageAcquisitionDate
            m.insert(
                "columbus.AcquisitionDate".to_string(),
                MetadataValue::String(date.to_string()),
            );
        }

        // Base plane (row, col, field, t=0, c=0, z=0): well-sample position +
        // physical pixel size (store.setWellSamplePositionX/Y, PhysicalSizeX/Y).
        if let Some(base) = find(0, 0, 0) {
            if let Some(x) = base.position_x {
                m.insert("columbus.WellSamplePositionX".to_string(), MetadataValue::Float(x));
            }
            if let Some(y) = base.position_y {
                m.insert("columbus.WellSamplePositionY".to_string(), MetadataValue::Float(y));
            }
            if let Some(x) = base.physical_size_x {
                m.insert("columbus.PhysicalSizeX".to_string(), MetadataValue::Float(x));
            }
            if let Some(y) = base.physical_size_y {
                m.insert("columbus.PhysicalSizeY".to_string(), MetadataValue::Float(y));
            }
            if let Some(z) = base.position_z {
                // store.setPlanePositionZ (first plane of the series)
                m.insert("columbus.PlanePositionZ".to_string(), MetadataValue::Float(z));
            }
            if let Some(dt) = base.delta_t {
                // store.setPlaneDeltaT (MeasurementTimeOffset)
                m.insert("columbus.PlaneDeltaT".to_string(), MetadataValue::Float(dt));
            }
            if let Some(abs) = &base.abs_time {
                m.insert(
                    "columbus.AbsTime".to_string(),
                    MetadataValue::String(abs.clone()),
                );
            }
        }

        // Per-channel scalars (store.setChannelName / Emission / Excitation).
        for c in 0..size_c as i32 {
            if let Some(p) = find(0, c, 0) {
                if let Some(name) = &p.channel_name {
                    m.insert(
                        format!("columbus.Channel{c}.Name"),
                        MetadataValue::String(name.clone()),
                    );
                }
                if let Some(em) = p.em_wavelength {
                    if em as i64 > 0 {
                        m.insert(
                            format!("columbus.Channel{c}.EmissionWavelength"),
                            MetadataValue::Float(em),
                        );
                    }
                }
                if let Some(ex) = p.ex_wavelength {
                    if ex as i64 > 0 {
                        m.insert(
                            format!("columbus.Channel{c}.ExcitationWavelength"),
                            MetadataValue::Float(ex),
                        );
                    }
                }
            }
        }
    }

    /// Parse a per-timepoint image-list XML, appending discovered planes.
    ///
    /// Returns the `MeasurementStartTime` text from the `Plates/Plate` graph
    /// (the acquisition date), mirroring `ColumbusReader.parseImageXML`. Java
    /// only records it for the first/base timepoint (`externalTime <= 0`).
    fn parse_image_xml(path: &Path, external_time: i32, out: &mut Vec<Plane>) -> Option<String> {
        let Ok(xml) = std::fs::read_to_string(path) else {
            return None;
        };
        let parent = path.parent().unwrap_or(Path::new(".")).to_path_buf();

        let mut reader = quick_xml::Reader::from_str(&xml);
        reader.config_mut().trim_text(false);

        let mut in_image = false;
        let mut depth_image = 0i32; // distinguish <Images> from <Image>
        let mut cur = String::new();
        let mut text = String::new();
        let mut cur_attrs: Map<String, String> = Map::new();
        let mut plane = Plane::default();
        let mut acquisition_date: Option<String> = None;

        loop {
            match reader.read_event() {
                Ok(quick_xml::events::Event::Start(ref e)) => {
                    let ln = super::xmlutil::local_name(e);
                    if ln == "Image" {
                        in_image = true;
                        depth_image += 1;
                        plane = Plane::default();
                    }
                    cur = ln;
                    text.clear();
                    cur_attrs.clear();
                    for a in e.attributes().flatten() {
                        let k = String::from_utf8_lossy(a.key.as_ref()).to_string();
                        let v = String::from_utf8_lossy(&a.value).to_string();
                        cur_attrs.insert(k, v);
                    }
                }
                Ok(quick_xml::events::Event::Text(ref t)) => {
                    if let Ok(s) = t.unescape() {
                        text.push_str(&s);
                    }
                }
                Ok(quick_xml::events::Event::End(ref e)) => {
                    let ln = super::xmlutil::local_name(e);
                    let v = text.trim().to_string();
                    // Plate-level MeasurementStartTime lives outside <Image>.
                    if ln == "MeasurementStartTime"
                        && !in_image
                        && external_time <= 0
                        && acquisition_date.is_none()
                        && !v.is_empty()
                    {
                        acquisition_date = Some(v.clone());
                    }
                    if in_image && ln != "Image" {
                        apply_image_field(&mut plane, &cur, &v, &cur_attrs, &parent, external_time);
                    }
                    if ln == "Image" {
                        in_image = false;
                        depth_image -= 1;
                        if depth_image >= 0 {
                            out.push(std::mem::take(&mut plane));
                        }
                    }
                    cur.clear();
                    text.clear();
                }
                Ok(quick_xml::events::Event::Eof) => break,
                Err(_) => break,
                _ => {}
            }
        }
        acquisition_date
    }

    /// Calculate a value in micrometers based on the raw value and the `Unit`
    /// attribute (port of `ColumbusReader.correctUnits`).
    fn correct_units(value: f64, unit: Option<&str>) -> f64 {
        match unit {
            Some("m") => value * 1_000_000.0,
            Some("cm") => value * 10_000.0,
            Some("nm") => value / 1000.0,
            _ => value,
        }
    }

    fn apply_image_field(
        p: &mut Plane,
        name: &str,
        value: &str,
        attrs: &Map<String, String>,
        parent: &Path,
        external_time: i32,
    ) {
        let unit = || attrs.get("Unit").map(|s| s.as_str());
        match name {
            "URL" => {
                p.file = Some(parent.join(value));
                if let Some(buf) = attrs.get("BufferNo") {
                    if let Ok(n) = buf.trim().parse() {
                        p.file_index = n;
                    }
                }
            }
            "Row" => {
                if let Ok(n) = value.parse::<i32>() {
                    p.row = n - 1;
                }
            }
            "Col" => {
                if let Ok(n) = value.parse::<i32>() {
                    p.col = n - 1;
                }
            }
            "FieldID" => {
                if let Ok(n) = value.parse::<i32>() {
                    p.field = n - 1;
                }
            }
            "PlaneID" => {
                if let Ok(n) = value.parse::<i32>() {
                    p.z = n - 1;
                }
            }
            "TimepointID" => {
                if let Ok(n) = value.parse::<i32>() {
                    p.timepoint = n - 1;
                    if p.timepoint == 0 {
                        p.timepoint = external_time;
                    }
                }
            }
            "ChannelID" => {
                if let Ok(n) = value.parse::<i32>() {
                    p.channel = n - 1;
                }
            }
            "ChannelName" => {
                if !value.is_empty() {
                    p.channel_name = Some(value.to_string());
                }
            }
            "ChannelColor" => {
                // Java decomposes BGRA but does NOT set p.channelColor (the
                // assignment is commented out), deferring colour to the
                // emission wavelength. We mirror that: parse, but leave unset.
                if let Ok(color) = value.parse::<i64>() {
                    let _blue = ((color >> 24) & 0xff) as u32;
                    let _green = ((color >> 16) & 0xff) as u32;
                    let _red = ((color >> 8) & 0xff) as u32;
                    let _alpha = (color & 0xff) as u32;
                    // p.channel_color intentionally left None (see field doc).
                    let _ = &mut p.channel_color;
                }
            }
            "MeasurementTimeOffset" => {
                if let Ok(v) = value.parse::<f64>() {
                    p.delta_t = Some(v);
                }
            }
            "AbsTime" => {
                // Java parses this ISO timestamp to epoch seconds and stores it
                // as deltaT; we retain the raw text for metadata projection.
                if !value.is_empty() {
                    p.abs_time = Some(value.to_string());
                }
            }
            "MainEmissionWavelength" => {
                if let Ok(v) = value.parse::<f64>() {
                    p.em_wavelength = Some(v);
                }
            }
            "MainExcitationWavelength" => {
                if let Ok(v) = value.parse::<f64>() {
                    p.ex_wavelength = Some(v);
                }
            }
            "ImageResolutionX" => {
                if let Ok(v) = value.parse::<f64>() {
                    p.physical_size_x = Some(correct_units(v, unit()));
                }
            }
            "ImageResolutionY" => {
                if let Ok(v) = value.parse::<f64>() {
                    p.physical_size_y = Some(correct_units(v, unit()));
                }
            }
            "PositionX" => {
                if let Ok(v) = value.parse::<f64>() {
                    p.position_x = Some(correct_units(v, unit()));
                }
            }
            "PositionY" => {
                if let Ok(v) = value.parse::<f64>() {
                    p.position_y = Some(correct_units(v, unit()));
                }
            }
            "PositionZ" => {
                if let Ok(v) = value.parse::<f64>() {
                    p.position_z = Some(correct_units(v, unit()));
                }
            }
            _ => {}
        }
    }
}

// ===========================================================================
// ScanR parser (experiment_descriptor.xml)  -- port of ScanrReader
// ===========================================================================

mod scanr {
    use super::*;
    use std::collections::HashMap as Map;

    fn block(index: i32, axis: &str) -> String {
        format!("{}{:05}", axis, index)
    }

    /// Port of Java `DataTools.parseDouble`: lenient numeric parse that returns
    /// `None` rather than throwing when the string is not a number.
    fn parse_double(v: &str) -> Option<f64> {
        v.trim().parse::<f64>().ok()
    }

    fn adjust_well_dims(well_count: usize) -> (i32, i32) {
        // (wellColumns, wellRows)
        if well_count <= 8 {
            (2, 4)
        } else if well_count <= 96 {
            (12, 8)
        } else {
            (24, 16)
        }
    }

    pub fn parse(path: &Path) -> Result<HcsAssembly> {
        let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
        let raw = std::fs::read(path).map_err(BioFormatsError::Io)?;
        // ScanR XML may be ISO-8859-1; decode leniently.
        let xml = String::from_utf8_lossy(&raw).to_string();

        let mut h = ScanrHandler::default();
        h.run(&xml);

        let mut well_rows = h.well_rows;
        let mut well_columns = h.well_columns;
        if well_rows == 0 || well_columns == 0 {
            let mut urows: Vec<String> = Vec::new();
            let mut ucols: Vec<String> = Vec::new();
            for w in h.well_labels.keys() {
                let first = w.chars().next().unwrap_or('0');
                if !first.is_alphabetic() {
                    continue;
                }
                let row = w[..1].trim().to_string();
                let col = w[1..].trim().to_string();
                if !row.is_empty() && !urows.contains(&row) {
                    urows.push(row);
                }
                if !col.is_empty() && !ucols.contains(&col) {
                    ucols.push(col);
                }
            }
            well_rows = urows.len() as i32;
            well_columns = ucols.len() as i32;
            if well_rows * well_columns != h.well_count as i32 {
                let (c, r) = adjust_well_dims(h.well_count);
                well_columns = c;
                well_rows = r;
            }
        }

        let n_channels = if h.size_c == 0 {
            h.channel_names.len().max(1)
        } else {
            (h.channel_names.len()).min(h.size_c as usize).max(1)
        } as i32;
        let n_slices = if h.size_z == 0 { 1 } else { h.size_z } as i32;
        let mut n_timepoints = h.size_t.max(0) as i32;
        let n_wells = h.well_count as i32;
        let n_pos = if h.found_positions {
            h.field_position_count.max(1) as i32
        } else {
            (h.field_rows * h.field_columns).max(1)
        };

        let data_dir = dir.join("data");
        let mut list = if data_dir.is_dir() {
            list_tiffs(&data_dir)
        } else {
            list_tiffs(&dir)
        };

        if n_timepoints == 0
            || (list.len() as i32) < n_timepoints * n_channels * n_slices * n_wells * n_pos
        {
            let denom = n_channels * n_wells * n_pos * n_slices;
            n_timepoints = if denom > 0 {
                (list.len() as i32) / denom
            } else {
                0
            };
            if n_timepoints == 0 {
                n_timepoints = 1;
            }
        }

        list.sort_by(|a, b| {
            let la = well_label_of(a);
            let lb = well_label_of(b);
            let ia = h.well_labels.get(&la).copied();
            let ib = h.well_labels.get(&lb).copied();
            match (ia, ib) {
                (Some(x), Some(y)) if x != y => x.cmp(&y),
                _ => a.cmp(b),
            }
        });

        let total = (n_channels * n_wells * n_pos * n_timepoints * n_slices).max(0) as usize;
        let mut tiffs: Vec<Option<PathBuf>> = vec![None; total];
        let mut next = 0usize;
        let mut last_list_index = 0usize;

        // Sorted well-label keys (row letter, then numeric column), mirroring
        // the Java `keys` array used to drop empty wells from `well_labels`.
        let mut keys: Vec<String> = h.well_labels.keys().cloned().collect();
        keys.sort_by(|s1, s2| {
            let r1 = s1.chars().next().unwrap_or('\0');
            let r2 = s2.chars().next().unwrap_or('\0');
            if r1 != r2 {
                return r1.cmp(&r2);
            }
            let c1: i32 = s1[1..].trim().parse().unwrap_or(0);
            let c2: i32 = s2[1..].trim().parse().unwrap_or(0);
            c1.cmp(&c2)
        });

        // Port of ScanrReader's skip-missing-wells loop. `well_numbers` is the
        // mutable map series->wellNumber; entries for fully-empty wells are
        // removed (skipMissingWells defaults to true), and `next` is NOT
        // advanced past their blank slots so present wells compact forward.
        let mut realpos_count = 0i32;
        for well in 0..n_wells {
            let mut missing_well_files = 0i32;
            let well_index = h.well_numbers.get(&well).copied().unwrap_or(well + 1);
            let well_pos = block(well_index, "W");
            let original_index = next;

            for pos in 0..n_pos {
                let pos_pos = block(pos + 1, "P");
                let pos_index = next;
                for z in 0..n_slices {
                    let z_pos = block(z, "Z");
                    for t in 0..n_timepoints {
                        let t_pos = block(t, "T");
                        for c in 0..n_channels {
                            let cname =
                                h.channel_names.get(c as usize).cloned().unwrap_or_default();
                            for i in last_list_index..list.len() {
                                let f = &list[i];
                                let fname = f.file_name().and_then(|n| n.to_str()).unwrap_or("");
                                if fname.contains(&well_pos)
                                    && fname.contains(&z_pos)
                                    && fname.contains(&pos_pos)
                                    && fname.contains(&t_pos)
                                    && (cname.is_empty() || fname.contains(&cname))
                                {
                                    if next < total {
                                        tiffs[next] = Some(f.clone());
                                    }
                                    next += 1;
                                    if c == n_channels - 1 {
                                        last_list_index = i;
                                    }
                                    break;
                                }
                            }
                            // Java: increments missingWellFiles whenever the
                            // whole well has produced nothing so far.
                            if next == original_index {
                                missing_well_files += 1;
                            }
                        }
                    }
                }
                if pos_index != next {
                    realpos_count += 1;
                }
            }
            // Drop empty well label (matches keys[] removal in Java).
            if next == original_index && (well as usize) < keys.len() {
                h.well_labels.remove(&keys[well as usize]);
            }
            // Fully-empty well: skip it (default), compacting later wells.
            if next == original_index
                && missing_well_files == n_slices * n_timepoints * n_channels * n_pos
            {
                h.well_numbers.remove(&well);
            }
        }
        let mut n_wells = h.well_numbers.len() as i32;

        // Recompute plate dimensions if labels were dropped (Java block).
        if !h.well_labels.is_empty() && h.well_labels.len() as i32 != n_wells {
            let mut urows: Vec<String> = Vec::new();
            let mut ucols: Vec<String> = Vec::new();
            for w in h.well_labels.keys() {
                if !w.chars().next().map(|c| c.is_alphabetic()).unwrap_or(false) {
                    continue;
                }
                let row = w[..1].trim().to_string();
                let col = w[1..].trim().to_string();
                if !row.is_empty() && !urows.contains(&row) {
                    urows.push(row);
                }
                if !col.is_empty() && !ucols.contains(&col) {
                    ucols.push(col);
                }
            }
            n_wells = (urows.len() * ucols.len()) as i32;
            let (c, r) = adjust_well_dims(n_wells as usize);
            well_columns = c;
            well_rows = r;
        }

        let mut n_pos = n_pos;
        if realpos_count < n_pos {
            n_pos = realpos_count;
        }

        let mut size_x = 1u32;
        let mut size_y = 1u32;
        let mut pixel_type = PixelType::Uint16;
        let mut little_endian = true;
        for t in tiffs.iter().flatten() {
            if let Some((sx, sy, pt, _b, le)) = super::probe_tiff(t) {
                size_x = sx;
                size_y = sy;
                // ScanR records signed pixels incorrectly; coerce to unsigned.
                pixel_type = match pt {
                    PixelType::Int8 => PixelType::Uint8,
                    PixelType::Int16 => PixelType::Uint16,
                    other => other,
                };
                little_endian = le;
                break;
            }
        }

        let series_count = (n_wells * n_pos).max(1) as usize;
        let order = DimensionOrder::XYCTZ;
        let size_c = n_channels.max(1) as u32;
        let size_z = n_slices.max(1) as u32;
        let size_t = n_timepoints.max(1) as u32;
        let image_count = (size_z * size_t * size_c) as usize;

        // Java `nFields` for store indexing: recomputed from the ORIGINAL field
        // geometry (NOT the realPosCount-clamped n_pos used for series_count).
        let n_fields = if h.found_positions {
            h.field_position_count.max(1) as i32
        } else {
            (h.field_rows * h.field_columns).max(1)
        };

        // Whether Java would populate per-plane Plane.* metadata at all.
        let populate_planes = h.delta_t.is_some()
            || h.exposures.len() >= size_c as usize
            || h.field_position_x.iter().any(|p| p.is_some())
            || h.field_position_y.iter().any(|p| p.is_some());

        // Java walks wellNumbers with a cursor that skips removed (None) entries,
        // converting the stored 1-based well number into a 0-based plate index.
        let mut well_index_cursor = 0i32;

        let mut series = Vec::with_capacity(series_count);
        let mut asm_planes: Vec<Vec<PlaneRef>> = Vec::with_capacity(series_count);
        for s in 0..series_count {
            let i = s as i32;
            let field = if n_fields > 0 { i % n_fields } else { 0 };
            let well = if n_fields > 0 { i / n_fields } else { 0 };

            // Mirror Java's wellNumbers cursor walk (store loop, ~line 650).
            while h.well_numbers.get(&well_index_cursor).is_none()
                && well_index_cursor < h.well_numbers.len() as i32
            {
                well_index_cursor += 1;
            }
            let well_index = match h.well_numbers.get(&well_index_cursor) {
                Some(n) => n - 1,
                None => well_index_cursor,
            };
            let well_row = if well_columns > 0 {
                well_index / well_columns
            } else {
                0
            };
            let well_col = if well_columns > 0 {
                well_index % well_columns
            } else {
                0
            };

            let mut meta = super::make_series_meta(
                size_x,
                size_y,
                size_z,
                size_c,
                size_t,
                pixel_type,
                12,
                little_endian,
                order,
                "Olympus ScanR",
            );

            // store.setPlateName / Plate dimensions (plate-level, repeated per series).
            if let Some(name) = &h.plate_name {
                meta.series_metadata.insert(
                    "Plate name".to_string(),
                    MetadataValue::String(name.clone()),
                );
            }
            meta.series_metadata
                .insert("PlateRows".to_string(), MetadataValue::Int(well_rows as i64));
            meta.series_metadata.insert(
                "PlateColumns".to_string(),
                MetadataValue::Int(well_columns as i64),
            );
            // store.setWellRow / setWellColumn.
            meta.series_metadata
                .insert("WellRow".to_string(), MetadataValue::Int(well_row as i64));
            meta.series_metadata.insert(
                "WellColumn".to_string(),
                MetadataValue::Int(well_col as i64),
            );

            // store.setChannelName(channelNames.get(c), i, c).
            for c in 0..size_c as usize {
                if let Some(cname) = h.channel_names.get(c) {
                    meta.series_metadata.insert(
                        format!("Channel {} Name", c),
                        MetadataValue::String(cname.clone()),
                    );
                }
            }

            // store.setPixelsPhysicalSizeX/Y (microns/pixel).
            if let Some(px) = h.pixel_size {
                meta.series_metadata.insert(
                    "PhysicalSizeX".to_string(),
                    MetadataValue::Float(px),
                );
                meta.series_metadata.insert(
                    "PhysicalSizeY".to_string(),
                    MetadataValue::Float(px),
                );
            }

            // store.setWellSamplePositionX/Y[field] (reference-frame lengths).
            if let Some(Some(px)) = h.field_position_x.get(field as usize) {
                meta.series_metadata.insert(
                    "WellSamplePositionX".to_string(),
                    MetadataValue::Float(*px),
                );
            }
            if let Some(Some(py)) = h.field_position_y.get(field as usize) {
                meta.series_metadata.insert(
                    "WellSamplePositionY".to_string(),
                    MetadataValue::Float(*py),
                );
            }

            if populate_planes {
                // store.setPlanePositionX/Y, ExposureTime, DeltaT, per plane.
                if let Some(Some(px)) = h.field_position_x.get(field as usize) {
                    meta.series_metadata.insert(
                        "PlanePositionX".to_string(),
                        MetadataValue::Float(*px),
                    );
                }
                if let Some(Some(py)) = h.field_position_y.get(field as usize) {
                    meta.series_metadata.insert(
                        "PlanePositionY".to_string(),
                        MetadataValue::Float(*py),
                    );
                }
                // exposure time per channel: ms -> seconds (store.setPlaneExposureTime).
                for c in 0..size_c as usize {
                    if let Some(Some(time_ms)) = h.exposures.get(c) {
                        meta.series_metadata.insert(
                            format!("Channel {} ExposureTime", c),
                            MetadataValue::Float(time_ms / 1000.0),
                        );
                    }
                }
                if let Some(dt) = h.delta_t {
                    meta.series_metadata
                        .insert("PlaneDeltaT".to_string(), MetadataValue::Float(dt));
                }
            }

            series.push(meta);
            if field == n_fields - 1 {
                well_index_cursor += 1;
            }
            let _ = well;

            // tiffs layout: index = series * image_count + plane (per Java openBytes).
            // tiffs is compacted by the skip-missing-wells loop above, so series
            // indices map only onto wells/positions that actually have data.
            let mut sp = vec![PlaneRef::default(); image_count];
            for plane in 0..image_count {
                let idx = s * image_count + plane;
                if let Some(Some(f)) = tiffs.get(idx) {
                    sp[plane] = PlaneRef::whole(f.clone(), 0);
                }
            }
            asm_planes.push(sp);
        }

        if series.is_empty() {
            return Err(BioFormatsError::Format(
                "Olympus ScanR: no series assembled".to_string(),
            ));
        }

        let mut asm = HcsAssembly::new();
        asm.series = series;
        asm.planes = asm_planes;
        Ok(asm)
    }

    fn list_tiffs(dir: &Path) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for e in entries.flatten() {
                let p = e.path();
                if p.is_file()
                    && p.extension()
                        .and_then(|x| x.to_str())
                        .map(|x| x.eq_ignore_ascii_case("tif") || x.eq_ignore_ascii_case("tiff"))
                        .unwrap_or(false)
                {
                    v.push(p);
                }
            }
        }
        v.sort();
        v
    }

    fn well_label_of(p: &Path) -> String {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        match name.find('-') {
            Some(i) => name[..i].to_string(),
            None => String::new(),
        }
    }

    #[derive(Default)]
    struct ScanrHandler {
        well_rows: i32,
        well_columns: i32,
        field_rows: i32,
        field_columns: i32,
        well_count: usize,
        size_c: u32,
        size_z: u32,
        size_t: u32,
        channel_names: Vec<String>,
        well_labels: Map<String, i32>,
        well_numbers: Map<i32, i32>,
        found_positions: bool,
        field_position_count: usize,
        /// Java `ScanrReader.plateName` ("plate name" Val).
        plate_name: Option<String>,
        /// Java `ScanrReader.pixelSize` ("conversion factor um/pixel" Val), microns/pixel.
        pixel_size: Option<f64>,
        /// Java `ScanrReader.exposures` ("exposure time" Vals), one per channel, in ms.
        exposures: Vec<Option<f64>>,
        /// Java `ScanrReader.deltaT` ("timeloop delay [ms]" Val) in seconds.
        delta_t: Option<f64>,
        /// Java `ScanrReader.fieldPositionX` (REFERENCEFRAME units), filled in
        /// subposition-list order. Sized lazily to `field_position_count`.
        field_position_x: Vec<Option<f64>>,
        /// Java `ScanrReader.fieldPositionY`.
        field_position_y: Vec<Option<f64>>,
        /// Java handler `nextXPos` cursor into `field_position_x`.
        next_x_pos: usize,
        /// Java handler `nextYPos` cursor into `field_position_y`.
        next_y_pos: usize,
    }

    impl ScanrHandler {
        fn run(&mut self, xml: &str) {
            let mut reader = quick_xml::Reader::from_str(xml);
            reader.config_mut().trim_text(false);
            let mut qname = String::new();
            let mut key = String::new();
            let mut valid_channel = false;
            let mut found_plate_layout = false;
            let mut well_index = String::new();
            let mut text = String::new();

            loop {
                match reader.read_event() {
                    Ok(quick_xml::events::Event::Start(ref e)) => {
                        qname = super::xmlutil::local_name(e);
                        text.clear();
                        if qname == "Array" || qname == "Cluster" {
                            valid_channel = true;
                        }
                    }
                    Ok(quick_xml::events::Event::Text(ref t)) => {
                        if let Ok(s) = t.unescape() {
                            text.push_str(&s);
                        }
                    }
                    Ok(quick_xml::events::Event::End(ref e)) => {
                        let v = text.trim().to_string();
                        if !v.is_empty() {
                            match qname.as_str() {
                                "Name" => {
                                    key = v.clone();
                                    if v == "subposition list" {
                                        self.found_positions = true;
                                    } else if v == "format typedef" {
                                        found_plate_layout = true;
                                    }
                                }
                                "Dimsize"
                                    if self.found_positions && self.field_position_count == 0 =>
                                {
                                    if let Ok(n) = v.parse::<usize>() {
                                        // Java: fieldPositionX/Y = new Length[nPositions].
                                        self.field_position_count = n;
                                        self.field_position_x = vec![None; n];
                                        self.field_position_y = vec![None; n];
                                    }
                                }
                                "Val" => {
                                    self.on_val(&key, &v, &mut valid_channel, &mut well_index);
                                }
                                _ => {
                                    if key == "Rows" && found_plate_layout {
                                        if let Ok(n) = v.parse() {
                                            self.well_rows = n;
                                        }
                                    } else if key == "Columns" && found_plate_layout {
                                        if let Ok(n) = v.parse() {
                                            self.well_columns = n;
                                        }
                                        found_plate_layout = false;
                                    }
                                }
                            }
                        }
                        let ln = super::xmlutil::local_name(e);
                        if ln == "Array" || ln == "Cluster" {
                            valid_channel = false;
                        }
                        text.clear();
                    }
                    Ok(quick_xml::events::Event::Eof) => break,
                    Err(_) => break,
                    _ => {}
                }
            }
        }

        fn on_val(
            &mut self,
            key: &str,
            v: &str,
            valid_channel: &mut bool,
            well_index: &mut String,
        ) {
            match key {
                "columns/well" => self.field_columns = v.parse().unwrap_or(0),
                "rows/well" => self.field_rows = v.parse().unwrap_or(0),
                "# slices" => self.size_z = v.parse().unwrap_or(0),
                "timeloop real" => self.size_t = v.parse().unwrap_or(0),
                "timeloop count" => self.size_t = v.parse::<u32>().unwrap_or(0) + 1,
                // Java: deltaT = Integer.parseInt(v) / 1000.0 (ms -> seconds).
                "timeloop delay [ms]" => {
                    if let Ok(n) = v.parse::<i64>() {
                        self.delta_t = Some(n as f64 / 1000.0);
                    }
                }
                "name" if *valid_channel => {
                    if !self.channel_names.contains(&v.to_string()) {
                        self.channel_names.push(v.to_string());
                    }
                }
                // Java: plateName = v.
                "plate name" => self.plate_name = Some(v.to_string()),
                // Java: exposures.add(DataTools.parseDouble(v)).
                "exposure time" => self.exposures.push(parse_double(v)),
                "idle" if *valid_channel => {
                    if let Some(last) = self.channel_names.last().cloned() {
                        if v == "0" && last != "Autofocus" {
                            self.size_c += 1;
                        } else {
                            // Java removes both the channel name and its exposure.
                            self.channel_names.pop();
                            self.exposures.pop();
                        }
                    }
                }
                "well selection table + cDNA" => {
                    if v.chars()
                        .next()
                        .map(|c| c.is_ascii_digit())
                        .unwrap_or(false)
                    {
                        *well_index = v.to_string();
                        if let Ok(n) = v.parse::<i32>() {
                            self.well_numbers.insert(self.well_count as i32, n);
                            self.well_count += 1;
                        }
                    } else if let Ok(n) = well_index.parse::<i32>() {
                        self.well_labels.insert(v.to_string(), n);
                    }
                }
                // Java: pixelSize = DataTools.parseDouble(v).
                "conversion factor um/pixel" => self.pixel_size = parse_double(v),
                // Java fall-through: subposition coordinates, X then Y, paired by
                // nextXPos == nextYPos. Values are reference-frame lengths.
                _ if self.found_positions => {
                    if self.next_x_pos == self.next_y_pos {
                        if self.next_x_pos < self.field_position_x.len() {
                            if let Some(n) = parse_double(v) {
                                self.field_position_x[self.next_x_pos] = Some(n);
                                self.next_x_pos += 1;
                            }
                        }
                    } else if self.next_y_pos < self.field_position_y.len() {
                        if let Some(n) = parse_double(v) {
                            self.field_position_y[self.next_y_pos] = Some(n);
                            self.next_y_pos += 1;
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

// ===========================================================================
// BD Pathway parser (Experiment.exp + .plt/.xyz/Well NN dirs) -- port of BDReader
// ===========================================================================

mod bd {
    use super::*;
    use std::collections::HashMap as Map;

    /// Minimal INI parser: returns section -> (key -> value).
    fn parse_ini(text: &str) -> Map<String, Map<String, String>> {
        let mut out: Map<String, Map<String, String>> = Map::new();
        let mut section = String::new();
        out.insert(String::new(), Map::new());
        for line in text.lines() {
            let l = line.trim();
            if l.is_empty() || l.starts_with(';') || l.starts_with('#') {
                continue;
            }
            if l.starts_with('[') && l.ends_with(']') {
                section = l[1..l.len() - 1].trim().to_string();
                out.entry(section.clone()).or_default();
            } else if let Some(eq) = l.find('=') {
                let k = l[..eq].trim().to_string();
                let v = l[eq + 1..].trim().to_string();
                out.entry(section.clone()).or_default().insert(k, v);
            }
        }
        out
    }

    fn get<'a>(
        ini: &'a Map<String, Map<String, String>>,
        sect: &str,
        key: &str,
    ) -> Option<&'a String> {
        ini.get(sect).and_then(|s| s.get(key))
    }

    pub fn parse(path: &Path) -> Result<HcsAssembly> {
        // Locate Experiment.exp.
        let exp_path = if path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("exp"))
            .unwrap_or(false)
        {
            path.to_path_buf()
        } else {
            let parent = path.parent().unwrap_or(Path::new("."));
            parent.join("Experiment.exp")
        };
        let dir = exp_path.parent().unwrap_or(Path::new(".")).to_path_buf();

        let exp_text = std::fs::read_to_string(&exp_path).map_err(BioFormatsError::Io)?;
        let exp = parse_ini(&exp_text);

        // Find the .plt (plate type) file in the directory tree.
        let mut well_rows = 0i32;
        let mut well_cols = 0i32;
        let mut z_axis_value: Option<f64> = None;
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for e in entries.flatten() {
                let p = e.path();
                let ext = p
                    .extension()
                    .and_then(|x| x.to_str())
                    .map(|x| x.to_ascii_lowercase());
                if ext.as_deref() == Some("plt") {
                    if let Ok(t) = std::fs::read_to_string(&p) {
                        let plt = parse_ini(&t);
                        if let Some(w) = get(&plt, "PlateType", "Wells") {
                            match w.trim().parse::<i32>() {
                                Ok(96) => {
                                    well_rows = 8;
                                    well_cols = 12;
                                }
                                Ok(384) => {
                                    well_rows = 16;
                                    well_cols = 24;
                                }
                                _ => {}
                            }
                        }
                    }
                } else if ext.as_deref() == Some("xyz") {
                    if let Ok(t) = std::fs::read_to_string(&p) {
                        let xyz = parse_ini(&t);
                        let enabled = get(&xyz, "Z1Axis", "Z1AxisEnabled")
                            .map(|s| s == "1")
                            .unwrap_or(false)
                            && get(&xyz, "Z1Axis", "Z1AxisMode")
                                .map(|s| s == "1")
                                .unwrap_or(false);
                        if enabled {
                            z_axis_value = get(&xyz, "Z1Axis", "Z1AxisValue")
                                .and_then(|s| s.trim().parse::<f64>().ok());
                        }
                    }
                }
            }
        }

        // Channels (dyes) from [General].Dyes + [Dyes] table.
        let n_dyes = get(&exp, "General", "Dyes")
            .and_then(|s| s.trim().parse::<i32>().ok())
            .unwrap_or(0);
        let mut channel_names: Vec<String> = Vec::new();
        for i in 1..=n_dyes {
            if let Some(name) = get(&exp, "Dyes", &i.to_string()) {
                channel_names.push(name.clone());
            }
        }
        if channel_names.is_empty() {
            channel_names.push("Channel 0".to_string());
        }
        let n_channels = channel_names.len() as i32;

        let bits = get(&exp, "Camera", "BitdepthUsed")
            .and_then(|s| s.trim().parse::<u8>().ok())
            .unwrap_or(16);

        // Montage (fields packed in a single TIFF).
        let montage = get(&exp, "Image", "Montaged")
            .map(|s| s == "1")
            .unwrap_or(false);
        let (field_rows, field_cols) = if montage {
            (
                get(&exp, "Image", "TilesY")
                    .and_then(|s| s.trim().parse::<i32>().ok())
                    .unwrap_or(1),
                get(&exp, "Image", "TilesX")
                    .and_then(|s| s.trim().parse::<i32>().ok())
                    .unwrap_or(1),
            )
        } else {
            (1, 1)
        };
        let n_fields = (field_rows * field_cols).max(1);

        let size_z = if let Some(zv) = z_axis_value {
            (zv as i32 + 1).max(1)
        } else {
            1
        } as u32;

        // Scan "Well NN" directories.
        let mut well_dirs: Vec<(String, PathBuf)> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            let mut all: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
            all.sort();
            for p in all {
                if p.is_dir() {
                    if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                        if let Some(rest) = name.strip_prefix("Well ") {
                            // label is first token after "Well " split on whitespace/'.'
                            let label = rest
                                .split(|c: char| c.is_whitespace() || c == '.')
                                .next()
                                .unwrap_or("")
                                .to_string();
                            if !label.is_empty() {
                                well_dirs.push((label, p));
                            }
                        }
                    }
                }
            }
        }
        if well_dirs.is_empty() {
            return Err(BioFormatsError::Format(
                "BD Pathway: no 'Well NN' directories found".to_string(),
            ));
        }

        // Collect per-well tiff lists matching ".* - nNNNNNN.tif".
        let mut well_tiffs: Vec<(String, Vec<PathBuf>)> = Vec::new();
        for (label, wdir) in &well_dirs {
            let mut tiffs: Vec<PathBuf> = Vec::new();
            if let Ok(entries) = std::fs::read_dir(wdir) {
                for e in entries.flatten() {
                    let p = e.path();
                    if matches_bd_tiff(&p) {
                        tiffs.push(p);
                    }
                }
            }
            tiffs.sort();
            well_tiffs.push((label.clone(), tiffs));
        }

        // Determine sizeT by counting per-channel images in a well.
        // Mirror Java BDReader.java:668-680: a running imageCount starts at 0,
        // so the first channel with any images sets sizeT = images/sizeZ, and
        // later channels only update if they have more images than the running count.
        // Java counts the SECOND well directory (wellList.get(1),
        // BDReader.java:671), not the first non-empty one; we guard the length
        // (Java would otherwise throw IndexOutOfBounds with a single well).
        let mut size_t = 0u32;
        if let Some((_, tiffs)) = well_tiffs.get(1) {
            let mut image_count = 0u32;
            for cname in &channel_names {
                let images = tiffs
                    .iter()
                    .filter(|p| {
                        p.file_name()
                            .and_then(|n| n.to_str())
                            .map(|n| n.starts_with(cname.as_str()) && n.ends_with(".tif"))
                            .unwrap_or(false)
                    })
                    .count() as u32;
                if images > image_count {
                    size_t = images / size_z.max(1);
                    image_count = size_z.max(1) * size_t * n_channels as u32;
                }
            }
        }
        let size_t = size_t.max(1);
        let size_c = n_channels.max(1) as u32;

        // Probe first TIFF for pixel parameters.
        let mut full_x = 0u32;
        let mut full_y = 0u32;
        let mut pixel_type = PixelType::Uint16;
        let mut bits_pp = bits;
        let mut little_endian = true;
        for (_, tiffs) in &well_tiffs {
            if let Some(p) = tiffs.first() {
                if let Some((sx, sy, pt, b, le)) = super::probe_tiff(p) {
                    full_x = sx;
                    full_y = sy;
                    pixel_type = pt;
                    bits_pp = b;
                    little_endian = le;
                    break;
                }
            }
        }
        let size_x = (full_x / field_cols.max(1) as u32).max(1);
        let size_y = (full_y / field_rows.max(1) as u32).max(1);

        let order = DimensionOrder::XYZTC;
        let image_count = (size_z * size_t * size_c) as usize;
        let series_count = well_tiffs.len() * n_fields as usize;

        let mut series = Vec::with_capacity(series_count);
        let mut asm_planes: Vec<Vec<PlaneRef>> = Vec::with_capacity(series_count);

        for (_label, tiffs) in well_tiffs.iter() {
            for field in 0..n_fields {
                series.push(super::make_series_meta(
                    size_x,
                    size_y,
                    size_z,
                    size_c,
                    size_t,
                    pixel_type,
                    bits_pp,
                    little_endian,
                    order,
                    "BD Pathway",
                ));
                // Montaged datasets pack all fields in one TIFF; split them per
                // the Java openBytes: fieldRow = field/fieldCols,
                // fieldCol = field%fieldCols, and the sub-region is
                // (fieldCol*sizeX, fieldRow*sizeY, sizeX, sizeY). Single-field
                // datasets read the whole plane.
                let field_row = field / field_cols.max(1);
                let field_col = field % field_cols.max(1);
                let off_x = field_col as u32 * size_x;
                let off_y = field_row as u32 * size_y;
                // Map each plane to its file via getFilename logic.
                let mut sp = vec![PlaneRef::default(); image_count];
                for plane in 0..image_count {
                    let (z, c, t) =
                        super::get_zct_coords(order, size_z, size_c, size_t, plane as u32);
                    if let Some(f) =
                        bd_filename(tiffs, &channel_names, c, z, t, order, size_z, size_t)
                    {
                        sp[plane] = if n_fields == 1 {
                            PlaneRef::whole(f, 0)
                        } else {
                            PlaneRef {
                                tiles: vec![Tile {
                                    filename: f,
                                    file_index: 0,
                                    src_x: off_x,
                                    src_y: off_y,
                                    src_w: size_x,
                                    src_h: size_y,
                                    dst_x: 0,
                                    dst_y: 0,
                                }],
                            }
                        };
                    }
                }
                asm_planes.push(sp);
            }
        }
        let _ = (well_rows, well_cols);

        let mut asm = HcsAssembly::new();
        asm.series = series;
        asm.planes = asm_planes;
        Ok(asm)
    }

    fn matches_bd_tiff(p: &Path) -> bool {
        // Pattern ".* - nDDDDDD.tif"
        let name = match p.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => return false,
        };
        if !name.ends_with(".tif") {
            return false;
        }
        let stem = &name[..name.len() - 4];
        if let Some(pos) = stem.rfind(" - n") {
            let digits = &stem[pos + 4..];
            digits.len() == 6 && digits.chars().all(|c| c.is_ascii_digit())
        } else {
            false
        }
    }

    /// getFilename: channel = channelNames[c]; realIndex = getIndex(z,0,t);
    /// match name starting with channel and trailing nNNNNNN == realIndex.
    #[allow(clippy::too_many_arguments)]
    fn bd_filename(
        tiffs: &[PathBuf],
        channel_names: &[String],
        c: u32,
        z: u32,
        t: u32,
        order: DimensionOrder,
        size_z: u32,
        size_t: u32,
    ) -> Option<PathBuf> {
        let channel = channel_names.get(c as usize)?;
        // Java: getIndex(z, 0, t) with sizeC forced to 1 (channel separated by name).
        let real_index = super::get_index(order, size_z, 1, size_t, z, 0, t);
        for p in tiffs {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let stem = name.strip_suffix(".tif").unwrap_or(name);
            if let Some(npos) = stem.rfind('n') {
                let idx_str = &stem[npos + 1..];
                if let Ok(idx) = idx_str.parse::<u32>() {
                    if name.starts_with(channel.as_str()) && idx == real_index {
                        return Some(p.clone());
                    }
                }
            }
        }
        None
    }
}

// ===========================================================================
// CellVoyager parser (MeasurementResult.xml)  -- port of CellVoyagerReader
//
// Faithful to the Java geometry parsing (channels/wells/areas/fields/Z/T and
// per-field pixel offsets), producing one series per well x area. Each series
// plane is stitched on the fly from all of the area's field tiles, each placed
// at its (xpixels, ypixels) offset within the reconstructed area image --
// mirroring the Java openBytes tile-paste loop.
// ===========================================================================

mod cellvoyager {
    use super::*;

    #[derive(Default, Clone)]
    struct Field {
        index: i32,
        // Stage position in micrometres; consumed for min/max during area
        // sizing.
        x: f64,
        y: f64,
        // Pixel offset of this tile within the reconstructed area image
        // (Java FieldInfo.xpixels / ypixels).
        xpixels: i64,
        ypixels: i64,
    }

    #[derive(Default, Clone)]
    struct Area {
        fields: Vec<Field>,
        width: i32,
        height: i32,
    }

    #[derive(Default, Clone)]
    struct Well {
        /// Well number from XML. Retained for metadata fidelity; the filename
        /// uses the wells-list position per Java, not this value.
        #[allow(dead_code)]
        number: i32,
        areas: Vec<Area>,
    }

    pub fn parse(path: &Path) -> Result<HcsAssembly> {
        // Resolve the measurement folder + Image dir.
        let start = path;
        let measurement_folder = if start.is_dir() {
            start.to_path_buf()
        } else {
            let mut p = start.parent().unwrap_or(Path::new(".")).to_path_buf();
            if p.file_name().and_then(|n| n.to_str()) == Some("Image") {
                p = p.parent().unwrap_or(Path::new(".")).to_path_buf();
            }
            p
        };
        let image_folder = measurement_folder.join("Image");
        let ms_file = measurement_folder.join("MeasurementResult.xml");
        let ms_file = if ms_file.exists() {
            ms_file
        } else if start.is_file() {
            start.to_path_buf()
        } else {
            ms_file
        };

        let xml = std::fs::read_to_string(&ms_file).map_err(BioFormatsError::Io)?;

        // Parse with the lightweight DOM builder.
        let dom = dom::parse(&xml);
        let root = dom.root();

        let magnification = root
            .child_text(&["ObjectiveLens", "Magnification"])
            .and_then(|s| s.trim().parse::<f64>().ok())
            .unwrap_or(1.0)
            .max(1e-9);

        // Channels: enabled only; collect tile size + unmagnified pixel size.
        let mut tile_w = 0i32;
        let mut tile_h = 0i32;
        let mut unmag_px_w = 1.0f64;
        let mut unmag_px_h = 1.0f64;
        let mut channel_names: Vec<String> = Vec::new();
        if let Some(channels_el) = root.child(&["Channels"]) {
            for ch in channels_el.children("Channel") {
                let enabled = ch
                    .child_text(&["IsEnabled"])
                    .map(|s| s.trim().eq_ignore_ascii_case("true"))
                    .unwrap_or(false);
                if !enabled {
                    continue;
                }
                if channel_names.is_empty() {
                    if let Some(cam) = ch.child(&["AcquisitionSetting", "Camera"]) {
                        tile_w = cam
                            .child_text(&["EffectiveHorizontalPixels_pixel"])
                            .and_then(|s| s.trim().parse().ok())
                            .unwrap_or(0);
                        tile_h = cam
                            .child_text(&["EffectiveVerticalPixels_pixel"])
                            .and_then(|s| s.trim().parse().ok())
                            .unwrap_or(0);
                        unmag_px_w = cam
                            .child_text(&["HorizonalCellSize_um"])
                            .and_then(|s| s.trim().parse().ok())
                            .unwrap_or(1.0);
                        unmag_px_h = cam
                            .child_text(&["VerticalCellSize_um"])
                            .and_then(|s| s.trim().parse().ok())
                            .unwrap_or(1.0);
                    }
                }
                let number = ch.child_text(&["Number"]).unwrap_or_default();
                channel_names.push(format!("Channel {}", number.trim()));
            }
        }
        if channel_names.is_empty() {
            return Err(BioFormatsError::Format(
                "CellVoyager: no enabled channels in MeasurementResult.xml".to_string(),
            ));
        }
        // Java CellVoyagerReader reads PhysicalSizeX/Y from the companion
        // MeasurementResult.ome.xml (Image/Pixels attributes) and divides by
        // magnification (CellVoyagerReader.java:533-534, 589-590). Tile
        // placement (xpixels = round((x-xmin)/pixelWidth)) depends on this, so
        // we read it from the OME XML; we only fall back to the camera
        // cell-size / magnification when the OME XML is absent or unparsable.
        let ome_file = measurement_folder.join("MeasurementResult.ome.xml");
        let ome_phys = std::fs::read_to_string(&ome_file).ok().and_then(|s| {
            let ome_dom = dom::parse(&s);
            let pixels = ome_dom.root().child(&["Image", "Pixels"])?;
            let px = pixels
                .attr("PhysicalSizeX")
                .and_then(|v| v.trim().parse::<f64>().ok());
            let py = pixels
                .attr("PhysicalSizeY")
                .and_then(|v| v.trim().parse::<f64>().ok());
            match (px, py) {
                (Some(px), Some(py)) => Some((px, py)),
                _ => None,
            }
        });
        let (pixel_width, pixel_height) = match ome_phys {
            Some((px, py)) => (
                (px / magnification).max(1e-9),
                (py / magnification).max(1e-9),
            ),
            None => (
                (unmag_px_w / magnification).max(1e-9),
                (unmag_px_h / magnification).max(1e-9),
            ),
        };

        // Areas may be shared per-well or defined per-well.
        let same_area_per_well = root
            .child_text(&["UsesSameAreaParWell"])
            .map(|s| s.trim().eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        let shared_areas = if same_area_per_well {
            root.child(&["SameAreaUsingWell", "Areas"]).map(|areas_el| {
                let mut field_index = 1;
                let mut out = Vec::new();
                for a in areas_el.children("Area") {
                    let area = read_area(
                        &a,
                        &mut field_index,
                        pixel_width,
                        pixel_height,
                        tile_w,
                        tile_h,
                    );
                    out.push(area);
                }
                out
            })
        } else {
            None
        };

        let mut wells: Vec<Well> = Vec::new();
        if let Some(wells_el) = root.child(&["Wells"]) {
            for w in wells_el.children("Well") {
                let enabled = w
                    .child_text(&["IsEnabled"])
                    .map(|s| s.trim().eq_ignore_ascii_case("true"))
                    .unwrap_or(false);
                if !enabled {
                    continue;
                }
                let number = w
                    .child_text(&["Number"])
                    .and_then(|s| s.trim().parse().ok())
                    .unwrap_or(0);
                let areas = if let Some(shared) = &shared_areas {
                    shared.clone()
                } else if let Some(areas_el) = w.child(&["Areas"]) {
                    let mut field_index = 1;
                    areas_el
                        .children("Area")
                        .iter()
                        .map(|a| {
                            read_area(
                                a,
                                &mut field_index,
                                pixel_width,
                                pixel_height,
                                tile_w,
                                tile_h,
                            )
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                wells.push(Well { number, areas });
            }
        }
        if wells.is_empty() {
            return Err(BioFormatsError::Format(
                "CellVoyager: no enabled wells in MeasurementResult.xml".to_string(),
            ));
        }

        let n_z = root
            .child_text(&["ZStackConditions", "NumberOfSlices"])
            .and_then(|s| s.trim().parse::<i32>().ok())
            .unwrap_or(1)
            .max(1) as u32;
        let n_t = root
            .child_text(&["TimelapsCondition", "Iteration"])
            .and_then(|s| s.trim().parse::<i32>().ok())
            .unwrap_or(1)
            .max(1) as u32;
        let n_c = channel_names.len() as u32;
        let order = DimensionOrder::XYCZT;
        let image_count = (n_z * n_c * n_t) as usize;

        // Probe pixel type from any existing field-0 tile.
        let mut pixel_type = PixelType::Uint16;
        let mut bits = 16u8;
        let mut little_endian = true;
        'probe: for (wi, well) in wells.iter().enumerate() {
            for area in &well.areas {
                if let Some(f) = area.fields.first() {
                    let fname = single_tiff_name(wi as i32 + 1, f.index, 1, 1, 1);
                    let p = image_folder.join(&fname);
                    if let Some((_x, _y, pt, b, le)) = super::probe_tiff(&p) {
                        pixel_type = pt;
                        bits = b;
                        little_endian = le;
                        break 'probe;
                    }
                }
            }
        }

        // Build one series per well x area. Each area plane is stitched from
        // all of the area's field tiles, placing each tile at its pixel offset
        // (Java openBytes loops over area.fields and pastes each tile). The
        // well index used in the filename is the position in the wells list
        // (wi + 1), matching Java's seriesToWellArea / SINGLE_TIFF_PATH_BUILDER.
        let mut series = Vec::new();
        let mut asm_planes: Vec<Vec<PlaneRef>> = Vec::new();
        for (wi, well) in wells.iter().enumerate() {
            let well_index = wi as i32 + 1;
            for area in &well.areas {
                let size_x = area.width.max(tile_w).max(1) as u32;
                let size_y = area.height.max(tile_h).max(1) as u32;
                series.push(super::make_series_meta(
                    size_x,
                    size_y,
                    n_z,
                    n_c,
                    n_t,
                    pixel_type,
                    bits,
                    little_endian,
                    order,
                    "CellVoyager",
                ));
                let mut sp = vec![PlaneRef::default(); image_count];
                for plane in 0..image_count {
                    let (z, c, t) = super::get_zct_coords(order, n_z, n_c, n_t, plane as u32);
                    let mut tiles: Vec<Tile> = Vec::with_capacity(area.fields.len());
                    for field in &area.fields {
                        // SINGLE_TIFF_PATH_BUILDER = "W%dF%03dT%04dZ%02dC%d.tif"
                        let fname = single_tiff_name(
                            well_index,
                            field.index,
                            t as i32 + 1,
                            z as i32 + 1,
                            c as i32 + 1,
                        );
                        let p = image_folder.join(&fname);
                        // Place the whole tile at its pixel offset within the
                        // reconstructed area image. src_w/src_h = 0 -> the
                        // compositor reads the full tile plane.
                        tiles.push(Tile {
                            filename: p,
                            file_index: 0,
                            src_x: 0,
                            src_y: 0,
                            src_w: 0,
                            src_h: 0,
                            dst_x: field.xpixels.max(0) as u32,
                            dst_y: field.ypixels.max(0) as u32,
                        });
                    }
                    sp[plane] = PlaneRef { tiles };
                }
                asm_planes.push(sp);
            }
        }

        if series.is_empty() {
            return Err(BioFormatsError::Format(
                "CellVoyager: no series assembled".to_string(),
            ));
        }

        let mut asm = HcsAssembly::new();
        asm.series = series;
        asm.planes = asm_planes;
        Ok(asm)
    }

    fn single_tiff_name(well: i32, field: i32, t: i32, z: i32, c: i32) -> String {
        format!("W{}F{:03}T{:04}Z{:02}C{}.tif", well, field, t, z, c)
    }

    fn read_area(
        area_el: &dom::Node,
        starting_field_index: &mut i32,
        pixel_width: f64,
        pixel_height: f64,
        tile_w: i32,
        tile_h: i32,
    ) -> Area {
        let mut fields: Vec<Field> = Vec::new();
        let mut xmin = f64::INFINITY;
        let mut ymin = f64::INFINITY;
        let mut xmax = f64::NEG_INFINITY;
        let mut ymax = f64::NEG_INFINITY;

        if let Some(fields_el) = area_el.child(&["Fields"]) {
            for f in fields_el.children("Field") {
                let x = f
                    .child_text(&["StageX_um"])
                    .and_then(|s| s.trim().parse::<f64>().ok())
                    .unwrap_or(0.0);
                let y = f
                    .child_text(&["StageY_um"])
                    .and_then(|s| s.trim().parse::<f64>().ok())
                    .unwrap_or(0.0);
                xmin = xmin.min(x);
                xmax = xmax.max(x);
                let yum = -y;
                ymin = ymin.min(yum);
                ymax = ymax.max(yum);
                fields.push(Field {
                    index: 0,
                    x,
                    y,
                    xpixels: 0,
                    ypixels: 0,
                });
            }
        }
        for f in fields.iter_mut() {
            // Java: xpixels = round((x - xmin)/pixelWidth);
            //       ypixels = round((-ymin - y)/pixelHeight).
            f.xpixels = ((f.x - xmin) / pixel_width).round() as i64;
            f.ypixels = ((-ymin - f.y) / pixel_height).round() as i64;
            f.index = *starting_field_index;
            *starting_field_index += 1;
        }
        let (width, height) = if fields.is_empty() {
            (0, 0)
        } else {
            (
                1 + ((xmax - xmin) / pixel_width) as i32,
                1 + ((ymax - ymin) / pixel_height) as i32,
            )
        };
        Area {
            fields,
            width: width + tile_w,
            height: height + tile_h,
        }
    }

    // -- Minimal read-only DOM for navigating MeasurementResult.xml --
    mod dom {
        use quick_xml::events::Event;

        #[derive(Default)]
        pub struct Node {
            pub name: String,
            pub text: String,
            pub attrs: Vec<(String, String)>,
            pub children: Vec<Node>,
        }

        pub struct Dom {
            root: Node,
        }

        impl Dom {
            pub fn root(&self) -> &Node {
                &self.root
            }
        }

        impl Node {
            /// Descend a path of local element names, returning the node.
            pub fn child(&self, path: &[&str]) -> Option<&Node> {
                let mut cur = self;
                for seg in path {
                    cur = cur.children.iter().find(|c| c.name == *seg)?;
                }
                Some(cur)
            }

            /// Text content at the end of a path.
            pub fn child_text(&self, path: &[&str]) -> Option<String> {
                self.child(path).map(|n| n.text.clone())
            }

            /// All direct children with the given local name.
            pub fn children(&self, name: &str) -> Vec<&Node> {
                self.children.iter().filter(|c| c.name == name).collect()
            }

            /// Attribute value by (local) name.
            pub fn attr(&self, name: &str) -> Option<&str> {
                self.attrs
                    .iter()
                    .find(|(k, _)| k == name)
                    .map(|(_, v)| v.as_str())
            }
        }

        fn collect_attrs(e: &quick_xml::events::BytesStart) -> Vec<(String, String)> {
            let mut out = Vec::new();
            for a in e.attributes().flatten() {
                let k = local(a.key.as_ref());
                let v = a
                    .unescape_value()
                    .map(|c| c.into_owned())
                    .unwrap_or_else(|_| String::from_utf8_lossy(&a.value).into_owned());
                out.push((k, v));
            }
            out
        }

        fn local(name: &[u8]) -> String {
            let l = match name.iter().position(|&b| b == b':') {
                Some(i) => &name[i + 1..],
                None => name,
            };
            String::from_utf8_lossy(l).to_string()
        }

        pub fn parse(xml: &str) -> Dom {
            let mut reader = quick_xml::Reader::from_str(xml);
            reader.config_mut().trim_text(false);
            let mut stack: Vec<Node> = vec![Node {
                name: "__root__".to_string(),
                ..Default::default()
            }];
            loop {
                match reader.read_event() {
                    Ok(Event::Start(ref e)) => {
                        stack.push(Node {
                            name: local(e.name().as_ref()),
                            attrs: collect_attrs(e),
                            ..Default::default()
                        });
                    }
                    Ok(Event::Empty(ref e)) => {
                        let n = Node {
                            name: local(e.name().as_ref()),
                            attrs: collect_attrs(e),
                            ..Default::default()
                        };
                        if let Some(parent) = stack.last_mut() {
                            parent.children.push(n);
                        }
                    }
                    Ok(Event::Text(ref t)) => {
                        if let Ok(s) = t.unescape() {
                            if let Some(top) = stack.last_mut() {
                                top.text.push_str(&s);
                            }
                        }
                    }
                    Ok(Event::CData(ref t)) => {
                        if let Some(top) = stack.last_mut() {
                            top.text.push_str(&String::from_utf8_lossy(t.as_ref()));
                        }
                    }
                    Ok(Event::End(_)) => {
                        if stack.len() > 1 {
                            let node = stack.pop().unwrap();
                            // Trim accumulated text.
                            let mut node = node;
                            node.text = node.text.trim().to_string();
                            if let Some(parent) = stack.last_mut() {
                                parent.children.push(node);
                            }
                        }
                    }
                    Ok(Event::Eof) => break,
                    Err(_) => break,
                    _ => {}
                }
            }
            // The real document root is the single child of __root__.
            let mut root = stack.pop().unwrap_or_default();
            let real = root.children.pop().unwrap_or_default();
            Dom { root: real }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::writer::FormatWriter;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

    fn temp_path(name: &str) -> PathBuf {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("bioformats_hcs2_{id}_{name}"))
    }

    fn test_meta(width: u32, height: u32) -> ImageMetadata {
        make_series_meta(
            width,
            height,
            1,
            1,
            1,
            PixelType::Uint8,
            8,
            true,
            DimensionOrder::XYZCT,
            "HCS test",
        )
    }

    fn assembly_with_plane(meta: ImageMetadata, plane: PlaneRef) -> HcsAssembly {
        let mut asm = HcsAssembly::new();
        asm.series = vec![meta];
        asm.planes = vec![vec![plane]];
        asm
    }

    fn write_tiff(path: &Path, meta: &ImageMetadata, data: &[u8]) {
        let mut writer = crate::tiff::TiffWriter::new();
        writer.set_metadata(meta).unwrap();
        writer.set_id(path).unwrap();
        writer.save_bytes(0, data).unwrap();
        writer.close().unwrap();
    }

    fn tiff_entry(tag: u16, typ: u16, count: u32, value: u32) -> [u8; 12] {
        let mut entry = [0u8; 12];
        entry[0..2].copy_from_slice(&tag.to_le_bytes());
        entry[2..4].copy_from_slice(&typ.to_le_bytes());
        entry[4..8].copy_from_slice(&count.to_le_bytes());
        entry[8..12].copy_from_slice(&value.to_le_bytes());
        entry
    }

    fn write_tiff_with_description(path: &Path, description: &str, pixel: u8) {
        let mut desc = description.as_bytes().to_vec();
        desc.push(0);

        let entries = [
            tiff_entry(256, 4, 1, 1),
            tiff_entry(257, 4, 1, 1),
            tiff_entry(258, 3, 1, 8),
            tiff_entry(259, 3, 1, 1),
            tiff_entry(262, 3, 1, 1),
            tiff_entry(270, 2, desc.len() as u32, 0),
            tiff_entry(273, 4, 1, 0),
            tiff_entry(277, 3, 1, 1),
            tiff_entry(278, 4, 1, 1),
            tiff_entry(279, 4, 1, 1),
            tiff_entry(284, 3, 1, 1),
        ];
        let ifd_start = 8u32;
        let desc_start = ifd_start + 2 + (entries.len() as u32) * 12 + 4;
        let pixel_start = desc_start + desc.len() as u32;

        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"II");
        bytes.extend_from_slice(&42u16.to_le_bytes());
        bytes.extend_from_slice(&ifd_start.to_le_bytes());
        bytes.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        for mut entry in entries {
            if u16::from_le_bytes([entry[0], entry[1]]) == 270 {
                entry[8..12].copy_from_slice(&desc_start.to_le_bytes());
            } else if u16::from_le_bytes([entry[0], entry[1]]) == 273 {
                entry[8..12].copy_from_slice(&pixel_start.to_le_bytes());
            }
            bytes.extend_from_slice(&entry);
        }
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&desc);
        bytes.push(pixel);

        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn simplepci_tiff_projects_description_metadata_and_delegates_pixels() {
        let path = temp_path("simplepci_metadata.tif");
        write_tiff_with_description(
            &path,
            "Created by SimplePCI HCImage\nExposure Time=12.5\nChannel Name=DAPI\nWell=A01\n",
            91,
        );

        let mut reader = SimplePciTiffReader::new();
        reader.set_id(&path).unwrap();
        let metadata = &reader.metadata().series_metadata;

        assert!(matches!(
            metadata.get("hcs2.wrapper"),
            Some(MetadataValue::String(value)) if value == "SimplePciTiffReader"
        ));
        assert!(matches!(
            metadata.get("simplepci.software"),
            Some(MetadataValue::String(value)) if value == "SimplePCI HCImage"
        ));
        assert!(matches!(
            metadata.get("simplepci.exposure_time"),
            Some(MetadataValue::Float(value)) if (*value - 12.5).abs() < f64::EPSILON
        ));
        assert!(matches!(
            metadata.get("simplepci.channel_name"),
            Some(MetadataValue::String(value)) if value == "DAPI"
        ));
        assert!(matches!(
            metadata.get("simplepci.well"),
            Some(MetadataValue::String(value)) if value == "A01"
        ));
        assert_eq!(reader.open_bytes(0).unwrap(), vec![91]);
        assert_eq!(reader.open_bytes_region(0, 0, 0, 1, 1).unwrap(), vec![91]);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn simplepci_tiff_ignores_plain_tiff_descriptions_for_vendor_metadata() {
        let path = temp_path("simplepci_plain_description.tif");
        write_tiff_with_description(&path, "Exposure Time=12.5\nWell=A01\n", 7);

        let mut reader = SimplePciTiffReader::new();
        reader.set_id(&path).unwrap();

        assert!(reader
            .metadata()
            .series_metadata
            .contains_key("hcs2.wrapper"));
        assert!(
            !reader
                .metadata()
                .series_metadata
                .keys()
                .any(|key| key.starts_with("simplepci.")),
            "plain TIFF descriptions should not get SimplePCI vendor metadata"
        );
        assert_eq!(reader.open_bytes(0).unwrap(), vec![7]);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn simplepci_tiff_preserves_nested_xml_object_scalars() {
        let path = temp_path("simplepci_nested_xml_metadata.tif");
        write_tiff_with_description(
            &path,
            "Created by HCImage\n\
<HCImage>\n\
  <Acquisition RunName=\"Assay 7\">\n\
    <Channel Name=\"TRITC\" Wavelength=\"561\">\n\
      <Objective Magnification=\"60\" NumericAperture=\"1.4\"/>\n\
    </Channel>\n\
    <Camera SerialNumber=\"CAM-17\">\n\
      <Gain>2.5</Gain>\n\
    </Camera>\n\
  </Acquisition>\n\
</HCImage>\n",
            19,
        );

        let mut reader = SimplePciTiffReader::new();
        reader.set_id(&path).unwrap();
        let metadata = &reader.metadata().series_metadata;

        assert!(matches!(
            metadata.get("simplepci.software"),
            Some(MetadataValue::String(value)) if value == "HCImage"
        ));
        assert!(matches!(
            metadata.get("simplepci.xml_scalar_count"),
            Some(MetadataValue::Int(7))
        ));
        assert!(matches!(
            metadata.get("simplepci.hierarchy.node_count"),
            Some(MetadataValue::Int(5))
        ));
        assert!(matches!(
            metadata.get("simplepci.hierarchy.scalar_count"),
            Some(MetadataValue::Int(7))
        ));
        assert!(matches!(
            metadata.get("simplepci.hierarchy.0.path"),
            Some(MetadataValue::String(value)) if value == "acquisition"
        ));
        assert!(matches!(
            metadata.get("simplepci.hierarchy.0.run_name"),
            Some(MetadataValue::String(value)) if value == "Assay 7"
        ));
        assert!(matches!(
            metadata.get("simplepci.hierarchy.1.path"),
            Some(MetadataValue::String(value)) if value == "acquisition.channel"
        ));
        assert!(matches!(
            metadata.get("simplepci.hierarchy.1.name"),
            Some(MetadataValue::String(value)) if value == "TRITC"
        ));
        assert!(matches!(
            metadata.get("simplepci.hierarchy.1.wavelength"),
            Some(MetadataValue::Float(value)) if (*value - 561.0).abs() < f64::EPSILON
        ));
        assert!(matches!(
            metadata.get("simplepci.hierarchy.2.path"),
            Some(MetadataValue::String(value)) if value == "acquisition.channel.objective"
        ));
        assert!(matches!(
            metadata.get("simplepci.hierarchy.2.magnification"),
            Some(MetadataValue::Float(value)) if (*value - 60.0).abs() < f64::EPSILON
        ));
        assert!(matches!(
            metadata.get("simplepci.hierarchy.2.numeric_aperture"),
            Some(MetadataValue::Float(value)) if (*value - 1.4).abs() < f64::EPSILON
        ));
        assert!(matches!(
            metadata.get("simplepci.hierarchy.3.serial_number"),
            Some(MetadataValue::String(value)) if value == "CAM-17"
        ));
        assert!(matches!(
            metadata.get("simplepci.hierarchy.4.path"),
            Some(MetadataValue::String(value)) if value == "acquisition.camera.gain"
        ));
        assert!(matches!(
            metadata.get("simplepci.hierarchy.4.text"),
            Some(MetadataValue::Float(value)) if (*value - 2.5).abs() < f64::EPSILON
        ));
        assert_eq!(reader.open_bytes(0).unwrap(), vec![19]);
        assert_eq!(reader.open_bytes_region(0, 0, 0, 1, 1).unwrap(), vec![19]);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn hcs_assembly_empty_plane_ref_stays_black() {
        let meta = test_meta(3, 2);
        let mut asm = assembly_with_plane(meta, PlaneRef::default());

        let bytes = asm.open_bytes(0).unwrap();

        assert_eq!(bytes, vec![0; 6]);
    }

    #[test]
    fn hcs_assembly_missing_referenced_whole_tile_returns_error() {
        let meta = test_meta(3, 2);
        let missing = temp_path("missing_whole_tile.tif");
        let mut asm = assembly_with_plane(meta, PlaneRef::whole(missing, 0));

        let err = asm.open_bytes(0).unwrap_err();

        assert!(
            err.to_string().contains("IO error"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn hcs_assembly_unreadable_referenced_region_tile_returns_error() {
        let meta = test_meta(4, 2);
        let bad = temp_path("bad_region_tile.tif");
        std::fs::write(&bad, b"not a tiff").unwrap();
        let plane = PlaneRef {
            tiles: vec![Tile {
                filename: bad.clone(),
                file_index: 0,
                src_x: 0,
                src_y: 0,
                src_w: 2,
                src_h: 2,
                dst_x: 1,
                dst_y: 0,
            }],
        };
        let mut asm = assembly_with_plane(meta, plane);

        let err = asm.open_bytes(0).unwrap_err();

        assert!(
            err.to_string().contains("TIFF") || err.to_string().contains("Unsupported format"),
            "unexpected error: {err}"
        );
        let _ = std::fs::remove_file(bad);
    }

    #[test]
    fn hcs_assembly_referenced_region_read_error_is_not_black() {
        let tile_meta = test_meta(2, 2);
        let path = temp_path("one_plane_region_tile.tif");
        write_tiff(&path, &tile_meta, &[1, 2, 3, 4]);
        let plane = PlaneRef {
            tiles: vec![Tile {
                filename: path.clone(),
                file_index: 1,
                src_x: 0,
                src_y: 0,
                src_w: 2,
                src_h: 2,
                dst_x: 0,
                dst_y: 0,
            }],
        };
        let mut asm = assembly_with_plane(test_meta(2, 2), plane);

        let err = asm.open_bytes(0).unwrap_err();

        assert!(
            err.to_string().contains("Plane index 1 out of range"),
            "unexpected error: {err}"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn incell3000_rejects_short_decoded_plane_instead_of_padding() {
        let path = temp_path("short.frm");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&6i16.to_le_bytes()); // pixels offset
        bytes.extend_from_slice(&2i16.to_le_bytes()); // size X
        bytes.extend_from_slice(&33i16.to_le_bytes()); // one plane, one row
        bytes.extend_from_slice(&0x1234u16.to_le_bytes()); // one of two pixels
        std::fs::write(&path, bytes).unwrap();

        let mut reader = InCell3000Reader::new();
        reader.set_id(&path).unwrap();
        let err = reader.open_bytes(0).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::InvalidData(ref message) if message.contains("decoded 2 bytes")),
            "{err:?}"
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn columbus_parse_image_xml_captures_measurement_start_time_and_scalars() {
        let dir = temp_path("columbus_scalars");
        std::fs::create_dir_all(&dir).unwrap();

        // A real backing TIFF so HcsAssembly::validate accepts the well-sample.
        let tiff = dir.join("img.tif");
        let meta = test_meta(2, 2);
        write_tiff(&tiff, &meta, &[1u8, 2, 3, 4]);

        std::fs::write(
            dir.join("MeasurementIndex.ColumbusIDX.xml"),
            r#"<ColumbusMeasurementIndex><ScreenName>MyScreen</ScreenName><PlateName>MyPlate</PlateName><PlateType>96well</PlateType><PlateRows>1</PlateRows><PlateColumns>1</PlateColumns><Reference>Images.ColumbusIDX.xml</Reference></ColumbusMeasurementIndex>"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("Images.ColumbusIDX.xml"),
            r#"<Root>
<Plates><Plate><MeasurementStartTime>2020-01-02T03:04:05Z</MeasurementStartTime></Plate></Plates>
<Images><Image>
<URL BufferNo="0">img.tif</URL>
<Row>1</Row><Col>1</Col><FieldID>1</FieldID><PlaneID>1</PlaneID>
<TimepointID>1</TimepointID><ChannelID>1</ChannelID>
<ChannelName>DAPI</ChannelName>
<MainEmissionWavelength>461</MainEmissionWavelength>
<MainExcitationWavelength>358</MainExcitationWavelength>
<ImageResolutionX Unit="m">0.000001</ImageResolutionX>
<ImageResolutionY Unit="m">0.000001</ImageResolutionY>
<PositionX Unit="m">0.001</PositionX>
<PositionY Unit="m">0.002</PositionY>
<PositionZ Unit="m">0.003</PositionZ>
<MeasurementTimeOffset>1.5</MeasurementTimeOffset>
</Image></Images>
</Root>"#,
        )
        .unwrap();

        let mut reader = ColumbusReader::new();
        reader
            .set_id(&dir.join("MeasurementIndex.ColumbusIDX.xml"))
            .unwrap();
        let md = &reader.metadata().series_metadata;

        assert!(
            matches!(md.get("columbus.AcquisitionDate"),
                Some(MetadataValue::String(v)) if v == "2020-01-02T03:04:05Z"),
            "missing MeasurementStartTime: {md:?}"
        );
        assert!(matches!(
            md.get("columbus.Channel0.Name"),
            Some(MetadataValue::String(v)) if v == "DAPI"
        ));
        assert!(matches!(
            md.get("columbus.Channel0.EmissionWavelength"),
            Some(MetadataValue::Float(v)) if (*v - 461.0).abs() < 1e-9
        ));
        // PositionX 0.001 m -> 1000 um via correct_units("m").
        assert!(matches!(
            md.get("columbus.WellSamplePositionX"),
            Some(MetadataValue::Float(v)) if (*v - 1000.0).abs() < 1e-6
        ));
        // ImageResolutionX 1e-6 m -> 1.0 um.
        assert!(matches!(
            md.get("columbus.PhysicalSizeX"),
            Some(MetadataValue::Float(v)) if (*v - 1.0).abs() < 1e-9
        ));
        assert!(matches!(
            md.get("columbus.PlaneDeltaT"),
            Some(MetadataValue::Float(v)) if (*v - 1.5).abs() < 1e-9
        ));

        // MeasurementHandler.endElement addGlobalMeta(currentName, value):
        // every element of the measurement index becomes a global key.
        assert!(matches!(
            md.get("ScreenName"),
            Some(MetadataValue::String(v)) if v == "MyScreen"
        ));
        assert!(matches!(
            md.get("PlateName"),
            Some(MetadataValue::String(v)) if v == "MyPlate"
        ));
        assert!(matches!(
            md.get("PlateType"),
            Some(MetadataValue::String(v)) if v == "96well"
        ));

        let _ = std::fs::remove_dir_all(dir);
    }

    /// `parse_measurement_index` captures every element as a global-meta entry
    /// (mirrors ColumbusReader.MeasurementHandler.endElement addGlobalMeta).
    #[test]
    fn columbus_measurement_index_captures_global_meta() {
        let info = columbus::parse_measurement_index(
            r#"<Root><ScreenName>S</ScreenName><PlateName>P</PlateName><PlateType>T</PlateType><PlateRows>2</PlateRows><PlateColumns>3</PlateColumns></Root>"#,
        );
        assert_eq!(info.plate_rows, 2);
        assert_eq!(info.plate_cols, 3);
        let has = |k: &str, want: &str| {
            info.global
                .iter()
                .any(|(key, v)| key == k && matches!(v, MetadataValue::String(s) if s == want))
        };
        assert!(has("ScreenName", "S"));
        assert!(has("PlateName", "P"));
        assert!(has("PlateType", "T"));
        assert!(has("PlateRows", "2"));
    }

    /// Build a single-IFD TIFF carrying a COPYRIGHT tag plus an
    /// ImageDescription comment, for exercising TrestleReader detection +
    /// `initStandardMetadata` comment parsing.
    fn write_tiff_with_copyright_and_comment(path: &Path, copyright: &str, comment: &str) {
        let mut cr = copyright.as_bytes().to_vec();
        cr.push(0);
        let mut desc = comment.as_bytes().to_vec();
        desc.push(0);

        let entries = [
            tiff_entry(256, 4, 1, 1),
            tiff_entry(257, 4, 1, 1),
            tiff_entry(258, 3, 1, 8),
            tiff_entry(259, 3, 1, 1),
            tiff_entry(262, 3, 1, 1),
            tiff_entry(270, 2, desc.len() as u32, 0),    // ImageDescription
            tiff_entry(273, 4, 1, 0),                    // StripOffsets
            tiff_entry(277, 3, 1, 1),
            tiff_entry(278, 4, 1, 1),
            tiff_entry(279, 4, 1, 1),
            tiff_entry(284, 3, 1, 1),
            tiff_entry(33432, 2, cr.len() as u32, 0),    // Copyright
        ];
        let ifd_start = 8u32;
        let cr_start = ifd_start + 2 + (entries.len() as u32) * 12 + 4;
        let desc_start = cr_start + cr.len() as u32;
        let pixel_start = desc_start + desc.len() as u32;

        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"II");
        bytes.extend_from_slice(&42u16.to_le_bytes());
        bytes.extend_from_slice(&ifd_start.to_le_bytes());
        bytes.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        for mut entry in entries {
            match u16::from_le_bytes([entry[0], entry[1]]) {
                270 => entry[8..12].copy_from_slice(&desc_start.to_le_bytes()),
                273 => entry[8..12].copy_from_slice(&pixel_start.to_le_bytes()),
                33432 => entry[8..12].copy_from_slice(&cr_start.to_le_bytes()),
                _ => {}
            }
            bytes.extend_from_slice(&entry);
        }
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&cr);
        bytes.extend_from_slice(&desc);
        bytes.push(42);
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn trestle_parses_comment_keyvalues_when_copyright_matches() {
        let path = temp_path("trestle.tif");
        write_tiff_with_copyright_and_comment(
            &path,
            "Copyright Trestle Corp.",
            "OverlapsXY=0 0 ; Objective=20x ;Scanner=MedScan",
        );

        let mut reader = TrestleReader::new();
        reader.set_id(&path).unwrap();
        let md = &reader.metadata().series_metadata;

        assert!(matches!(
            md.get("hcs2.wrapper"),
            Some(MetadataValue::String(v)) if v == "TrestleReader"
        ));
        // addGlobalMeta(key, value) for each `;`-separated `key=value`.
        assert!(matches!(
            md.get("OverlapsXY"),
            Some(MetadataValue::String(v)) if v == "0 0"
        ));
        assert!(matches!(
            md.get("Objective"),
            Some(MetadataValue::String(v)) if v == "20x"
        ));
        assert!(matches!(
            md.get("Scanner"),
            Some(MetadataValue::String(v)) if v == "MedScan"
        ));
        assert_eq!(reader.open_bytes(0).unwrap(), vec![42]);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn trestle_ignores_comment_without_trestle_copyright() {
        let path = temp_path("trestle_plain.tif");
        write_tiff_with_copyright_and_comment(
            &path,
            "Some Other Vendor",
            "Objective=20x;Scanner=MedScan",
        );

        let mut reader = TrestleReader::new();
        reader.set_id(&path).unwrap();
        let md = &reader.metadata().series_metadata;

        assert!(md.contains_key("hcs2.wrapper"));
        assert!(
            !md.contains_key("Objective") && !md.contains_key("Scanner"),
            "non-Trestle copyright must not capture comment scalars: {md:?}"
        );

        let _ = std::fs::remove_file(path);
    }

    /// Build a minimal ScanR dataset (experiment_descriptor.xml + one data TIFF)
    /// and confirm the newly-captured ScanrHandler fields surface into
    /// series_metadata: plate name, physical pixel size, channel name,
    /// per-channel exposure time (ms -> s) and timeloop deltaT (ms -> s).
    #[test]
    fn scanr_surfaces_plate_channel_exposure_and_deltat() {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("bioformats_scanr_{id}"));
        let data = dir.join("data");
        std::fs::create_dir_all(&data).unwrap();

        // One channel "DAPI", one well "A1" (number 1), pixel size + plate name +
        // exposure (1500 ms) + timeloop delay (2000 ms -> 2.0 s deltaT).
        let xml = r#"<?xml version="1.0" encoding="ISO-8859-1"?>
<root>
  <Name>plate name</Name><Val>MyPlate</Val>
  <Name>conversion factor um/pixel</Name><Val>0.65</Val>
  <Name>timeloop delay [ms]</Name><Val>2000</Val>
  <Cluster>
    <Name>name</Name><Val>DAPI</Val>
    <Name>exposure time</Name><Val>1500</Val>
    <Name>idle</Name><Val>0</Val>
  </Cluster>
  <Name>well selection table + cDNA</Name><Val>1</Val>
  <Name>well selection table + cDNA</Name><Val>A1</Val>
</root>"#;
        let xml_path = dir.join("experiment_descriptor.xml");
        std::fs::write(&xml_path, xml).unwrap();

        // data TIFF whose name carries the W/P/Z/T blocks and channel name.
        let tiff = data.join("--W00001--P00001--Z00000--T00000--DAPI.tif");
        let meta = test_meta(4, 4);
        write_tiff(&tiff, &meta, &vec![0u8; 16]);

        let mut reader = ScanrReader::new();
        reader.set_id(&xml_path).unwrap();
        let md = &reader.metadata().series_metadata;

        assert!(
            matches!(md.get("Plate name"), Some(MetadataValue::String(v)) if v == "MyPlate"),
            "plate name not captured: {md:?}"
        );
        assert!(
            matches!(md.get("PhysicalSizeX"), Some(MetadataValue::Float(v)) if (*v - 0.65).abs() < 1e-9),
            "pixel size not captured: {md:?}"
        );
        assert!(
            matches!(md.get("Channel 0 Name"), Some(MetadataValue::String(v)) if v == "DAPI"),
            "channel name not captured: {md:?}"
        );
        assert!(
            matches!(md.get("Channel 0 ExposureTime"), Some(MetadataValue::Float(v)) if (*v - 1.5).abs() < 1e-9),
            "exposure time (s) not captured: {md:?}"
        );
        assert!(
            matches!(md.get("PlaneDeltaT"), Some(MetadataValue::Float(v)) if (*v - 2.0).abs() < 1e-9),
            "deltaT not captured: {md:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Confirm ScanR subposition-list field positions surface as
    /// WellSamplePositionX/Y / PlanePositionX/Y (Java foundPositions branch).
    #[test]
    fn scanr_surfaces_field_positions() {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("bioformats_scanr_{id}"));
        let data = dir.join("data");
        std::fs::create_dir_all(&data).unwrap();

        // subposition list with one position: X=10.0 then Y=20.0 (paired).
        let xml = r#"<?xml version="1.0" encoding="ISO-8859-1"?>
<root>
  <Cluster>
    <Name>name</Name><Val>DAPI</Val>
    <Name>idle</Name><Val>0</Val>
  </Cluster>
  <Name>subposition list</Name>
  <Dimsize>1</Dimsize>
  <Val>10.0</Val>
  <Val>20.0</Val>
  <Name>well selection table + cDNA</Name><Val>1</Val>
  <Name>well selection table + cDNA</Name><Val>A1</Val>
</root>"#;
        let xml_path = dir.join("experiment_descriptor.xml");
        std::fs::write(&xml_path, xml).unwrap();

        let tiff = data.join("--W00001--P00001--Z00000--T00000--DAPI.tif");
        let meta = test_meta(4, 4);
        write_tiff(&tiff, &meta, &vec![0u8; 16]);

        let mut reader = ScanrReader::new();
        reader.set_id(&xml_path).unwrap();
        let md = &reader.metadata().series_metadata;

        assert!(
            matches!(md.get("WellSamplePositionX"), Some(MetadataValue::Float(v)) if (*v - 10.0).abs() < 1e-9),
            "field position X not captured: {md:?}"
        );
        assert!(
            matches!(md.get("WellSamplePositionY"), Some(MetadataValue::Float(v)) if (*v - 20.0).abs() < 1e-9),
            "field position Y not captured: {md:?}"
        );
        assert!(
            matches!(md.get("PlanePositionX"), Some(MetadataValue::Float(v)) if (*v - 10.0).abs() < 1e-9),
            "plane position X not captured: {md:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// MetaXpress nested `TimePoint_<t>/ZStep_<z>/` layout must produce the full
    /// well x field x T x Z series grid (via the CellWorxReader hook), not a flat
    /// single plane series, and route each plane to the correct subdir TIFF.
    #[test]
    fn metaxpress_subdir_layout_builds_well_grid() {
        let dir = temp_path("metaxpress_subdir");
        std::fs::create_dir_all(&dir).unwrap();

        // 1x1 well plate, single field, 2 timepoints x 2 z-steps, 1 wavelength.
        let htd = dir.join("Plate.HTD");
        std::fs::write(
            &htd,
            "\"XWells\", 1\n\
             \"YWells\", 1\n\
             \"WellsSelection1\", true\n\
             \"XSites\", 1\n\
             \"YSites\", 1\n\
             \"TimePoints\", 2\n\
             \"ZSteps\", 2\n\
             \"NWavelengths\", 1\n\
             \"WaveName1\", \"DAPI\"\n",
        )
        .unwrap();

        // Walk order is TimePoint_1/ZStep_1, .._2, TimePoint_2/ZStep_1, .._2.
        // The flat naming finds nothing on disk, so the subdir fallback drives
        // the hook. Per-plane pixel values let us check the ZCT routing.
        let meta = test_meta(1, 1);
        let plane_values: [(u32, u32, u8); 4] = [(1, 1, 10), (1, 2, 20), (2, 1, 30), (2, 2, 40)];
        for (t, z, value) in plane_values {
            let zdir = dir.join(format!("TimePoint_{t}")).join(format!("ZStep_{z}"));
            std::fs::create_dir_all(&zdir).unwrap();
            // Filename must start with the well prefix "Plate_A01".
            let tiff = zdir.join(format!("Plate_A01_w1_t{t}_z{z}.tif"));
            write_tiff(&tiff, &meta, &[value]);
        }

        let mut reader = MetaxpressTiffReader::new();
        reader.set_id(&htd).expect("subdir layout opens");

        // Full grid: one well x one field = one series, but with Z=2, T=2.
        assert_eq!(reader.series_count(), 1, "expected single well x field series");
        let m = reader.metadata();
        assert_eq!(m.size_z, 2, "Z grid from HTD ZSteps");
        assert_eq!(m.size_t, 2, "T grid from HTD TimePoints");
        assert_eq!(m.size_c, 1);
        assert_eq!(m.image_count, 4, "Z*C*T planes");

        // Each plane routes to its subdir TIFF via getFile ZCT indexing.
        // Plane p under XYCZT (C=1): c=0, z=p%2, t=p/2 -> files[z + 2t].
        let expected = [10u8, 20u8, 30u8, 40u8];
        for (p, &want) in expected.iter().enumerate() {
            let bytes = reader.open_bytes(p as u32).expect("plane reads");
            assert_eq!(bytes.first().copied(), Some(want), "plane {p} routed wrong");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}
