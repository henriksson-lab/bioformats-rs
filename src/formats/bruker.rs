//! Bruker reader — a faithful Rust port of the Java Bio-Formats `BrukerReader`
//! (`loci.formats.in.BrukerReader`).
//!
//! Despite its name being commonly associated with Bruker/SkyScan microCT, the
//! upstream Java `BrukerReader` reads **Bruker MRI** datasets (ParaVision
//! layout). A dataset is a directory tree of the form:
//!
//! ```text
//! <dataset-root>/
//!   <N>/                 (one acquisition per numbered directory)
//!     acqp               (acquisition parameters, "##$KEY=VALUE" text)
//!     fid                (raw FID; used only for type detection)
//!     pdata/
//!       1/
//!         2dseq          (reconstructed raw pixel data — read directly)
//!         reco           (reconstruction parameters, "##$KEY=VALUE" text)
//!         d3proc         (3D processing parameters, "##$KEY=VALUE" text)
//! ```
//!
//! Detection keys off the companion `fid`/`acqp` filenames (Java
//! `suffixSufficient = false`; the file *name* must equal `fid` or `acqp`).
//! `initFile` walks two directory levels up from the opened file, enumerates the
//! numbered acquisition directories, and for each `pdata/1/2dseq` builds one
//! series. The `acqp`, `reco` and optional `d3proc` text files are parsed for
//! `##$`-prefixed keys to derive dimensions, byte order and pixel type.
//!
//! Pixel data is **raw** in the `2dseq` file: a plane is `sizeX * sizeY *
//! bytesPerPixel` contiguous bytes (sizeC == 1, not RGB), so `open_bytes` seeks
//! to `plane * planeSize` and reads the bytes directly. No TIFF / image-reader
//! delegation is involved (unlike the prompt's description — this matches the
//! actual Java source).

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::{uninitialized_metadata, FormatReader};

/// `FormatTools.pixelTypeFromBytes` — map (bytesPerPixel, signed, floating) to a
/// pixel type. Mirrors the Java helper used by `BrukerReader.initFile`.
fn pixel_type_from_bytes(bytes: i64, signed: bool, floating: bool) -> PixelType {
    match bytes {
        1 => {
            if signed {
                PixelType::Int8
            } else {
                PixelType::Uint8
            }
        }
        2 => {
            if signed {
                PixelType::Int16
            } else {
                PixelType::Uint16
            }
        }
        4 => {
            if floating {
                PixelType::Float32
            } else if signed {
                PixelType::Int32
            } else {
                PixelType::Uint32
            }
        }
        8 => PixelType::Float64,
        // Java would throw; we fall back to the smallest sane type.
        _ => PixelType::Uint8,
    }
}

fn parse_i64(s: &str) -> i64 {
    s.trim().parse::<i64>().unwrap_or(0)
}

/// Mutable per-series parse state, accumulated across `acqp`, `reco` and
/// `d3proc`. Mirrors the loose collection of fields on the Java reader plus the
/// `CoreMetadata` it mutates while parsing.
#[derive(Default)]
struct SeriesParse {
    ni: i64,
    nr: i64,
    ns: i64,
    bits: i64,
    signed: bool,
    is_float: bool,
    sizes: Option<Vec<String>>,
    #[allow(dead_code)]
    ordering: Option<Vec<String>>,
    // CoreMetadata fields touched during parsing.
    little_endian: bool, // Java CoreMetadata default is big-endian (false).
    size_x: i64,
    size_y: i64,
    size_z: i64,
    size_t: i64,
    // Companion strings surfaced to OME / metadata.
    image_name: Option<String>,
    timestamp: Option<String>,
    institution: Option<String>,
    user: Option<String>,
    // Original "##$KEY" metadata (keys stripped of the leading "##$").
    meta: std::collections::HashMap<String, MetadataValue>,
}

impl SeriesParse {
    /// Port of `BrukerReader.parseLines`.
    fn parse_lines(&mut self, data: &str) {
        let lines: Vec<&str> = data.split('\n').map(|l| l.trim_end_matches('\r')).collect();
        for i in 0..lines.len() {
            let line = lines[i];
            let index = match line.find('=') {
                Some(idx) => idx,
                None => continue,
            };
            let key = &line[..index];
            let mut value = line[index + 1..].to_string();

            // A value of the form "( ... )" means the real value is on the next
            // line; "<...>" wrappers are stripped.
            if value.starts_with('(') {
                if let Some(next) = lines.get(i + 1) {
                    value = next.trim().to_string();
                    if value.starts_with('<') && value.len() >= 2 {
                        value = value[1..value.len() - 1].to_string();
                    }
                }
            }

            if key.len() < 4 {
                continue;
            }

            // addSeriesMeta(key.substring(3), value)
            self.meta
                .insert(key[3..].to_string(), MetadataValue::String(value.clone()));

            match key {
                "##$NI" => self.ni = parse_i64(&value),
                "##$NR" => self.nr = parse_i64(&value),
                "##$ACQ_word_size" => {
                    // bits = parseInt(value.substring(1, value.lastIndexOf("_")))
                    if let Some(end) = value.rfind('_') {
                        if end >= 1 {
                            self.bits = parse_i64(&value[1..end]);
                        }
                    }
                }
                "##$BYTORDA" => {
                    self.little_endian = value.trim().eq_ignore_ascii_case("little");
                }
                "##$ACQ_size" => {
                    self.sizes = Some(value.split(' ').map(|s| s.to_string()).collect());
                }
                "##$ACQ_obj_order" => {
                    self.ordering = Some(value.split(' ').map(|s| s.to_string()).collect());
                }
                "##$ACQ_time" => self.timestamp = Some(value.clone()),
                "##$ACQ_institution" => self.institution = Some(value.clone()),
                "##$ACQ_operator" => self.user = Some(value.clone()),
                "##$ACQ_scan_name" => self.image_name = Some(value.clone()),
                "##$ACQ_ns_list_size" => self.ns = parse_i64(&value),
                "##$RECO_size" => {
                    self.sizes = Some(value.split(' ').map(|s| s.to_string()).collect());
                }
                "##$RECO_wordtype" => {
                    // bits = parseInt(value.substring(1, value.indexOf("BIT")))
                    if let Some(idx) = value.find("BIT") {
                        if idx >= 1 {
                            self.bits = parse_i64(&value[1..idx]);
                        }
                    }
                    self.signed = value.contains("_SGN_");
                    self.is_float = !value.trim_end().ends_with("_INT");
                }
                "##$IM_SIX" => self.size_x = parse_i64(&value),
                "##$IM_SIY" => self.size_y = parse_i64(&value),
                "##$IM_SIZ" => self.size_z = parse_i64(&value),
                "##$IM_SIT" => self.size_t = parse_i64(&value),
                _ => {}
            }
        }
    }
}

/// Bruker MRI reader (`acqp` / `2dseq` ParaVision datasets).
pub struct BrukerReader {
    /// Absolute path to each series' `2dseq` pixel file (one per series).
    pixels_files: Vec<PathBuf>,
    /// Per-series core metadata.
    metas: Vec<ImageMetadata>,
    /// Per-series image name (from `##$ACQ_scan_name`).
    image_names: Vec<Option<String>>,
    /// Per-series acquisition institution (from `##$ACQ_institution`).
    /// Java: `String[] institutions` -> `setExperimenterInstitution`.
    institutions: Vec<Option<String>>,
    /// Per-series operator/user (from `##$ACQ_operator`).
    /// Java: `String[] users` -> `setExperimenterLastName`.
    users: Vec<Option<String>>,
    /// Per-series acquisition time string (from `##$ACQ_time`).
    /// Java: `String[] timestamps` -> `setImageAcquisitionDate`.
    timestamps: Vec<Option<String>>,
    current: usize,
}

impl BrukerReader {
    pub fn new() -> Self {
        BrukerReader {
            pixels_files: Vec::new(),
            metas: Vec::new(),
            image_names: Vec::new(),
            institutions: Vec::new(),
            users: Vec::new(),
            timestamps: Vec::new(),
            current: 0,
        }
    }

    /// Number of bytes in one full plane of the given series.
    fn plane_size(meta: &ImageMetadata) -> usize {
        meta.size_x as usize
            * meta.size_y as usize
            * meta.pixel_type.bytes_per_sample()
            * meta.size_c.max(1) as usize
    }

    /// Read a sub-rectangle of one plane directly from the `2dseq` file.
    /// Mirrors `openBytes` + `readPlane` (sizeC == 1, non-interleaved, so the
    /// layout is plain row-major `bpp`-byte samples).
    fn read_region(
        &self,
        series: usize,
        no: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let meta = self
            .metas
            .get(series)
            .ok_or(BioFormatsError::SeriesOutOfRange(series))?;
        let path = self
            .pixels_files
            .get(series)
            .ok_or(BioFormatsError::SeriesOutOfRange(series))?;

        let bpp = meta.pixel_type.bytes_per_sample();
        let sx = meta.size_x as usize;
        let row_bytes = w as usize * bpp;
        let mut buf = vec![0u8; h as usize * row_bytes];

        let full_plane = Self::plane_size(meta);
        let base = no as usize * full_plane;

        let mut file = fs::File::open(path)?;
        let len = file.metadata()?.len() as usize;

        for row in 0..h as usize {
            let src = base + ((y as usize + row) * sx + x as usize) * bpp;
            if src >= len {
                break;
            }
            let avail = (len - src).min(row_bytes);
            file.seek(SeekFrom::Start(src as u64))?;
            let dst = &mut buf[row * row_bytes..row * row_bytes + avail];
            file.read_exact(dst)?;
        }
        Ok(buf)
    }

    /// Port of `BrukerReader.initFile`: discover the acquisition directories and
    /// build one series per `pdata/1/2dseq`.
    fn discover(&mut self, id: &Path) -> Result<()> {
        let original = abs(id);
        // parent = originalFile.getParentFile().getParentFile()
        let parent = original
            .parent()
            .and_then(|p| p.parent())
            .ok_or_else(|| BioFormatsError::Format("Bruker: file has no grandparent dir".into()))?
            .to_path_buf();

        // List the acquisition directory names and sort them numerically.
        let mut acquisition_dirs: Vec<String> = match fs::read_dir(&parent) {
            Ok(rd) => rd
                .filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().into_string().ok())
                .collect(),
            Err(_) => Vec::new(),
        };
        // Stable sort by numeric value (non-numeric names sort as 0), mirroring
        // the Java Comparator (NumberFormatException -> 0).
        acquisition_dirs.sort_by_key(|n| n.parse::<i64>().unwrap_or(0));

        let mut acqp_files: Vec<PathBuf> = Vec::new();
        let mut reco_files: Vec<PathBuf> = Vec::new();
        let mut proc_files: Vec<PathBuf> = Vec::new();

        for f in &acquisition_dirs {
            let dir = parent.join(f);
            if !dir.is_dir() {
                continue;
            }
            let entries = match fs::read_dir(&dir) {
                Ok(rd) => rd,
                Err(_) => continue,
            };
            for entry in entries.filter_map(|e| e.ok()) {
                let file_name = match entry.file_name().into_string() {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let child = dir.join(&file_name);
                if !child.is_dir() {
                    if file_name == "acqp" {
                        acqp_files.push(child.clone());
                    }
                } else {
                    let grandchild = child.join("1");
                    if grandchild.exists() {
                        if let Ok(more) = fs::read_dir(&grandchild) {
                            for m in more.filter_map(|e| e.ok()) {
                                let mname = match m.file_name().into_string() {
                                    Ok(s) => s,
                                    Err(_) => continue,
                                };
                                let ggc = grandchild.join(&mname);
                                if ggc.is_dir() {
                                    continue;
                                }
                                match mname.as_str() {
                                    "2dseq" => self.pixels_files.push(ggc),
                                    "reco" => reco_files.push(ggc),
                                    "d3proc" => proc_files.push(ggc),
                                    _ => {}
                                }
                            }
                        }
                    }
                }
            }

            // Keep the companion lists aligned with pixelsFiles per directory.
            if acqp_files.len() > self.pixels_files.len() {
                acqp_files.pop();
            }
            if reco_files.len() > self.pixels_files.len() {
                reco_files.pop();
            }
            if proc_files.len() > self.pixels_files.len() {
                proc_files.pop();
            }
        }

        let series_count = self.pixels_files.len();
        // Java allocates these per-series arrays up front in initFile.
        self.image_names = vec![None; series_count];
        self.institutions = vec![None; series_count];
        self.users = vec![None; series_count];
        self.timestamps = vec![None; series_count];

        for series in 0..series_count {
            let mut p = SeriesParse::default();

            // acqp (required)
            let acq_path = acqp_files.get(series).ok_or_else(|| {
                BioFormatsError::Format(format!("Bruker: missing acqp for series {series}"))
            })?;
            p.parse_lines(&read_text(acq_path)?);

            // reco (required)
            let reco_path = reco_files.get(series).ok_or_else(|| {
                BioFormatsError::Format(format!("Bruker: missing reco for series {series}"))
            })?;
            p.parse_lines(&read_text(reco_path)?);

            // d3proc (optional)
            let parsed_proc_file = if series < proc_files.len() {
                p.parse_lines(&read_text(&proc_files[series])?);
                true
            } else {
                false
            };

            let meta = build_metadata(&mut p, parsed_proc_file)?;
            self.image_names[series] = p.image_name.clone();
            self.institutions[series] = p.institution.clone();
            self.users[series] = p.user.clone();
            self.timestamps[series] = p.timestamp.clone();
            self.metas.push(meta);
        }

        Ok(())
    }
}

/// Apply `BrukerReader.initFile`'s dimension logic and produce the core
/// metadata for a single series.
fn build_metadata(p: &mut SeriesParse, parsed_proc_file: bool) -> Result<ImageMetadata> {
    let bytes = p.bits / 8;
    let pixel_type = pixel_type_from_bytes(bytes, p.signed, p.is_float);

    // Reset the dimensions if the d3proc data does not match the pixel file
    // size; otherwise discard the IM_* dims and recompute from sizes/ni/nr/ns.
    if parsed_proc_file && p.size_z * p.size_t != p.nr * p.ni && (p.ni > 1 || p.nr > 1 || p.ns > 1)
    {
        p.ni = 1;
        p.nr = 1;
        p.ns = 1;
    } else {
        p.size_x = 0;
        p.size_y = 0;
        p.size_z = 0;
        p.size_t = 0;
    }

    let sizes = p
        .sizes
        .as_ref()
        .ok_or_else(|| BioFormatsError::Format("Bruker: missing ACQ_size/RECO_size".into()))?;
    let td = sizes.first().map(|s| parse_i64(s)).unwrap_or(0);
    let ys = if sizes.len() > 1 {
        parse_i64(&sizes[1])
    } else {
        0
    };
    let zs = if sizes.len() > 2 {
        parse_i64(&sizes[2])
    } else {
        0
    };

    if p.size_y == 0 || p.size_z == 0 {
        if sizes.len() == 2 {
            if p.ni == 1 {
                p.size_y = ys;
                p.size_z = p.nr;
            } else {
                p.size_y = ys;
                p.size_z = p.ni;
            }
        } else if sizes.len() == 3 {
            p.size_y = p.ni * ys;
            p.size_z = p.nr * zs;
        }
    }
    if p.size_x == 0 {
        p.size_x = td;
    }

    if p.size_t == 0 {
        // Java does integer division by ns here (throws if ns == 0). We guard
        // against a divide-by-zero panic by treating ns == 0 as 1.
        let ns = if p.ns == 0 { 1 } else { p.ns };
        p.size_z /= ns;
        p.size_t = p.ns * p.nr;
    }

    let size_x = p.size_x.max(0) as u32;
    let size_y = p.size_y.max(0) as u32;
    let size_z = p.size_z.max(0) as u32;
    let size_t = p.size_t.max(0) as u32;
    let size_c = 1u32;

    let mut meta = ImageMetadata {
        size_x,
        size_y,
        size_z,
        size_c,
        size_t,
        pixel_type,
        bits_per_pixel: (pixel_type.bytes_per_sample() * 8) as u8,
        image_count: size_z * size_c * size_t,
        dimension_order: DimensionOrder::XYCTZ,
        is_rgb: false,
        is_interleaved: false,
        is_indexed: false,
        is_little_endian: p.little_endian,
        resolution_count: 1,
        ..Default::default()
    };
    meta.series_metadata = std::mem::take(&mut p.meta);
    Ok(meta)
}

/// Read a metadata text file as a (lossy-UTF-8) string.
fn read_text(path: &Path) -> Result<String> {
    let bytes = fs::read(path)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Best-effort absolute path (Java `getAbsoluteFile`, not canonicalisation).
fn abs(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|d| d.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

impl Default for BrukerReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for BrukerReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        // suffixSufficient = false: the file *name* must be exactly fid or acqp.
        matches!(
            path.file_name().and_then(|n| n.to_str()),
            Some("fid") | Some("acqp")
        )
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        // Java isThisType(RandomAccessInputStream) returns false.
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        self.discover(path)?;
        self.current = 0;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.pixels_files.clear();
        self.metas.clear();
        self.image_names.clear();
        self.institutions.clear();
        self.users.clear();
        self.timestamps.clear();
        self.current = 0;
        Ok(())
    }

    fn series_count(&self) -> usize {
        self.pixels_files.len()
    }

    fn set_series(&mut self, series: usize) -> Result<()> {
        if series >= self.pixels_files.len() {
            return Err(BioFormatsError::SeriesOutOfRange(series));
        }
        self.current = series;
        Ok(())
    }

    fn series(&self) -> usize {
        self.current
    }

    fn metadata(&self) -> &ImageMetadata {
        self.metas
            .get(self.current)
            .unwrap_or_else(|| uninitialized_metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self
            .metas
            .get(self.current)
            .ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let (w, h) = (meta.size_x, meta.size_y);
        self.read_region(self.current, plane_index, 0, 0, w, h)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let meta = self
            .metas
            .get(self.current)
            .ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        crate::common::region::validate_region("Bruker", meta.size_x, meta.size_y, x, y, w, h)?;
        self.read_region(self.current, plane_index, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        // No dedicated thumbnail; return the full plane (callers downsample).
        self.open_bytes(plane_index)
    }

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        if self.metas.is_empty() {
            return None;
        }
        use crate::common::ome_metadata::{OmeExperimenter, OmeImage, OmeMetadata};
        // Mirror Java initFile's per-series MetadataStore loop:
        //   setImageName, setImageAcquisitionDate, setExperimenterLastName,
        //   setExperimenterInstitution.
        let images = self
            .image_names
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let base = name.clone().unwrap_or_default();
                let acquisition_date = self
                    .timestamps
                    .get(i)
                    .and_then(|t| t.as_deref())
                    .and_then(format_bruker_date);
                OmeImage {
                    name: Some(format!("{} #{}", base, i + 1)),
                    acquisition_date,
                    ..Default::default()
                }
            })
            .collect();
        let experimenters = (0..self.image_names.len())
            .map(|i| OmeExperimenter {
                id: Some(format!("Experimenter:{i}")),
                last_name: self.users.get(i).and_then(|u| u.clone()),
                institution: self.institutions.get(i).and_then(|n| n.clone()),
                ..Default::default()
            })
            .collect();
        Some(OmeMetadata {
            images,
            experimenters,
            ..Default::default()
        })
    }
}

// ===========================================================================
// MicroCTReader — faithful Rust port of the Java Bio-Formats `MicroCTReader`
// (`loci.formats.in.MicroCTReader`).
//
// Despite the "Bruker MicroCT" / SkyScan association in the project TODO, the
// upstream Java `MicroCTReader` is the reader for **GE MicroCT VFF** datasets:
// a directory holding one `.vff` file per Z slice (grouped via a FilePattern)
// plus several companion metadata files (`.log`, `.protocol`, `Parameters.txt`,
// `Description.txt`, and assorted single-value files). Each `.vff` begins with
// the ASCII magic `ncaa`, followed by an LF-terminated text header of
// `key=value;` lines (terminated by a `0x0c 0x0a` line), then raw big-endian
// pixel data stored with the origin in the **lower-left** corner.
//
// This port lives alongside the Bruker MRI reader because both are Bruker /
// medical-domain microscopy companion-file readers; it is a separate public
// type (`MicroCtVffReader`) and does not collide with the unrelated `.ctf`
// TIFF-delegating `MicroCtReader` in `formats::flim2`.

/// `EEE, MMM dd, yyyy HH:mm:ss a` — the Java `MicroCTReader.DATE_FORMAT`.
/// Mirrored as a named constant for parity; the parse in `format_microct_date`
/// implements exactly this pattern.
#[allow(dead_code)]
const MICROCT_DATE_FORMAT: &str = "EEE, MMM dd, yyyy HH:mm:ss a";

/// VFF header magic (`MicroCTReader.VFF_MAGIC`).
const VFF_MAGIC: &[u8] = b"ncaa";

/// `DataTools.parseDouble` — locale-tolerant double parse (returns `None` like
/// the Java helper does for unparseable input).
fn parse_double(value: &str) -> Option<f64> {
    value.trim().replace(',', ".").parse::<f64>().ok()
}

/// Port of `DateTools.formatDate(date + " " + time, DATE_FORMAT)` for the
/// MicroCT `EEE, MMM dd, yyyy HH:mm:ss a` pattern, e.g.
/// `"Mon, Jan 05, 2015 03:14:00 PM"` → `"2015-01-05T15:14:00"`. Returns `None`
/// if unparseable (Java returns null and the date is then skipped).
fn format_microct_date(date: &str, time: &str) -> Option<String> {
    let combined = format!("{date} {time}");
    // Tokens: <weekday,> <month> <day,> <year> <HH:mm:ss> <AM|PM>
    let tokens: Vec<&str> = combined.split_whitespace().collect();
    if tokens.len() != 6 {
        return None;
    }
    let month = match tokens[1] {
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
    let day: u32 = tokens[2].trim_end_matches(',').parse().ok()?;
    let year: i32 = tokens[3].parse().ok()?;
    let time_parts: Vec<&str> = tokens[4].split(':').collect();
    if time_parts.len() != 3 {
        return None;
    }
    let mut hour: u32 = time_parts[0].parse().ok()?;
    let minute: u32 = time_parts[1].parse().ok()?;
    let second: u32 = time_parts[2].parse().ok()?;
    match tokens[5].to_ascii_uppercase().as_str() {
        "PM" => {
            if hour != 12 {
                hour += 12;
            }
        }
        "AM" => {
            if hour == 12 {
                hour = 0;
            }
        }
        _ => return None,
    }
    Some(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}"
    ))
}

/// Convert a Bruker `##$ACQ_time` string into an OME ISO-8601 timestamp.
///
/// Java `BrukerReader.DATE_FORMAT = "HH:mm:ss  d MMM yyyy"`, e.g.
/// `"15:14:00  5 Jan 2015"` -> `"2015-01-05T15:14:00"`. Returns `None` when the
/// string does not match (mirrors `DateTools.formatDate` returning null).
fn format_bruker_date(raw: &str) -> Option<String> {
    // Tokens: <HH:mm:ss> <day> <month> <year>. Collapsing whitespace handles the
    // double-space the Java pattern uses between seconds and the day.
    let tokens: Vec<&str> = raw.split_whitespace().collect();
    if tokens.len() != 4 {
        return None;
    }
    let time_parts: Vec<&str> = tokens[0].split(':').collect();
    if time_parts.len() != 3 {
        return None;
    }
    let hour: u32 = time_parts[0].parse().ok()?;
    let minute: u32 = time_parts[1].parse().ok()?;
    let second: u32 = time_parts[2].parse().ok()?;
    let day: u32 = tokens[1].parse().ok()?;
    let month = match tokens[2] {
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
    let year: i32 = tokens[3].parse().ok()?;
    Some(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}"
    ))
}

/// GE MicroCT VFF reader (`.vff` datasets). Faithful port of Java
/// `loci.formats.in.MicroCTReader`.
pub struct MicroCtVffReader {
    /// One `.vff` file per Z slice, in FilePattern order (Java `vffs`).
    vffs: Vec<PathBuf>,
    /// Cached header size per VFF, computed lazily (Java `headerSize`).
    header_size: Vec<u64>,
    /// Non-`.vff` companion metadata files in the parent dir (Java `metadataFiles`).
    metadata_files: Vec<PathBuf>,
    /// Parsed core metadata (single series).
    meta: ImageMetadata,
    /// `Date` companion key (Java `date`).
    date: Option<String>,
    /// `Time` companion key (Java `time`).
    time: Option<String>,
    /// `Description.txt` companion key (Java `imageDescription`).
    image_description: Option<String>,
    /// `Exposure Time (ms)` companion key, in seconds (Java `exposureTime`).
    exposure_time: Option<f64>,
    /// Physical pixel size in micrometres (Java `physicalSize`).
    physical_size: Option<f64>,
    initialized: bool,
}

impl MicroCtVffReader {
    pub fn new() -> Self {
        MicroCtVffReader {
            vffs: Vec::new(),
            header_size: Vec::new(),
            metadata_files: Vec::new(),
            meta: ImageMetadata::default(),
            date: None,
            time: None,
            image_description: None,
            exposure_time: None,
            physical_size: None,
            initialized: false,
        }
    }

    /// Port of `MicroCTReader.processKey`: stash into the original metadata
    /// table and, for the recognised keys, the appropriate field.
    fn process_key(&mut self, key: &str, value: &str) {
        // addGlobalMeta(key, value)
        self.meta
            .series_metadata
            .insert(key.to_string(), MetadataValue::String(value.to_string()));

        match key {
            "Exposure Time (ms)" => {
                self.exposure_time = parse_double(value).map(|v| v / 1000.0);
            }
            "Description.txt" => self.image_description = Some(value.to_string()),
            "Date" => self.date = Some(value.to_string()),
            "Time" => self.time = Some(value.to_string()),
            _ => {}
        }
    }

    /// Port of `MicroCTReader.skipHeader`: advance past the LF-terminated header
    /// lines (the final header line is `0x0c0a`, i.e. blank after trimming).
    /// Returns the byte offset of the first pixel.
    fn skip_header(file: &mut fs::File) -> Result<u64> {
        let mut reader = std::io::BufReader::new(file);
        loop {
            let mut line = Vec::new();
            let n = read_line_bytes(&mut reader, &mut line)?;
            if n == 0 {
                break;
            }
            // Java: while (readLine().trim().length() > 0)
            let trimmed = String::from_utf8_lossy(&line);
            if trimmed.trim().is_empty() {
                break;
            }
        }
        Ok(reader.stream_position()?)
    }

    /// Port of `MicroCTReader.initFile`.
    fn init_file(&mut self, id: &Path) -> Result<()> {
        let original = abs(id);

        // FilePattern: find any other .vff files in the same dataset.
        self.vffs = match crate::stitcher::FilePattern::from_file(&original) {
            Ok(p) => {
                let names = p.filenames();
                if names.is_empty() {
                    vec![original.clone()]
                } else {
                    names
                }
            }
            Err(_) => vec![original.clone()],
        };
        self.header_size = vec![0u64; self.vffs.len()];

        // Find all non-vff metadata files in the same directory.
        let parent = original
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        if let Ok(rd) = fs::read_dir(&parent) {
            let mut entries: Vec<String> = rd
                .filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().into_string().ok())
                .collect();
            entries.sort();
            for file in entries {
                if !has_suffix(&file, "vff") {
                    let metadata = parent.join(&file);
                    if !metadata.is_dir() {
                        self.metadata_files.push(metadata);
                    }
                }
            }
        }

        // sizeZ starts at the number of VFF files.
        self.meta.size_z = self.vffs.len() as u32;

        // Parse the VFF header of the opened file.
        let header = read_text(&original)?;
        let mut dim_count = 0i64;
        for raw in header.split('\n') {
            let line = raw.trim();
            if line.is_empty() {
                break;
            }
            if let Some(eq) = line.find('=') {
                let key = &line[..eq];
                // Java: value = line.substring(eq + 1, line.length() - 1)
                // (drops the trailing ';'). Use the trimmed line so the dropped
                // character is the ';' rather than a stray '\r'.
                let after = &line[eq + 1..];
                let value = if after.is_empty() {
                    after
                } else {
                    &after[..after.len() - 1]
                };

                self.process_key(key, value);

                match key {
                    "rank" => dim_count = parse_i64(value),
                    "size" => {
                        let dims: Vec<&str> = value.split(' ').collect();
                        if dim_count > 0 {
                            if let Some(d) = dims.first() {
                                self.meta.size_x = parse_i64(d).max(0) as u32;
                            }
                        }
                        if dim_count > 1 {
                            if let Some(d) = dims.get(1) {
                                self.meta.size_y = parse_i64(d).max(0) as u32;
                            }
                        }
                        if dim_count > 2 {
                            if let Some(d) = dims.get(2) {
                                self.meta.size_z =
                                    self.meta.size_z.saturating_mul(parse_i64(d).max(0) as u32);
                            }
                        }
                    }
                    "bits" => {
                        let bits = parse_i64(value);
                        self.meta.pixel_type = pixel_type_from_bytes(bits / 8, true, false);
                    }
                    "elementsize" => {
                        // physical size is stored in mm, not um.
                        if let Some(size) = parse_double(value) {
                            self.physical_size = Some(size * 1000.0);
                        }
                    }
                    _ => {}
                }
            }
        }

        self.meta.size_t = 1;
        self.meta.size_c = 1;
        self.meta.image_count = self.meta.size_z * self.meta.size_t * self.meta.size_c;
        self.meta.dimension_order = DimensionOrder::XYZCT;
        self.meta.is_rgb = false;
        self.meta.is_interleaved = false;
        // VFF pixel data is big-endian.
        self.meta.is_little_endian = false;
        self.meta.bits_per_pixel = (self.meta.pixel_type.bytes_per_sample() * 8) as u8;
        self.meta.resolution_count = 1;

        // Parse extra values from metadata files.
        let metadata_files = std::mem::take(&mut self.metadata_files);
        for file in &metadata_files {
            let name = file
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            let data = read_text(file)?;
            let data = data.trim();
            if has_suffix(&name, "protocol") || has_suffix(&name, "log") || name == "Parameters.txt"
            {
                // key/value pairs separated by '=' or ':'.
                let separator = if name == "Parameters.txt" { ':' } else { '=' };
                for pair in data.split("\r\n") {
                    if let Some(sep) = pair.find(separator) {
                        let k = pair[..sep].trim().to_string();
                        let v = pair[sep + 1..].trim().to_string();
                        self.process_key(&k, &v);
                    }
                }
            } else {
                // assume a single value; the file name is the key.
                self.process_key(&name, data);
            }
        }
        self.metadata_files = metadata_files;

        self.initialized = true;
        Ok(())
    }

    /// Port of `MicroCTReader.openBytes`: select the VFF, skip its header, read
    /// the requested raw rectangle, then reverse rows in the returned buffer
    /// (data is stored origin-lower-left).
    fn read_region(&mut self, no: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        if self.vffs.is_empty() {
            return Err(BioFormatsError::NotInitialized);
        }
        let n = self.vffs.len();
        let vff_index = (no as usize) % n;

        let path = self.vffs[vff_index].clone();
        let mut file = fs::File::open(&path)?;

        if self.header_size[vff_index] == 0 {
            self.header_size[vff_index] = Self::skip_header(&mut file)?;
        }
        let header = self.header_size[vff_index];

        let bpp = self.meta.pixel_type.bytes_per_sample();
        let sx = self.meta.size_x as usize;
        let plane_size = sx * self.meta.size_y as usize * bpp;
        let row_bytes = w as usize * bpp;
        let mut buf = vec![0u8; h as usize * row_bytes];

        let plane_offset = header + plane_size as u64 * (no as usize / n) as u64;
        let len = file.metadata()?.len();

        for row in 0..h as usize {
            let offset = plane_offset + (((y as usize + row) * sx + x as usize) * bpp) as u64;
            if offset >= len {
                break;
            }
            let avail = ((len - offset) as usize).min(row_bytes);
            file.seek(SeekFrom::Start(offset))?;
            file.read_exact(&mut buf[row * row_bytes..row * row_bytes + avail])?;
        }

        // Reverse the rows: origin is in the lower-left corner.
        if row_bytes > 0 {
            for yy in 0..h as usize / 2 {
                let top = (h as usize - 1 - yy) * row_bytes;
                let bottom = yy * row_bytes;
                for b in 0..row_bytes {
                    buf.swap(bottom + b, top + b);
                }
            }
        }
        Ok(buf)
    }
}

impl Default for MicroCtVffReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for MicroCtVffReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        // Java suffix "vff".
        matches!(
            path.extension().and_then(|e| e.to_str()),
            Some(e) if e.eq_ignore_ascii_case("vff")
        )
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // Java: stream.readString(4).equals(VFF_MAGIC)
        header.len() >= VFF_MAGIC.len() && &header[..VFF_MAGIC.len()] == VFF_MAGIC
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        self.init_file(path)
    }

    fn close(&mut self) -> Result<()> {
        self.vffs.clear();
        self.header_size.clear();
        self.metadata_files.clear();
        self.meta = ImageMetadata::default();
        self.date = None;
        self.time = None;
        self.image_description = None;
        self.exposure_time = None;
        self.physical_size = None;
        self.initialized = false;
        Ok(())
    }

    fn series_count(&self) -> usize {
        if self.initialized {
            1
        } else {
            0
        }
    }

    fn set_series(&mut self, series: usize) -> Result<()> {
        if series == 0 && self.initialized {
            Ok(())
        } else {
            Err(BioFormatsError::SeriesOutOfRange(series))
        }
    }

    fn series(&self) -> usize {
        0
    }

    fn metadata(&self) -> &ImageMetadata {
        if self.initialized {
            &self.meta
        } else {
            uninitialized_metadata()
        }
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        if !self.initialized {
            return Err(BioFormatsError::NotInitialized);
        }
        if plane_index >= self.meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        self.read_region(plane_index, 0, 0, self.meta.size_x, self.meta.size_y)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        if !self.initialized {
            return Err(BioFormatsError::NotInitialized);
        }
        if plane_index >= self.meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        crate::common::region::validate_region(
            "MicroCT",
            self.meta.size_x,
            self.meta.size_y,
            x,
            y,
            w,
            h,
        )?;
        self.read_region(plane_index, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        self.open_bytes(plane_index)
    }

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        if !self.initialized {
            return None;
        }
        use crate::common::ome_metadata::{OmeImage, OmeMetadata, OmePlane};

        let acquisition_date = match (&self.date, &self.time) {
            (Some(d), Some(t)) => format_microct_date(d, t),
            _ => None,
        };

        // Per-plane exposure time (Java setPlaneExposureTime for all planes).
        let planes: Vec<OmePlane> = if let Some(exp) = self.exposure_time {
            (0..self.meta.image_count)
                .map(|_| OmePlane {
                    exposure_time: Some(exp),
                    ..Default::default()
                })
                .collect()
        } else {
            Vec::new()
        };

        let image = OmeImage {
            description: self.image_description.clone(),
            acquisition_date,
            physical_size_x: self.physical_size,
            physical_size_y: self.physical_size,
            physical_size_z: self.physical_size,
            planes,
            ..Default::default()
        };

        Some(OmeMetadata {
            images: vec![image],
            ..Default::default()
        })
    }
}

/// `loci.common.RandomAccessInputStream.readLine` analogue: read up to and
/// including the next `\n`, appending bytes (sans the trailing `\n`) to `out`.
/// Returns the number of bytes consumed from the stream.
fn read_line_bytes<R: Read>(reader: &mut R, out: &mut Vec<u8>) -> Result<usize> {
    let mut consumed = 0usize;
    let mut byte = [0u8; 1];
    loop {
        match reader.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => {
                consumed += 1;
                if byte[0] == b'\n' {
                    break;
                }
                out.push(byte[0]);
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(consumed)
}

/// `loci.formats.FormatReader.checkSuffix(name, suffix)` — case-insensitive
/// extension test.
fn has_suffix(name: &str, suffix: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(&format!(".{}", suffix.to_ascii_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn unique_dir() -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("bruker_test_{}_{}", std::process::id(), nanos));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn detects_companion_filenames() {
        let r = BrukerReader::new();
        assert!(r.is_this_type_by_name(Path::new("/data/study/1/acqp")));
        assert!(r.is_this_type_by_name(Path::new("/data/study/1/fid")));
        assert!(!r.is_this_type_by_name(Path::new("/data/study/1/2dseq")));
        assert!(!r.is_this_type_by_name(Path::new("/data/study/image.tif")));
        assert!(!r.is_this_type_by_bytes(&[0x49, 0x49, 0x2a, 0x00]));
    }

    #[test]
    fn reads_synthetic_dataset() {
        let root = unique_dir();
        // <root>/1/acqp  and  <root>/1/pdata/1/{2dseq,reco,d3proc}
        let acq_dir = root.join("1");
        let pdata1 = acq_dir.join("pdata").join("1");
        fs::create_dir_all(&pdata1).unwrap();

        let acqp = "##$NI=1\n##$NR=1\n##$ACQ_ns_list_size=1\n##$BYTORDA=little\n\
                    ##$ACQ_time=15:14:00  5 Jan 2015\n\
                    ##$ACQ_institution=( 60 )\n<My Institute>\n\
                    ##$ACQ_operator=( 60 )\n<Jane Doe>\n\
                    ##$ACQ_scan_name=( 32 )\n<my scan>\n";
        fs::write(acq_dir.join("acqp"), acqp).unwrap();

        let reco = "##$RECO_size=( 2 )\n128 128\n##$RECO_wordtype=_16BIT_SGN_INT\n";
        fs::write(pdata1.join("reco"), reco).unwrap();

        let d3proc = "##$IM_SIX=128\n##$IM_SIY=128\n##$IM_SIZ=1\n##$IM_SIT=1\n";
        fs::write(pdata1.join("d3proc"), d3proc).unwrap();

        // 128*128*2 bytes of pixel data, byte i = i % 251.
        let plane: Vec<u8> = (0..128 * 128 * 2).map(|i| (i % 251) as u8).collect();
        let mut f = fs::File::create(pdata1.join("2dseq")).unwrap();
        f.write_all(&plane).unwrap();
        drop(f);

        let mut reader = BrukerReader::new();
        reader.set_id(&acq_dir.join("acqp")).unwrap();

        assert_eq!(reader.series_count(), 1);
        let m = reader.metadata();
        assert_eq!(m.size_x, 128);
        assert_eq!(m.size_y, 128);
        assert_eq!(m.size_z, 1);
        assert_eq!(m.size_t, 1);
        assert_eq!(m.size_c, 1);
        assert_eq!(m.pixel_type, PixelType::Int16);
        assert!(m.is_little_endian);
        assert_eq!(m.image_count, 1);
        assert_eq!(m.dimension_order, DimensionOrder::XYCTZ);
        // series_metadata retains the raw Bruker keys (stripped of "##$").
        assert_eq!(
            m.series_metadata
                .get("ACQ_institution")
                .map(|v| v.to_string()),
            Some("My Institute".to_string())
        );

        let bytes = reader.open_bytes(0).unwrap();
        assert_eq!(bytes.len(), 128 * 128 * 2);
        assert_eq!(bytes, plane);

        // Region read: top-left 4x4.
        let region = reader.open_bytes_region(0, 0, 0, 4, 4).unwrap();
        assert_eq!(region.len(), 4 * 4 * 2);

        // OME image name surfaced.
        let ome = reader.ome_metadata().unwrap();
        assert_eq!(ome.images.len(), 1);
        assert_eq!(ome.images[0].name.as_deref(), Some("my scan #1"));
        // Acquisition date from ##$ACQ_time (Java setImageAcquisitionDate).
        assert_eq!(
            ome.images[0].acquisition_date.as_deref(),
            Some("2015-01-05T15:14:00")
        );
        // Experimenter institution/last name from ##$ACQ_institution / ##$ACQ_operator.
        assert_eq!(ome.experimenters.len(), 1);
        assert_eq!(
            ome.experimenters[0].institution.as_deref(),
            Some("My Institute")
        );
        assert_eq!(ome.experimenters[0].last_name.as_deref(), Some("Jane Doe"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn bruker_date_parsing() {
        // Java DATE_FORMAT "HH:mm:ss  d MMM yyyy" (double space collapses).
        assert_eq!(
            format_bruker_date("15:14:00  5 Jan 2015").as_deref(),
            Some("2015-01-05T15:14:00")
        );
        assert_eq!(
            format_bruker_date("09:05:01 12 Dec 1999").as_deref(),
            Some("1999-12-12T09:05:01")
        );
        // Malformed strings yield None (DateTools.formatDate returns null).
        assert_eq!(format_bruker_date("garbage"), None);
        assert_eq!(format_bruker_date("15:14:00 5 Foo 2015"), None);
    }

    // -- MicroCT (GE VFF) reader tests --

    #[test]
    fn microct_detects_vff() {
        let r = MicroCtVffReader::new();
        assert!(r.is_this_type_by_name(Path::new("/data/scan/slice0001.vff")));
        assert!(r.is_this_type_by_name(Path::new("/data/scan/slice.VFF")));
        assert!(!r.is_this_type_by_name(Path::new("/data/scan/slice.tif")));
        assert!(r.is_this_type_by_bytes(b"ncaa\nrank=2;\n"));
        assert!(!r.is_this_type_by_bytes(b"II*\0"));
    }

    #[test]
    fn microct_date_format() {
        assert_eq!(
            format_microct_date("Mon, Jan 05, 2015", "03:14:00 PM"),
            Some("2015-01-05T15:14:00".to_string())
        );
        assert_eq!(
            format_microct_date("Tue, Dec 31, 2019", "12:00:00 AM"),
            Some("2019-12-31T00:00:00".to_string())
        );
        assert_eq!(
            format_microct_date("Tue, Dec 31, 2019", "12:00:00 PM"),
            Some("2019-12-31T12:00:00".to_string())
        );
        assert_eq!(format_microct_date("garbage", "x"), None);
    }

    #[test]
    fn microct_reads_synthetic_vff_and_companion_metadata() {
        let root = unique_dir();
        let vff = root.join("scan0001.vff");

        // 4x3 unsigned-byte plane (bits=8); rows stored origin lower-left.
        let sx = 4usize;
        let sy = 3usize;
        let header = "ncaa\nrank=2;\nsize=4 3;\nbits=8;\nelementsize=0.025;\n\u{0c}\n";
        // pixel value = row index, so we can verify row reversal.
        let mut pixels = Vec::new();
        for row in 0..sy {
            for _ in 0..sx {
                pixels.push(row as u8);
            }
        }
        {
            let mut f = fs::File::create(&vff).unwrap();
            f.write_all(header.as_bytes()).unwrap();
            f.write_all(&pixels).unwrap();
        }

        // Companion metadata files.
        fs::write(
            root.join("scan.log"),
            "Exposure Time (ms)=500\r\nGantry=A\r\n",
        )
        .unwrap();
        fs::write(
            root.join("scan.protocol"),
            "Date=Mon, Jan 05, 2015\r\nTime=03:14:00 PM\r\n",
        )
        .unwrap();
        fs::write(root.join("Parameters.txt"), "Voltage:80\r\nCurrent:200\r\n").unwrap();
        fs::write(root.join("Description.txt"), "test microCT scan").unwrap();

        let mut reader = MicroCtVffReader::new();
        reader.set_id(&vff).unwrap();

        assert_eq!(reader.series_count(), 1);
        let m = reader.metadata();
        assert_eq!(m.size_x, 4);
        assert_eq!(m.size_y, 3);
        assert_eq!(m.size_z, 1);
        assert_eq!(m.size_t, 1);
        assert_eq!(m.size_c, 1);
        assert_eq!(m.pixel_type, PixelType::Int8);
        assert!(!m.is_little_endian);
        assert_eq!(m.dimension_order, DimensionOrder::XYZCT);

        // Original metadata captured the VFF header + companion keys.
        assert!(m.series_metadata.contains_key("rank"));
        assert!(m.series_metadata.contains_key("Gantry"));
        assert!(m.series_metadata.contains_key("Voltage"));
        assert!(m.series_metadata.contains_key("Description.txt"));

        // Row reversal: stored rows are [0,0,0,0][1,1,1,1][2,2,2,2];
        // after reversal the first row should be the last (value 2).
        let bytes = reader.open_bytes(0).unwrap();
        assert_eq!(bytes.len(), sx * sy);
        assert_eq!(&bytes[0..sx], &[2u8, 2, 2, 2]);
        assert_eq!(&bytes[2 * sx..3 * sx], &[0u8, 0, 0, 0]);

        // Java reads the requested raw rectangle first, then reverses only the
        // returned rows.  This differs from cropping a fully row-reversed plane.
        let region = reader.open_bytes_region(0, 1, 0, 2, 2).unwrap();
        assert_eq!(region, vec![1, 1, 0, 0]);

        // OME metadata: description, physical size (mm -> um), exposure, date.
        let ome = reader.ome_metadata().unwrap();
        assert_eq!(ome.images.len(), 1);
        let img = &ome.images[0];
        assert_eq!(img.description.as_deref(), Some("test microCT scan"));
        assert_eq!(img.physical_size_x, Some(25.0)); // 0.025 mm * 1000
        assert_eq!(img.acquisition_date.as_deref(), Some("2015-01-05T15:14:00"));
        assert!(!img.planes.is_empty());
        assert_eq!(img.planes[0].exposure_time, Some(0.5)); // 500 ms -> 0.5 s

        let _ = fs::remove_dir_all(&root);
    }
}
