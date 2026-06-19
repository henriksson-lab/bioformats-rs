//! Olympus FV1000 OIF/OIB format reader.
//!
//! An OIF dataset is a Windows INI-style text file (`.oif`) describing a
//! multi-channel, multi-z, multi-time confocal acquisition. Pixel data are
//! stored as individual TIFF files in a companion directory named
//! `<stem>.files/` or `<stem>/`, indexed by per-plane `.pty` INI files.
//!
//! The single-file `.oib` variant is an OLE2/Compound Document (CFB). The
//! embedded `OibInfo.txt` stream maps logical file names (the `.oif`, `.pty`
//! and TIFF names) to CFB stream paths; all reads then go through those
//! streams.
//!
//! Following the Java `FV1000Reader`:
//!   - `ProfileSaveInfo` in the `.oif` lists the per-plane `.pty` files via
//!     `IniFileNameN` keys.
//!   - `Axis N Parameters Common` gives the `AxisCode` / `MaxSize` for each of
//!     the 9 dimension axes.
//!   - Each `.pty` file's `File Info / DataName` names the TIFF for that plane,
//!     and its `Axis N Parameters / Number` entries build the dimension order.
//!   - The dimension order starts as "XY" and appends C, Z, T (in axis order)
//!     for any axis whose `Number` is greater than one.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::tiff::TiffReader;

const NUM_DIMENSIONS: usize = 9;

/// Source of a logical file: either a path on disk (OIF) or a stream inside the
/// OLE2/Compound Document (OIB).
enum FileSource {
    /// OIF: files live on disk; `dir` is the companion `.files/` directory.
    Disk,
    /// OIB: `mapping` maps a logical (sanitized) file name to a CFB stream path.
    Oib {
        path: PathBuf,
        mapping: HashMap<String, String>,
    },
}

impl FileSource {
    /// Read the entire contents of a logical file as bytes.
    fn read_bytes(&self, logical: &str) -> Result<Vec<u8>> {
        match self {
            FileSource::Disk => {
                let mut f = File::open(logical).map_err(BioFormatsError::Io)?;
                let mut buf = Vec::new();
                f.read_to_end(&mut buf).map_err(BioFormatsError::Io)?;
                Ok(buf)
            }
            FileSource::Oib { path, mapping } => {
                let key = sanitize_value(logical);
                let stream_path = mapping.get(&key).ok_or_else(|| {
                    BioFormatsError::Format(format!("OIB: logical file not found: {logical}"))
                })?;
                let mut comp = cfb::open(path)
                    .map_err(|e| BioFormatsError::Format(format!("OIB CFB open: {e}")))?;
                let norm = normalize_cfb_path(stream_path);
                let mut stream = comp
                    .open_stream(&norm)
                    .or_else(|_| comp.open_stream(stream_path))
                    .map_err(|e| BioFormatsError::Format(format!("OIB stream {norm}: {e}")))?;
                let mut buf = Vec::new();
                stream.read_to_end(&mut buf).map_err(BioFormatsError::Io)?;
                Ok(buf)
            }
        }
    }
}

/// Normalise a CFB path: backslashes to forward slashes, drop "Root Entry".
fn normalize_cfb_path(path: &str) -> String {
    let p = path.replace('\\', "/");
    let p = p.replace("Root Entry/", "").replace("/Root Entry", "");
    if p.starts_with('/') {
        p
    } else {
        format!("/{p}")
    }
}

/// Decode bytes that may be UTF-16LE (with or without BOM) or UTF-8.
fn decode_text(bytes: &[u8]) -> String {
    if bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] == 0xFE {
        let u16s: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16_lossy(&u16s)
    } else if bytes.iter().take(64).filter(|&&b| b == 0).count() > 8 {
        let u16s: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16_lossy(&u16s)
    } else {
        String::from_utf8_lossy(bytes).to_string()
    }
}

/// A parsed INI document: ordered sections, each a key->value map.
struct IniList {
    sections: Vec<(String, HashMap<String, String>)>,
}

impl IniList {
    fn parse(text: &str) -> IniList {
        let mut sections: Vec<(String, HashMap<String, String>)> = Vec::new();
        let mut current: Option<(String, HashMap<String, String>)> = None;
        // Java strips everything before the first '['.
        let start = text.find('[').unwrap_or(0);
        for line in text[start..].lines() {
            let t = line.trim_matches(|c| c == '\r' || c == '\n');
            let t = t.trim();
            if t.is_empty() {
                continue;
            }
            if t.starts_with('[') && t.ends_with(']') {
                if let Some(sec) = current.take() {
                    sections.push(sec);
                }
                let name = t[1..t.len() - 1].to_string();
                current = Some((name, HashMap::new()));
            } else if let Some((_, map)) = current.as_mut() {
                if let Some(eq) = t.find('=') {
                    let key = t[..eq].trim().to_string();
                    let value = sanitize_value(t[eq + 1..].trim());
                    map.insert(key, value);
                }
            }
        }
        if let Some(sec) = current.take() {
            sections.push(sec);
        }
        IniList { sections }
    }

    fn table(&self, name: &str) -> Option<&HashMap<String, String>> {
        self.sections
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, m)| m)
    }
}

/// Java sanitizeValue: strip quotes, normalise separators, drop "GST..." runs.
fn sanitize_value(value: &str) -> String {
    let mut f = value.replace('"', "");
    f = f.replace('\\', "/");
    while f.contains("GST") {
        f = remove_gst(&f);
    }
    f
}

/// Java removeGST.
fn remove_gst(s: &str) -> String {
    if let Some(gst) = s.find("GST") {
        let first = &s[..gst];
        let sep = s.find('/').unwrap_or(s.len());
        let ndx = if sep < gst { s.len() } else { sep };
        // last "=" before ndx
        let last = match s[..ndx.min(s.len())].rfind('=') {
            Some(eq) => &s[eq + 1..],
            None => s,
        };
        format!("{first}{last}")
    } else {
        s.to_string()
    }
}

fn replace_extension(name: &str, old_ext: &str, new_ext: &str) -> String {
    let suffix = format!(".{old_ext}");
    if name.to_ascii_lowercase().ends_with(&suffix) {
        format!("{}{}", &name[..name.len() - old_ext.len()], new_ext)
    } else {
        name.to_string()
    }
}

/// "-R" near the tail indicates a preview image (Java isPreviewName).
fn is_preview_name(name: &str) -> bool {
    if let Some(idx) = name.find("-R") {
        idx == name.len().saturating_sub(9)
    } else {
        false
    }
}

/// Parsed plane data (subset of Java PlaneData).
#[derive(Default, Clone)]
struct PlaneData {
    delta_t: Option<f64>,
    position_z: Option<f64>,
}

/// Extract the integer at each `%03d` field of `pattern` from `string`
/// (Java FV1000Reader.scanFormat). Returns one integer per format field.
fn scan_format(pattern: &str, string: &str) -> Option<Vec<i64>> {
    let pat: Vec<char> = pattern.chars().collect();
    let s: Vec<char> = string.chars().collect();
    let mut result = Vec::new();
    let mut so = 0usize; // string index
    let mut i = 0usize; // pattern index
    while i < pat.len() {
        if pat[i] == '%' && i + 1 < pat.len() && pat[i + 1] == '0' {
            // At a "%03d" field: read a run of digits from the string.
            let mut end = so;
            while end < s.len() && s[end].is_ascii_digit() {
                end += 1;
            }
            let num: String = s[so..end].iter().collect();
            result.push(num.parse::<i64>().ok()?);
            so = end;
            // Skip the format specifier in the pattern (until past 'd').
            i += 1;
            while i < pat.len() && pat[i - 1] != 'd' {
                i += 1;
            }
        } else {
            // Literal char: must match.
            if so >= s.len() || s[so] != pat[i] {
                return None;
            }
            so += 1;
            i += 1;
        }
    }
    Some(result)
}

/// Java FormatTools.rasterToPosition: block 0 varies fastest.
fn raster_to_position(lengths: &[i64], mut raster: i64) -> Vec<i64> {
    let mut pos = vec![0i64; lengths.len()];
    let mut offset = 1i64;
    for i in 0..lengths.len() {
        let offset1 = offset * lengths[i];
        let q = if i < lengths.len() - 1 {
            raster % offset1
        } else {
            raster
        };
        pos[i] = q / offset;
        raster -= q;
        offset = offset1;
    }
    pos
}

/// Synthesise the per-plane `.pty` file list from FV1000 v2 metadata.
///
/// Mirrors FV1000Reader.addPtyFiles: given the first and last `.pty` names and
/// a printf pattern (default `s_C%03dT%03d.pty`), enumerate every plane in
/// raster order (block 0 fastest), keyed 0..total.
fn add_pty_files(
    pty_start: Option<&str>,
    pty_end: Option<&str>,
    pty_pattern: Option<&str>,
    filenames: &mut BTreeMap<usize, String>,
) {
    let (Some(start), Some(end)) = (pty_start, pty_end) else {
        return;
    };
    // Default pattern uses the directory prefix of ptyStart.
    let pattern = match pty_pattern {
        Some(p) => p.to_string(),
        None => {
            let dir = match start.find('/') {
                Some(slash) => &start[..=slash],
                None => "",
            };
            format!("{dir}s_C%03dT%03d.pty")
        }
    };

    let prefixes: Vec<&str> = pattern.split("%03d").collect();
    let Some(first) = scan_format(&pattern, start) else {
        return;
    };
    let Some(last) = scan_format(&pattern, end) else {
        return;
    };
    if first.len() != last.len() || first.is_empty() {
        return;
    }
    let mut lengths = Vec::with_capacity(first.len());
    let mut total: i64 = 1;
    for i in 0..first.len() {
        let len = last[i] - first[i] + 1;
        if len <= 0 {
            return;
        }
        lengths.push(len);
        total *= len;
    }

    for file in 0..total {
        let pos = raster_to_position(&lengths, file);
        let mut name = String::new();
        for (block, prefix) in prefixes.iter().enumerate() {
            name.push_str(prefix);
            if block < pos.len() {
                let num = pos[block] + 1;
                name.push_str(&format!("{num:03}"));
            }
        }
        filenames.insert(file as usize, name);
    }
}

/// One active acquisition channel, mirroring Java FV1000Reader.ChannelData
/// (only the fields needed for OME parity).
#[derive(Debug, Clone, Default)]
struct OifChannel {
    name: Option<String>,
    emission: Option<f64>,
    excitation: Option<f64>,
}

pub struct Fv1000Reader {
    source: FileSource,
    /// Companion directory for OIF (path prefix); empty for OIB.
    path_prefix: String,
    meta: Option<ImageMetadata>,
    /// Resolved TIFF logical names, in plane order.
    tiffs: Vec<String>,
    #[allow(dead_code)]
    planes: Vec<PlaneData>,
    /// Physical pixel sizes (µm) from WidthConvertValue / HeightConvertValue.
    physical_size_x: Option<f64>,
    physical_size_y: Option<f64>,
    /// Active channels in acquisition order.
    channels: Vec<OifChannel>,
}

impl Fv1000Reader {
    pub fn new() -> Self {
        Fv1000Reader {
            source: FileSource::Disk,
            path_prefix: String::new(),
            meta: None,
            tiffs: Vec::new(),
            planes: Vec::new(),
            physical_size_x: None,
            physical_size_y: None,
            channels: Vec::new(),
        }
    }

    /// Resolve and initialise from an `.oif` text + companion directory.
    fn init_oif(&mut self, oif_path: &Path) -> Result<()> {
        self.source = FileSource::Disk;
        let oif_text = decode_text(&std::fs::read(oif_path).map_err(BioFormatsError::Io)?);
        let dir = oif_path.parent().unwrap_or_else(|| Path::new("."));
        // The companion directory: prefer "<stem>.files" then "<stem>".
        let companion = find_companion_dir(oif_path);
        let prefix = companion
            .clone()
            .map(|d| d.to_string_lossy().to_string())
            .unwrap_or_else(|| dir.to_string_lossy().to_string());
        self.path_prefix = prefix;
        self.build(&oif_text)
    }

    /// Resolve and initialise from a `.oib` OLE2 compound document.
    fn init_oib(&mut self, oib_path: &Path) -> Result<()> {
        // Read OibInfo.txt and build the logical-name -> stream mapping.
        let mut comp = cfb::open(oib_path)
            .map_err(|e| BioFormatsError::Format(format!("OIB CFB open: {e}")))?;

        // Locate OibInfo.txt among the streams.
        let info_stream = comp
            .walk()
            .filter(|e| e.is_stream())
            .map(|e| e.path().to_string_lossy().to_string())
            .find(|p| {
                p.replace('\\', "/")
                    .to_ascii_lowercase()
                    .ends_with("oibinfo.txt")
            })
            .ok_or_else(|| {
                BioFormatsError::Format("OIB: OibInfo.txt not found in compound document".into())
            })?;
        let mut info_data = Vec::new();
        comp.open_stream(&info_stream)
            .map_err(|e| BioFormatsError::Format(format!("OIB OibInfo stream: {e}")))?
            .read_to_end(&mut info_data)
            .map_err(BioFormatsError::Io)?;
        let info_text = decode_text(&info_data);

        let (oif_name, mapping) = map_oib_files(&info_text);
        let oif_name = oif_name
            .ok_or_else(|| BioFormatsError::Format("OIB: no .oif entry in OibInfo.txt".into()))?;

        self.source = FileSource::Oib {
            path: oib_path.to_path_buf(),
            mapping,
        };
        self.path_prefix = String::new();

        let oif_bytes = self.source.read_bytes(&oif_name)?;
        let oif_text = decode_text(&oif_bytes);
        self.build(&oif_text)
    }

    /// Common build path shared by OIF and OIB once the `.oif` text and a
    /// `FileSource` are available.
    fn build(&mut self, oif_text: &str) -> Result<()> {
        let f = IniList::parse(oif_text);

        // ---- ProfileSaveInfo: collect .pty file names (IniFileNameN) ----
        let mut filenames: BTreeMap<usize, String> = BTreeMap::new();
        // Mirrors Java FV1000Reader.previewNames.size(): number of preview
        // ("-R") planes that resolve to a ".tif" name. Used in the image-count
        // reconciliation branch below.
        let mut preview_count: usize = 0;
        // FV1000 v2 lists only the first/last .pty plus a printf pattern; v1
        // lists each .pty via IniFileNameN.
        let mut pty_start: Option<String> = None;
        let mut pty_end: Option<String> = None;
        let mut pty_pattern: Option<String> = None;
        if let Some(save_info) = f.table("ProfileSaveInfo") {
            for (key, value) in save_info {
                let value = sanitize_value(value);
                let value = value.trim().to_string();
                if key.starts_with("IniFileName")
                    && !key.contains("Thumb")
                    && !is_preview_name(&value)
                {
                    if let Ok(idx) = key[11..].parse::<usize>() {
                        filenames.insert(idx, value);
                    }
                } else if key == "PtyFileNameS" {
                    pty_start = Some(value);
                } else if key == "PtyFileNameE" {
                    pty_end = Some(value);
                } else if key == "PtyFileNameT2" {
                    pty_pattern = Some(value);
                } else if key.starts_with("IniFileName")
                    && !key.contains("Thumb")
                    && is_preview_name(&value)
                {
                    // Java: isPreviewName(value) branch populates previewNames.
                    // Java additionally requires the referenced file to exist and
                    // its ".pty"->".tif" name to end in ".tif" (FV1000Reader:466-490,
                    // 583-590). We approximate by counting preview entries here so
                    // the diff==previewCount reconciliation branch below can fire.
                    let tif = replace_extension(&value, "pty", "tif");
                    if tif.ends_with(".tif") {
                        preview_count += 1;
                    }
                }
            }
        }

        // FV1000Reader.addPtyFiles: when no per-plane IniFileNameN entries are
        // present, synthesise the .pty list from the start/end names and the
        // printf pattern (e.g. "s_C%03dZ%03d.pty"), block 0 varying fastest.
        if filenames.is_empty() {
            add_pty_files(
                pty_start.as_deref(),
                pty_end.as_deref(),
                pty_pattern.as_deref(),
                &mut filenames,
            );
        }

        // ---- Axis N Parameters Common: AxisCode / MaxSize ----
        let mut code = vec![String::new(); NUM_DIMENSIONS];
        let mut size = vec![1u32; NUM_DIMENSIONS];
        for i in 0..NUM_DIMENSIONS {
            if let Some(common) = f.table(&format!("Axis {i} Parameters Common")) {
                code[i] = common.get("AxisCode").cloned().unwrap_or_default();
                size[i] = common
                    .get("MaxSize")
                    .and_then(|s| s.trim().parse::<u32>().ok())
                    .unwrap_or(1);
            }
        }

        // ---- Reference Image Parameter: ImageDepth / ValidBitCounts ----
        // Physical pixel sizes (µm) come from WidthConvertValue / HeightConvertValue
        // here too (Java FV1000Reader ~505).
        let mut image_depth = 1u32;
        let mut valid_bits = 0u32;
        if let Some(rip) = f.table("Reference Image Parameter") {
            image_depth = rip
                .get("ImageDepth")
                .and_then(|s| s.trim().parse::<u32>().ok())
                .unwrap_or(1);
            if let Some(vb) = rip.get("ValidBitCounts") {
                valid_bits = vb.trim().parse::<u32>().unwrap_or(0);
            }
            self.physical_size_x = rip
                .get("WidthConvertValue")
                .and_then(|s| sanitize_value(s).trim().parse::<f64>().ok())
                .filter(|v| *v > 0.0);
            self.physical_size_y = rip
                .get("HeightConvertValue")
                .and_then(|s| sanitize_value(s).trim().parse::<f64>().ok())
                .filter(|v| *v > 0.0);
        }

        // ---- GUI Channel N Parameters: active channels in acquisition order ----
        // Mirrors FV1000Reader (~532-553, ~1055-1088): collect channels with
        // CH Activate != 0, using CH Name, EmissionWavelength, ExcitationWavelength.
        let mut channels: Vec<OifChannel> = Vec::new();
        let mut ch_index = 1usize;
        while let Some(gui) = f.table(&format!("GUI Channel {ch_index} Parameters")) {
            let active = gui
                .get("CH Activate")
                .and_then(|s| s.trim().parse::<i64>().ok())
                .map(|v| v != 0)
                .unwrap_or(false);
            if active {
                let parse_wave = |k: &str| -> Option<f64> {
                    gui.get(k)
                        .and_then(|s| sanitize_value(s).trim().parse::<f64>().ok())
                        .filter(|v| *v > 0.0)
                };
                channels.push(OifChannel {
                    name: gui
                        .get("CH Name")
                        .map(|s| sanitize_value(s))
                        .filter(|s| !s.is_empty()),
                    emission: parse_wave("EmissionWavelength"),
                    excitation: parse_wave("ExcitationWavelength"),
                });
            }
            ch_index += 1;
        }
        self.channels = channels;

        let mut image_count = filenames.len();
        if image_count == 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "OIF/OIB does not reference any PTY image planes".into(),
            ));
        }

        // ---- Build dimension order + tiff list by reading each .pty ----
        let mut dimension_order = String::from("XY");
        let mut tiffs: Vec<String> = Vec::with_capacity(image_count);
        let mut planes: Vec<PlaneData> = Vec::new();
        let mut tiff_dir: Option<String>;

        let keys: Vec<usize> = filenames.keys().copied().collect();
        let mut ki = 0usize;
        let mut produced = 0usize;
        while produced < image_count && ki < keys.len() {
            let mut file = match filenames.get(&keys[ki]) {
                Some(s) => s.clone(),
                None => {
                    ki += 1;
                    continue;
                }
            };
            ki += 1;
            file = sanitize_file(&file, &self.path_prefix);

            // Establish tiff directory from the .pty location.
            if let Some(slash) = file.rfind('/') {
                tiff_dir = Some(file[..slash].to_string());
            } else {
                tiff_dir = Some(file.clone());
            }

            let mut pty_bytes = self.source.read_bytes(&file);
            // Java FV1000Reader (~660-674): when the .pty path embedded in the
            // metadata names a directory that does not exist on disk (e.g. the
            // dataset has been renamed), rebuild the path as
            // "<companion .files dir>/<pty basename>".
            if pty_bytes.is_err() && matches!(self.source, FileSource::Disk) {
                let base = file.rsplit('/').next().unwrap_or(&file).to_string();
                let rebuilt = if self.path_prefix.is_empty() {
                    base.clone()
                } else if self.path_prefix.ends_with('/') {
                    format!("{}{}", self.path_prefix, base)
                } else {
                    format!("{}/{}", self.path_prefix, base)
                };
                if let Ok(b) = self.source.read_bytes(&rebuilt) {
                    file = rebuilt;
                    if let Some(slash) = file.rfind('/') {
                        tiff_dir = Some(file[..slash].to_string());
                    } else {
                        tiff_dir = Some(file.clone());
                    }
                    pty_bytes = Ok(b);
                }
            }
            let pty_bytes = match pty_bytes {
                Ok(b) => b,
                Err(_) => {
                    return Err(BioFormatsError::Format(format!(
                        "OIF/OIB: referenced PTY file {file} could not be read"
                    )));
                }
            };
            let pty = IniList::parse(&decode_text(&pty_bytes));

            // File Info / DataName -> TIFF name
            if let Some(file_info) = pty.table("File Info") {
                if let Some(data_name) = file_info.get("DataName") {
                    let mut dn = sanitize_value(data_name);
                    if !is_preview_name(&dn) {
                        while dn.contains("GST") {
                            dn = remove_gst(&dn);
                        }
                        let dir = tiff_dir.clone().unwrap_or_default();
                        let mut full = if dir.is_empty() {
                            dn.clone()
                        } else {
                            format!("{dir}/{dn}")
                        };
                        full = replace_extension(&full, "pty", "tif");
                        tiffs.push(full);
                    }
                }
            }

            // Axis N Parameters: build dimension order from axes with Number>1.
            let mut plane = PlaneData::default();
            for dim in 0..NUM_DIMENSIONS {
                let Some(axis) = pty.table(&format!("Axis {dim} Parameters")) else {
                    break;
                };
                let number = axis
                    .get("Number")
                    .and_then(|s| s.trim().parse::<u32>().ok())
                    .unwrap_or(1);
                let add_axis = number > 1;
                match dim {
                    2 => {
                        if add_axis && !dimension_order.contains('C') {
                            dimension_order.push('C');
                        }
                    }
                    3 => {
                        if add_axis && !dimension_order.contains('Z') {
                            dimension_order.push('Z');
                        }
                        plane.position_z = axis
                            .get("AbsPositionValue")
                            .and_then(|s| s.trim().parse::<f64>().ok());
                    }
                    4 => {
                        if add_axis && !dimension_order.contains('T') {
                            dimension_order.push('T');
                        }
                        plane.delta_t = axis
                            .get("AbsPositionValue")
                            .and_then(|s| s.trim().parse::<f64>().ok())
                            .map(|v| v / 1000.0);
                    }
                    _ => {}
                }
            }
            planes.push(plane);
            produced += 1;

            // Java FV1000Reader lets per-plane Acquisition Parameters Common
            // override Reference Image Parameter / ValidBitCounts.
            if let Some(acquisition) = pty.table("Acquisition Parameters Common") {
                if let Some(vb) = acquisition.get("ValidBitCounts") {
                    if let Ok(parsed) = vb.trim().parse::<u32>() {
                        valid_bits = parsed;
                    }
                }
            }
        }

        if tiffs.len() != image_count {
            return Err(BioFormatsError::Format(format!(
                "OIF/OIB: referenced {image_count} PTY plane(s) but resolved {} TIFF plane(s)",
                tiffs.len()
            )));
        }
        if tiffs.is_empty() {
            return Err(BioFormatsError::UnsupportedFormat(
                "OIF/OIB does not reference any TIFF image planes".into(),
            ));
        }

        // ---- Compute axis sizes from the OIF axis codes ----
        let mut size_x = 0u32;
        let mut size_y = 0u32;
        let mut size_z = 0u32;
        let mut size_c = 0u32;
        let mut size_t = 0u32;
        for i in 0..NUM_DIMENSIONS {
            let ss = size[i];
            match code[i].as_str() {
                "X" => size_x = ss,
                "Y" if ss > 1 => size_y = ss,
                "Z" => {
                    if size_y == 0 {
                        size_y = ss;
                    } else {
                        size_z = ss;
                    }
                }
                "T" => {
                    if size_y == 0 {
                        size_y = ss;
                    } else {
                        size_t = ss;
                    }
                }
                _ => {
                    if ss > 0 {
                        if size_c == 0 {
                            size_c = ss;
                        } else {
                            size_c *= ss;
                        }
                    }
                }
            }
        }
        if size_z == 0 {
            size_z = 1;
        }
        if size_c == 0 {
            size_c = 1;
        }
        if size_t == 0 {
            size_t = 1;
        }

        // Java image-count reconciliation.
        if image_count as u32 == size_c && size_y == 1 {
            image_count = (image_count as u32 * size_z * size_t) as usize;
        } else if image_count as u32 == size_c {
            size_z = 1;
            size_t = 1;
        }

        if size_z * size_t * size_c != image_count as u32 {
            // Java FV1000Reader.java:874-882 — diff is a *signed* plane-count
            // delta. When diff == previewNames.size() or diff < 0, divide by
            // sizeC and SUBTRACT from the relevant dimension (so a negative diff
            // GROWS the dimension); otherwise add diff to imageCount.
            let mut diff = (size_z * size_c * size_t) as i64 - image_count as i64;
            if diff == preview_count as i64 || diff < 0 {
                diff /= size_c.max(1) as i64;
                if size_t > 1 && size_z == 1 {
                    size_t = (size_t as i64 - diff) as u32;
                } else if size_z > 1 && size_t == 1 {
                    size_z = (size_z as i64 - diff) as u32;
                }
            } else {
                image_count = (image_count as i64 + diff) as usize;
            }
        }

        // Finalise dimension order: append remaining axes.
        if size_c > 1 && size_z == 1 && size_t == 1 && !dimension_order.contains('C') {
            dimension_order.push('C');
        }
        if !dimension_order.contains('Z') {
            dimension_order.push('Z');
        }
        if !dimension_order.contains('C') {
            dimension_order.push('C');
        }
        if !dimension_order.contains('T') {
            dimension_order.push('T');
        }

        let dimension_order = parse_dimension_order(&dimension_order);

        // ---- Pixel type from ImageDepth (bytes) ----
        let mut pixel_type = match image_depth {
            1 => PixelType::Uint8,
            2 => PixelType::Uint16,
            4 => PixelType::Float32,
            _ => {
                return Err(BioFormatsError::Format(format!(
                    "OIF/OIB: unsupported ImageDepth {image_depth}"
                )))
            }
        };
        let mut bits = if valid_bits > 0 {
            valid_bits as u8
        } else {
            (image_depth * 8).max(8) as u8
        };
        let mut is_little_endian = true;
        let mut is_rgb = false;

        // Derive endianness / RGB / refined pixel type from the first TIFF.
        // Java FV1000Reader reports `validBits` (ValidBitCounts, e.g. 12) as the
        // bits-per-pixel even though the TIFF stores 16-bit samples, so only fall
        // back to the TIFF depth when ValidBitCounts is absent.
        if let Some(first) = tiffs.first() {
            if let Ok(bytes) = self.source.read_bytes(first) {
                if let Some(tm) = probe_tiff(&bytes) {
                    pixel_type = tm.0;
                    is_little_endian = tm.1;
                    is_rgb = tm.2;
                    if tm.3 > 0 && valid_bits == 0 {
                        bits = tm.3;
                    }
                }
            }
        }

        // size_x/size_y fallback if axis codes didn't yield them.
        if size_x == 0 || size_y == 0 {
            if let Some(first) = tiffs.first() {
                if let Ok(bytes) = self.source.read_bytes(first) {
                    if let Some((_, _, _, _, w, h)) = probe_tiff_dims(&bytes) {
                        if size_x == 0 {
                            size_x = w;
                        }
                        if size_y == 0 {
                            size_y = h;
                        }
                    }
                }
            }
        }
        if size_x == 0 || size_y == 0 {
            return Err(BioFormatsError::Format(format!(
                "OIF/OIB: invalid image dimensions {size_x}x{size_y}"
            )));
        }
        let _images_per_file = if tiffs.is_empty() {
            0
        } else if image_count % tiffs.len() == 0 {
            image_count / tiffs.len()
        } else {
            return Err(BioFormatsError::Format(format!(
                "OIF/OIB: image count {image_count} is not divisible by {} TIFF file(s)",
                tiffs.len()
            )));
        };
        let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
        meta_map.insert(
            "format".into(),
            MetadataValue::String("Olympus FV1000".into()),
        );

        self.meta = Some(ImageMetadata {
            size_x,
            size_y,
            size_z,
            size_c,
            size_t,
            pixel_type,
            bits_per_pixel: bits,
            image_count: image_count as u32,
            dimension_order,
            is_rgb,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian,
            resolution_count: 1,
            thumbnail: false,
            series_metadata: meta_map,
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        });
        self.tiffs = tiffs;
        self.planes = planes;
        Ok(())
    }

    /// Read a plane's bytes from its resolved TIFF (disk or OLE2 stream).
    fn read_plane(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let n_files = self.tiffs.len().max(1);
        let images_per_file = (meta.image_count as usize / n_files).max(1);
        let file = (plane_index as usize) / images_per_file;
        let image = (plane_index as usize) % images_per_file;

        let tiff_name = self
            .tiffs
            .get(file)
            .ok_or_else(|| {
                BioFormatsError::Format(format!("OIF/OIB: no TIFF for plane {plane_index}"))
            })?
            .clone();

        match &self.source {
            FileSource::Disk => {
                let mut reader = TiffReader::new();
                if reader.set_id(Path::new(&tiff_name)).is_err() {
                    return fv1000_blank_plane(meta);
                }
                let inner = reader.metadata().image_count.max(1);
                if image as u32 >= inner {
                    return fv1000_blank_plane(meta);
                }
                reader.open_bytes(image as u32)
            }
            FileSource::Oib { .. } => {
                // Read the embedded TIFF into a temp file, then parse it. The
                // TiffReader requires a path; OIB embeds full TIFF streams.
                let bytes = match self.source.read_bytes(&tiff_name) {
                    Ok(bytes) => bytes,
                    Err(_) => return fv1000_blank_plane(meta),
                };
                let mut tmp = std::env::temp_dir();
                tmp.push(format!(
                    "bioformats_oib_{}_{}.tif",
                    std::process::id(),
                    plane_index
                ));
                std::fs::write(&tmp, &bytes).map_err(BioFormatsError::Io)?;
                let mut reader = TiffReader::new();
                let r = reader.set_id(&tmp).and_then(|_| {
                    let inner = reader.metadata().image_count.max(1);
                    if image as u32 >= inner {
                        return fv1000_blank_plane(meta);
                    }
                    reader.open_bytes(image as u32)
                });
                let _ = std::fs::remove_file(&tmp);
                r
            }
        }
    }
}

fn fv1000_blank_plane(meta: &ImageMetadata) -> Result<Vec<u8>> {
    let channels = if meta.is_rgb { meta.size_c.max(1) } else { 1 };
    let len = meta
        .size_x
        .checked_mul(meta.size_y)
        .and_then(|px| px.checked_mul(channels))
        .and_then(|px| px.checked_mul(meta.pixel_type.bytes_per_sample() as u32))
        .ok_or_else(|| BioFormatsError::Format("OIF/OIB: blank plane size overflows".into()))?
        as usize;
    Ok(vec![0; len])
}

impl Default for Fv1000Reader {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse the OibInfo.txt mapping. Mirrors Java mapOIBFiles: lines are sorted,
/// `Storage*` keys define a directory prefix, `Stream*` keys map a logical file
/// name to its CFB stream path. Returns (oif_name, mapping).
fn map_oib_files(info_text: &str) -> (Option<String>, HashMap<String, String>) {
    let mut lines: Vec<String> = info_text
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    lines.sort();

    let mut mapping: HashMap<String, String> = HashMap::new();
    let mut oif_name: Option<String> = None;
    let mut directory_key: Option<String> = None;
    let mut directory_value: Option<String> = None;

    for line in &lines {
        let Some(eq) = line.find('=') else { continue };
        let key = line[..eq].to_string();
        let mut value = line[eq + 1..].to_string();

        if let (Some(dk), Some(dv)) = (&directory_key, &directory_value) {
            value = value.replace(dk.as_str(), dv.as_str());
        }
        value = remove_gst(&value);

        if key.starts_with("Stream") {
            value = sanitize_value(&value);
            if value.to_ascii_lowercase().ends_with(".oif") {
                oif_name = Some(value.clone());
            }
            let stream_path = match (&directory_key, &directory_value) {
                (Some(dk), Some(dv)) if value.starts_with(dv.as_str()) => {
                    format!("Root Entry/{dk}/{key}")
                }
                _ => format!("Root Entry/{key}"),
            };
            mapping.insert(value, stream_path);
        } else if key.starts_with("Storage") {
            directory_key = Some(key);
            directory_value = Some(value);
        }
    }

    (oif_name, mapping)
}

/// Java sanitizeFile: sanitize + prepend path prefix.
fn sanitize_file(file: &str, path: &str) -> String {
    let f = sanitize_value(file);
    if path.is_empty() {
        return f;
    }
    if path.ends_with('/') {
        format!("{path}{f}")
    } else {
        format!("{path}/{f}")
    }
}

fn parse_dimension_order(s: &str) -> DimensionOrder {
    // s starts with "XY".
    let rest: Vec<char> = s.chars().skip(2).collect();
    match (rest.first(), rest.get(1), rest.get(2)) {
        (Some('C'), Some('Z'), Some('T')) => DimensionOrder::XYCZT,
        (Some('C'), Some('T'), Some('Z')) => DimensionOrder::XYCTZ,
        (Some('Z'), Some('C'), Some('T')) => DimensionOrder::XYZCT,
        (Some('Z'), Some('T'), Some('C')) => DimensionOrder::XYZTC,
        (Some('T'), Some('C'), Some('Z')) => DimensionOrder::XYTCZ,
        (Some('T'), Some('Z'), Some('C')) => DimensionOrder::XYTZC,
        _ => DimensionOrder::XYCZT,
    }
}

/// Find the companion `.files/` (or stem) directory for an `.oif`.
fn find_companion_dir(oif_path: &Path) -> Option<PathBuf> {
    let parent = oif_path.parent()?;
    // Java FV1000Reader uses "<full .oif filename>.files" (e.g.
    // "foo.oif.files"); fall back to "<stem>.files" and "<stem>".
    if let Some(name) = oif_path.file_name() {
        let d0 = parent.join(format!("{}.files", name.to_string_lossy()));
        if d0.is_dir() {
            return Some(d0);
        }
    }
    let stem = oif_path.file_stem()?;
    let d1 = parent.join(format!("{}.files", stem.to_string_lossy()));
    if d1.is_dir() {
        return Some(d1);
    }
    let d2 = parent.join(stem);
    if d2.is_dir() {
        return Some(d2);
    }
    None
}

fn find_oif_for_entry(path: &Path) -> Option<PathBuf> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    if matches!(ext.as_deref(), Some("oif")) {
        return path.exists().then(|| path.to_path_buf());
    }
    if matches!(ext.as_deref(), Some("oib") | Some("bmp")) {
        return None;
    }

    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut prefix = path.file_stem()?.to_string_lossy().to_string();
    loop {
        for name in [format!("{prefix}.oif"), format!("{prefix}.OIF")] {
            let candidate = dir.join(name);
            if candidate.exists() && !candidate.is_dir() {
                return Some(candidate);
            }
        }
        match prefix.rfind('_') {
            Some(i) => prefix.truncate(i),
            None => break,
        }
    }
    None
}

/// Probe a TIFF held in memory: returns (pixel_type, little_endian, rgb, bits).
fn probe_tiff(bytes: &[u8]) -> Option<(PixelType, bool, bool, u8)> {
    let mut tmp = std::env::temp_dir();
    tmp.push(format!("bioformats_oib_probe_{}.tif", rand_suffix(bytes)));
    std::fs::write(&tmp, bytes).ok()?;
    let mut r = TiffReader::new();
    let res = r.set_id(&tmp).ok().map(|_| {
        let tm = r.metadata();
        (
            tm.pixel_type,
            tm.is_little_endian,
            tm.is_rgb,
            tm.bits_per_pixel,
        )
    });
    let _ = std::fs::remove_file(&tmp);
    res
}

/// Probe a TIFF for dimensions only: (pt, le, rgb, bits, width, height).
fn probe_tiff_dims(bytes: &[u8]) -> Option<(PixelType, bool, bool, u8, u32, u32)> {
    let mut tmp = std::env::temp_dir();
    tmp.push(format!("bioformats_oib_dims_{}.tif", rand_suffix(bytes)));
    std::fs::write(&tmp, bytes).ok()?;
    let mut r = TiffReader::new();
    let res = r.set_id(&tmp).ok().map(|_| {
        let tm = r.metadata();
        (
            tm.pixel_type,
            tm.is_little_endian,
            tm.is_rgb,
            tm.bits_per_pixel,
            tm.size_x,
            tm.size_y,
        )
    });
    let _ = std::fs::remove_file(&tmp);
    res
}

fn rand_suffix(bytes: &[u8]) -> u64 {
    // cheap, deterministic-ish unique suffix
    let pid = std::process::id() as u64;
    let len = bytes.len() as u64;
    let h = bytes
        .iter()
        .take(16)
        .fold(0u64, |a, &b| a.wrapping_mul(31).wrapping_add(b as u64));
    pid ^ (len << 16) ^ h
}

impl FormatReader for Fv1000Reader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        match ext.as_deref() {
            Some("oif") | Some("oib") => true,
            Some("bmp") => false,
            _ => find_oif_for_entry(path).is_some(),
        }
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // Only sniff the OIF text variant by magic. The OIB variant is an OLE2
        // compound document whose magic is shared with ZVI/XRM and other CFB
        // formats; matching it here would cause this reader to intercept those
        // files. OIB is instead detected via its `.oib` extension (Java
        // FV1000Reader.isThisType relies on the suffix for OIB), so non-OIB
        // OLE2 readers get the first attempt during the magic pass.
        let s = std::str::from_utf8(&header[..header.len().min(1024)]).unwrap_or("");
        s.contains("FileInformation")
            || s.contains("Acquisition Parameters")
            || s.contains("[File Info]")
            || s.contains("[Version Info]")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        match ext.as_deref() {
            Some("oib") => self.init_oib(path),
            Some("oif") => self.init_oif(path),
            _ => {
                let oif = find_oif_for_entry(path).ok_or_else(|| {
                    BioFormatsError::Format(format!(
                        "OIF/OIB: could not find .oif file for {}",
                        path.display()
                    ))
                })?;
                self.init_oif(&oif)
            }
        }
    }

    fn close(&mut self) -> Result<()> {
        self.source = FileSource::Disk;
        self.path_prefix.clear();
        self.meta = None;
        self.tiffs.clear();
        self.planes.clear();
        self.physical_size_x = None;
        self.physical_size_y = None;
        self.channels.clear();
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
        self.read_plane(plane_index)
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
        let meta = self.meta.as_ref().unwrap();
        crate::formats::leica::crop_region(&full, meta, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = (
            (meta.size_x.saturating_sub(tw)) / 2,
            (meta.size_y.saturating_sub(th)) / 2,
        );
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        use crate::common::ome_metadata::OmeMetadata;
        let meta = self.meta.as_ref()?;
        let mut ome = OmeMetadata::from_image_metadata(meta);
        let img = ome.images.get_mut(0)?;

        // Image name is always "Series N" (FV1000Reader ~986).
        img.name = Some("Series 1".to_string());

        // Physical pixel size: WidthConvertValue / HeightConvertValue (µm).
        if let Some(x) = self.physical_size_x {
            img.physical_size_x = Some(x);
        }
        if let Some(y) = self.physical_size_y {
            img.physical_size_y = Some(y);
        }
        // Z size defaults to 1.0 µm (FV1000Reader ~1022-1036).
        img.physical_size_z = Some(1.0);

        // Active channels in acquisition order map to channelIndex 0..sizeC.
        for (c, channel) in img.channels.iter_mut().enumerate() {
            if let Some(src) = self.channels.get(c) {
                channel.name = src.name.clone();
                channel.emission_wavelength = src.emission;
                channel.excitation_wavelength = src.excitation;
            }
        }

        Some(ome)
    }
}
// ===========================================================================
// OlympusTileReader — Olympus `.omp2info` tiled-acquisition reader.
//
// Faithful port of java-bioformats
// components/formats-gpl/src/loci/formats/in/OlympusTileReader.java.
//
// The `.omp2info` file is a small XML document describing a grid of image
// tiles. Each `matl:area` element names a tile image file (an Olympus `.oir`
// or `.vsi`) and its (xIndex, yIndex) position in the mosaic. The reader
// delegates all pixel access to a "helper" reader chosen by tile suffix:
//   - `.oir`  -> crate::formats::flim2::OirReader   (Java OIRReader)
//   - `.vsi`  -> crate::formats::flim2::CellSensReader (Java CellSensReader)
// openBytes stitches the requested region together from the intersecting
// tiles, exactly as the Java implementation does.
// ===========================================================================

/// A read-only DOM node, mirroring the `org.w3c.dom.Element`/`Node` operations
/// the Java reader relies on (`getElementsByTagName`, `getTextContent`,
/// `getAttribute`, attribute/child traversal).
#[derive(Default, Clone)]
struct DomNode {
    /// Qualified element name as written (e.g. `matl:area`).
    name: String,
    /// Concatenated direct text content (Java getTextContent on a leaf-ish node).
    text: String,
    attrs: Vec<(String, String)>,
    children: Vec<DomNode>,
}

impl DomNode {
    /// Java `Element.getAttribute(name)` — returns "" when absent.
    fn attribute(&self, name: &str) -> String {
        self.attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    }

    /// Java `Element.getElementsByTagName(name)` — every *descendant* (not just
    /// direct child) element with the given qualified name, in document order.
    fn elements_by_tag_name<'a>(&'a self, name: &str, out: &mut Vec<&'a DomNode>) {
        for child in &self.children {
            if child.name == name {
                out.push(child);
            }
            child.elements_by_tag_name(name, out);
        }
    }
}

/// Java `OlympusTileReader.Tile` — one mosaic tile.
#[derive(Clone)]
struct Tile {
    file: String,
    files: Vec<String>,
    region: Region,
}

impl Tile {
    /// Java `Tile.compareTo`: sort by y, then x.
    fn compare_to(&self, o: &Tile) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        if self.region == o.region {
            return Ordering::Equal;
        }
        let y_diff = self.region.y - o.region.y;
        if y_diff != 0 {
            return y_diff.cmp(&0);
        }
        (self.region.x - o.region.x).cmp(&0)
    }
}

/// Port of `loci.common.Region` (the subset used here).
#[derive(Clone, Copy, PartialEq, Eq)]
struct Region {
    x: i64,
    y: i64,
    width: i64,
    height: i64,
}

impl Region {
    fn new(x: i64, y: i64, width: i64, height: i64) -> Region {
        Region {
            x,
            y,
            width,
            height,
        }
    }

    /// Java `Region.intersects`.
    fn intersects(&self, other: &Region) -> bool {
        let tw = self.width;
        let th = self.height;
        let rw = other.width;
        let rh = other.height;
        if rw <= 0 || rh <= 0 || tw <= 0 || th <= 0 {
            return false;
        }
        let tx = self.x;
        let ty = self.y;
        let rx = other.x;
        let ry = other.y;
        let rw = rx + rw;
        let rh = ry + rh;
        let tw = tx + tw;
        let th = ty + th;
        (rw < rx || rw > tx) && (rh < ry || rh > ty) && (tw < tx || tw > rx) && (th < ty || th > ry)
    }

    /// Java `Region.intersection`.
    fn intersection(&self, other: &Region) -> Region {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let w = (self.x + self.width).min(other.x + other.width) - x;
        let h = (self.y + self.height).min(other.y + other.height) - y;
        Region::new(x, y, w.max(0), h.max(0))
    }
}

/// The pixel-data helper reader, chosen by tile suffix (Java `helperReader`).
enum TileHelper {
    /// `.oir` tiles — Java `OIRReader`.
    Oir(Box<crate::formats::flim2::OirReader>),
    /// `.vsi` tiles — Java `CellSensReader`.
    CellSens(Box<crate::formats::flim2::CellSensReader>),
}

impl TileHelper {
    fn set_id(&mut self, file: &str) -> Result<()> {
        match self {
            TileHelper::Oir(r) => r.set_id(Path::new(file)),
            TileHelper::CellSens(r) => r.set_id(Path::new(file)),
        }
    }

    fn metadata(&self) -> &ImageMetadata {
        match self {
            TileHelper::Oir(r) => r.metadata(),
            TileHelper::CellSens(r) => r.metadata(),
        }
    }

    fn open_bytes_region(&mut self, no: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        match self {
            TileHelper::Oir(r) => r.open_bytes_region(no, x, y, w, h),
            TileHelper::CellSens(r) => r.open_bytes_region(no, x, y, w, h),
        }
    }

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        match self {
            TileHelper::Oir(r) => r.ome_metadata(),
            TileHelper::CellSens(r) => r.ome_metadata(),
        }
    }

    fn used_files(&self, fallback: &str) -> Vec<String> {
        match self {
            TileHelper::Oir(r) => {
                let files = r.series_used_files();
                if files.is_empty() {
                    vec![fallback.to_string()]
                } else {
                    files
                        .into_iter()
                        .map(|p| p.to_string_lossy().to_string())
                        .collect()
                }
            }
            TileHelper::CellSens(_) => vec![fallback.to_string()],
        }
    }

    fn close(&mut self) -> Result<()> {
        match self {
            TileHelper::Oir(r) => r.close(),
            TileHelper::CellSens(r) => r.close(),
        }
    }
}

/// Olympus `.omp2info` reader.
pub struct OlympusTileReader {
    current_id: Option<PathBuf>,
    helper_reader: Option<TileHelper>,
    tiles: Vec<Tile>,
    all_pixels_files: Option<Vec<String>>,
    extra_files: Vec<String>,
    meta: Option<ImageMetadata>,
    /// Per-tile physical sizes (µm) carried over from the helper for OME.
    physical_size_x: Option<f64>,
    physical_size_y: Option<f64>,
    /// Global metadata accumulated by `read_metadata` (Java addGlobalMeta*).
    global_meta: HashMap<String, MetadataValue>,
}

impl OlympusTileReader {
    pub fn new() -> Self {
        OlympusTileReader {
            current_id: None,
            helper_reader: None,
            tiles: Vec::new(),
            all_pixels_files: None,
            extra_files: Vec::new(),
            meta: None,
            physical_size_x: None,
            physical_size_y: None,
            global_meta: HashMap::new(),
        }
    }

    /// Java `getCurrentFile()`.
    fn get_current_file(&self) -> &Path {
        self.current_id.as_deref().unwrap_or_else(|| Path::new(""))
    }

    /// Java `initFile(String)`.
    fn init_file(&mut self, id: &Path) -> Result<()> {
        self.current_id = Some(id.to_path_buf());

        let xml = decode_text(&std::fs::read(id).map_err(BioFormatsError::Io)?);
        let xml = sanitize_xml(&xml);
        self.read_metadata(&xml)?;

        // tiles.sort(null) — Java natural ordering via Tile.compareTo.
        self.tiles.sort_by(|a, b| a.compare_to(b));

        if self.tiles.is_empty() {
            return Err(BioFormatsError::Format(
                "Olympus .omp2info references no tiles".into(),
            ));
        }

        // helperReader.setId(tiles.get(0).file); core comes from helper plane 0.
        let first_file = self.tiles[0].file.clone();
        let helper = self
            .helper_reader
            .as_mut()
            .ok_or_else(|| BioFormatsError::Format("Olympus .omp2info: no helper reader".into()))?;
        helper.set_id(&first_file)?;

        // CoreMetadata copied from helper, with sizeX/sizeY grown to cover tiles.
        let mut ms = helper.metadata().clone();
        let mut size_x = ms.size_x as i64;
        let mut size_y = ms.size_y as i64;
        for t in &self.tiles {
            let r = &t.region;
            size_x = size_x.max(r.width + r.x);
            size_y = size_y.max(r.height + r.y);
        }
        ms.size_x = size_x.max(0) as u32;
        ms.size_y = size_y.max(0) as u32;

        // Carry physical pixel sizes from the helper's OME for our own OME.
        if let Some(ome) = helper.ome_metadata() {
            if let Some(img) = ome.images.first() {
                self.physical_size_x = img.physical_size_x;
                self.physical_size_y = img.physical_size_y;
            }
        }

        self.meta = Some(ms);
        Ok(())
    }

    /// Java `getMetadataRoot(String)`.
    fn get_metadata_root(&self, xml: &str) -> Result<DomNode> {
        parse_dom(xml)
            .ok_or_else(|| BioFormatsError::Format("Olympus .omp2info: malformed XML".into()))
    }

    /// Java `getChildNode(Element, String)` — first descendant with that name.
    fn get_child_node<'a>(&self, root: &'a DomNode, name: &str) -> Option<&'a DomNode> {
        let mut out = Vec::new();
        root.elements_by_tag_name(name, &mut out);
        out.into_iter().next()
    }

    /// Java `getChildValue(Element, String)`.
    fn get_child_value(&self, root: &DomNode, name: &str) -> Option<String> {
        self.get_child_node(root, name).map(|n| n.text.clone())
    }

    /// Java `getName(Node)` — strip the namespace prefix.
    fn get_name(&self, name: &str) -> String {
        match name.find(':') {
            Some(i) => name[i + 1..].to_string(),
            None => name.to_string(),
        }
    }

    /// Java `readMetadata(String)`.
    fn read_metadata(&mut self, xml: &str) -> Result<()> {
        let parent_dir = self
            .get_current_file()
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));

        let root = self.get_metadata_root(xml)?;

        let tile_group = self
            .get_child_node(&root, "matl:group")
            .cloned()
            .ok_or_else(|| BioFormatsError::Format("Olympus .omp2info: no matl:group".into()))?;
        let region_info = self.get_child_node(&tile_group, "marker:regionInfo");
        let coordinates = region_info.and_then(|ri| self.get_child_node(ri, "marker:coordinates"));

        let area_info = self
            .get_child_node(&tile_group, "matl:areaInfo")
            .ok_or_else(|| BioFormatsError::Format("Olympus .omp2info: no matl:areaInfo".into()))?;
        let rows: i64 = self
            .get_child_value(area_info, "matl:numOfYAreas")
            .and_then(|s| s.trim().parse().ok())
            .ok_or_else(|| BioFormatsError::Format("Olympus .omp2info: bad numOfYAreas".into()))?;
        let cols: i64 = self
            .get_child_value(area_info, "matl:numOfXAreas")
            .and_then(|s| s.trim().parse().ok())
            .ok_or_else(|| BioFormatsError::Format("Olympus .omp2info: bad numOfXAreas".into()))?;

        // nanometers
        let mut stitched_width: Option<f64> = None;
        let mut stitched_height: Option<f64> = None;
        if let Some(coords) = coordinates {
            stitched_width = parse_double(&coords.attribute("width"));
            stitched_height = parse_double(&coords.attribute("height"));
        }

        let mut all_tiles = Vec::new();
        tile_group.elements_by_tag_name("matl:area", &mut all_tiles);

        let mut adjust_width: i64 = 0;
        let mut adjust_height: i64 = 0;
        let stage = self.get_child_node(&root, "matl:stage").cloned();
        let mut stage_overlap: i64 = 0;
        if let Some(stage) = &stage {
            stage_overlap = self
                .get_child_value(stage, "matl:overlap")
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(0);
        }

        for tile in &all_tiles {
            let tile_file_rel = self.get_child_value(tile, "matl:image").ok_or_else(|| {
                BioFormatsError::Format("Olympus .omp2info: tile without image".into())
            })?;
            let tile_file = parent_dir.join(&tile_file_rel);
            let tile_file = tile_file.to_string_lossy().to_string();

            if self.helper_reader.is_none() {
                // Choose the helper by tile suffix (Java OIRReader/CellSensReader).
                if check_suffix(&tile_file, "oir") {
                    self.helper_reader = Some(TileHelper::Oir(Box::new(
                        crate::formats::flim2::OirReader::new(),
                    )));
                } else if check_suffix(&tile_file, "vsi") {
                    self.helper_reader = Some(TileHelper::CellSens(Box::new(
                        crate::formats::flim2::CellSensReader::new(),
                    )));
                } else {
                    return Err(BioFormatsError::Format(format!(
                        "Unsupported tile file {tile_file}"
                    )));
                }

                let helper = self.helper_reader.as_mut().unwrap();
                helper.set_id(&tile_file)?;

                let helper_meta = helper.metadata().clone();
                let width_with_overlaps = helper_meta.size_x as i64 * cols;
                let height_with_overlaps = helper_meta.size_y as i64 * rows;

                // physicalSizeX/Y in nanometers (OME stores micrometers).
                let physical_size_x = helper
                    .ome_metadata()
                    .and_then(|o| o.images.first().and_then(|i| i.physical_size_x))
                    .map(|um| um * 1000.0);
                let physical_size_y = helper
                    .ome_metadata()
                    .and_then(|o| o.images.first().and_then(|i| i.physical_size_y))
                    .map(|um| um * 1000.0);

                let mut diff_x = stage_overlap * cols * 4;
                let mut diff_y = stage_overlap * rows * 4;

                if let (Some(sw), Some(sh), Some(px), Some(py)) = (
                    stitched_width,
                    stitched_height,
                    physical_size_x,
                    physical_size_y,
                ) {
                    let actual_width = (sw / px) as i64;
                    let actual_height = (sh / py) as i64;
                    diff_x = width_with_overlaps - actual_width;
                    diff_y = height_with_overlaps - actual_height;
                }

                adjust_width = helper_meta.size_x as i64;
                if cols > 1 {
                    adjust_width -= diff_x / (cols - 1);
                }
                adjust_height = helper_meta.size_y as i64;
                if rows > 1 {
                    adjust_height -= diff_y / (rows - 1);
                }
            } else {
                self.helper_reader.as_mut().unwrap().set_id(&tile_file)?;
            }

            let helper = self.helper_reader.as_mut().unwrap();
            // Java: currentTile.files = helperReader.getUsedFiles().
            let files = helper.used_files(&tile_file);
            let helper_meta = helper.metadata().clone();

            let x_index: i64 = self
                .get_child_value(tile, "matl:xIndex")
                .and_then(|s| s.trim().parse().ok())
                .ok_or_else(|| BioFormatsError::Format("Olympus .omp2info: bad xIndex".into()))?;
            let y_index: i64 = self
                .get_child_value(tile, "matl:yIndex")
                .and_then(|s| s.trim().parse().ok())
                .ok_or_else(|| BioFormatsError::Format("Olympus .omp2info: bad yIndex".into()))?;

            let region = Region::new(
                x_index * adjust_width,
                y_index * adjust_height,
                helper_meta.size_x as i64,
                helper_meta.size_y as i64,
            );
            self.tiles.push(Tile {
                file: tile_file,
                files,
                region,
            });

            self.add_global_meta_list("tile X index", MetadataValue::Int(x_index));
            self.add_global_meta_list("tile Y index", MetadataValue::Int(y_index));
            self.add_global_meta_list(
                "tile bounding box (pixels)",
                MetadataValue::String(format!(
                    "x={}, y={}, w={}, h={}",
                    region.x, region.y, region.width, region.height
                )),
            );
        }
        if let Some(helper) = self.helper_reader.as_mut() {
            helper.close()?;
        }

        if let Some(stage) = &stage {
            self.parse_original_metadata(stage);
        }

        if let Some(cycle) = self.get_child_node(&root, "matl:cycle").cloned() {
            self.parse_original_metadata(&cycle);
        }

        if let Some(map) = self.get_child_node(&root, "matl:map") {
            if let Some(map_file) = self.get_child_value(map, "matl:image") {
                let map_file = parent_dir.join(&map_file);
                self.extra_files
                    .push(map_file.to_string_lossy().to_string());
            }
        }

        Ok(())
    }

    /// Java `parseOriginalMetadata(Node)` — recursively flatten attributes and
    /// text into the global metadata table.
    fn parse_original_metadata(&mut self, node: &DomNode) {
        self.parse_original_metadata_node(node, None);
    }

    fn parse_original_metadata_node(&mut self, node: &DomNode, parent: Option<&str>) {
        let value = node.text.trim();
        if !value.is_empty() {
            let node_name = self.get_name(&node.name);
            let key = parent
                .map(|parent| format!("{} {}", self.get_name(parent), node_name))
                .unwrap_or(node_name);
            self.add_global_meta(&key, MetadataValue::String(value.to_string()));
        }
        for (k, v) in &node.attrs {
            let key = format!("{} {}", self.get_name(&node.name), k);
            self.add_global_meta(&key, MetadataValue::String(v.clone()));
        }
        for child in &node.children {
            self.parse_original_metadata_node(child, Some(&node.name));
        }
    }

    /// Java `addGlobalMeta`.
    fn add_global_meta(&mut self, key: &str, value: MetadataValue) {
        self.global_meta.insert(key.to_string(), value);
    }

    /// Java `addGlobalMetaList` — append-with-index so repeated keys survive.
    fn add_global_meta_list(&mut self, key: &str, value: MetadataValue) {
        let idx = self
            .global_meta
            .keys()
            .filter(|k| k.starts_with(key))
            .count();
        self.global_meta.insert(format!("{key} {idx}"), value);
    }

    /// Java `getSeriesUsedFiles(boolean noPixels)`.
    pub fn get_series_used_files(&mut self, no_pixels: bool) -> Vec<String> {
        let current = self
            .current_id
            .as_ref()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        if no_pixels {
            let mut all = vec![current];
            all.extend(self.extra_files.iter().cloned());
            return all;
        }
        if self.all_pixels_files.is_none() {
            let mut all = vec![current];
            all.extend(self.extra_files.iter().cloned());
            for t in &self.tiles {
                for f in &t.files {
                    all.push(f.clone());
                }
            }
            self.all_pixels_files = Some(all);
        }
        self.all_pixels_files.clone().unwrap_or_default()
    }

    /// Read-only view of the accumulated global metadata (Java getGlobalMetadata).
    pub fn global_metadata(&self) -> &HashMap<String, MetadataValue> {
        &self.global_meta
    }
}

impl Default for OlympusTileReader {
    fn default() -> Self {
        Self::new()
    }
}

/// Java `XMLTools.sanitizeXML` (the subset relevant here): strip control
/// characters that are illegal in XML 1.0 so the parser does not choke.
fn sanitize_xml(s: &str) -> String {
    s.chars()
        .filter(|&c| {
            c == '\t'
                || c == '\n'
                || c == '\r'
                || (c >= '\u{20}' && c <= '\u{D7FF}')
                || (c >= '\u{E000}' && c <= '\u{FFFD}')
                || c >= '\u{10000}'
        })
        .collect()
}

/// Java `DataTools.parseDouble`.
fn parse_double(s: &str) -> Option<f64> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        t.parse::<f64>().ok()
    }
}

/// Java `FormatReader.checkSuffix(name, suffix)`.
fn check_suffix(name: &str, suffix: &str) -> bool {
    name.to_ascii_lowercase()
        .ends_with(&format!(".{}", suffix.to_ascii_lowercase()))
}

/// Build a read-only DOM from XML, returning the document element (Java
/// `XMLTools.parseDOM(xml).getDocumentElement()`).
fn parse_dom(xml: &str) -> Option<DomNode> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(false);

    fn qualified(name: &[u8]) -> String {
        String::from_utf8_lossy(name).to_string()
    }
    fn collect_attrs(e: &quick_xml::events::BytesStart) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for a in e.attributes().flatten() {
            let k = qualified(a.key.as_ref());
            let v = a
                .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                .map(|c| c.into_owned())
                .unwrap_or_else(|_| String::from_utf8_lossy(&a.value).into_owned());
            out.push((k, v));
        }
        out
    }

    // Synthetic document root; its single child is the document element.
    let mut stack: Vec<DomNode> = vec![DomNode {
        name: "#document".to_string(),
        ..Default::default()
    }];
    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) => {
                stack.push(DomNode {
                    name: qualified(e.name().as_ref()),
                    attrs: collect_attrs(e),
                    ..Default::default()
                });
            }
            Ok(Event::Empty(ref e)) => {
                let n = DomNode {
                    name: qualified(e.name().as_ref()),
                    attrs: collect_attrs(e),
                    ..Default::default()
                };
                if let Some(parent) = stack.last_mut() {
                    parent.children.push(n);
                }
            }
            Ok(Event::Text(ref t)) => {
                if let Some(s) = crate::common::xml::decode_xml_text(t) {
                    if let Some(top) = stack.last_mut() {
                        top.text.push_str(&s);
                    }
                }
            }
            Ok(Event::GeneralRef(ref r)) => {
                if let Some(s) = crate::common::xml::decode_xml_ref(r) {
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
                    let mut node = stack.pop().unwrap();
                    node.text = node.text.trim().to_string();
                    if let Some(parent) = stack.last_mut() {
                        parent.children.push(node);
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => return None,
            _ => {}
        }
    }
    let doc = stack.pop()?;
    doc.children.into_iter().next()
}

impl FormatReader for OlympusTileReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        check_suffix(&path.to_string_lossy(), "omp2info")
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // The `.omp2info` document is XML rooted at a `matl:` element. Sniff for
        // the Olympus tile-metadata namespace tokens.
        let s = std::str::from_utf8(&header[..header.len().min(1024)]).unwrap_or("");
        s.contains("matl:") || s.contains("marker:regionInfo")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        self.init_file(path)
    }

    fn close(&mut self) -> Result<()> {
        if let Some(helper) = self.helper_reader.as_mut() {
            helper.close()?;
        }
        self.helper_reader = None;
        self.tiles.clear();
        self.all_pixels_files = None;
        self.extra_files.clear();
        self.current_id = None;
        self.meta = None;
        self.physical_size_x = None;
        self.physical_size_y = None;
        self.global_meta.clear();
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

    /// Java `openBytes(int no, byte[] buf, int x, int y, int w, int h)`.
    fn open_bytes_region(&mut self, no: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let pixel = {
            let rgb = if meta.is_rgb { meta.size_c.max(1) } else { 1 } as usize;
            rgb * meta.pixel_type.bytes_per_sample()
        };
        let w_us = w as usize;
        let h_us = h as usize;
        let mut buf = vec![0u8; w_us * h_us * pixel];

        let image_region = Region::new(x as i64, y as i64, w as i64, h as i64);

        // Take ownership of tiles to avoid borrowing self while mutating helper.
        let tiles = std::mem::take(&mut self.tiles);
        let mut result: Result<()> = Ok(());
        for t in &tiles {
            if t.region.intersects(&image_region) {
                let helper = match self.helper_reader.as_mut() {
                    Some(h) => h,
                    None => {
                        result = Err(BioFormatsError::NotInitialized);
                        break;
                    }
                };
                if let Err(e) = helper.set_id(&t.file) {
                    result = Err(e);
                    break;
                }

                let intersection = t.region.intersection(&image_region);
                let src = match helper.open_bytes_region(
                    no,
                    (intersection.x - t.region.x) as u32,
                    (intersection.y - t.region.y) as u32,
                    intersection.width as u32,
                    intersection.height as u32,
                ) {
                    Ok(s) => s,
                    Err(e) => {
                        result = Err(e);
                        break;
                    }
                };
                for row in 0..intersection.height as usize {
                    let src_index = row * intersection.width as usize * pixel;
                    let dest_index = pixel
                        * (((intersection.y - y as i64) as usize + row) * w_us
                            + (intersection.x - x as i64) as usize);
                    let len = intersection.width as usize * pixel;
                    if src_index + len <= src.len() && dest_index + len <= buf.len() {
                        buf[dest_index..dest_index + len]
                            .copy_from_slice(&src[src_index..src_index + len]);
                    }
                }
            }
        }
        self.tiles = tiles;
        result?;
        Ok(buf)
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let (w, h) = {
            let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
            (meta.size_x, meta.size_y)
        };
        self.open_bytes_region(plane_index, 0, 0, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = (
            (meta.size_x.saturating_sub(tw)) / 2,
            (meta.size_y.saturating_sub(th)) / 2,
        );
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        use crate::common::ome_metadata::OmeMetadata;
        let meta = self.meta.as_ref()?;
        let mut ome = OmeMetadata::from_image_metadata(meta);
        if let Some(img) = ome.images.get_mut(0) {
            if let Some(x) = self.physical_size_x {
                img.physical_size_x = Some(x);
            }
            if let Some(y) = self.physical_size_y {
                img.physical_size_y = Some(y);
            }
        }
        Some(ome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::pixel_type::PixelType;
    use crate::writer_registry::ImageWriter;

    fn temp_path(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "bioformats_olympus_test_{}_{}_{}",
            std::process::id(),
            nanos,
            name
        ))
    }

    #[test]
    fn ini_parses_sections_and_keys() {
        let text = "[File Info]\r\nDataName=\"image.tif\"\r\n[Axis 2 Parameters]\r\nNumber=3\r\n";
        let ini = IniList::parse(text);
        assert_eq!(
            ini.table("File Info")
                .and_then(|t| t.get("DataName"))
                .map(String::as_str),
            Some("image.tif")
        );
        assert_eq!(
            ini.table("Axis 2 Parameters")
                .and_then(|t| t.get("Number"))
                .map(String::as_str),
            Some("3")
        );
    }

    #[test]
    fn sanitize_value_strips_quotes_and_normalises_separators() {
        assert_eq!(sanitize_value("\"a\\b/c\""), "a/b/c");
    }

    #[test]
    fn replace_extension_swaps_pty_for_tif() {
        assert_eq!(replace_extension("s_C001.pty", "pty", "tif"), "s_C001.tif");
        assert_eq!(replace_extension("foo.bar", "pty", "tif"), "foo.bar");
    }

    #[test]
    fn preview_name_detection() {
        // Java isPreviewName: "-R" must sit exactly 9 chars from the end,
        // i.e. "-R" followed by 7 trailing characters.
        assert!(is_preview_name("img-R1234567"));
        assert!(!is_preview_name("plain.tif"));
        assert!(!is_preview_name("abcd-R12345")); // "-R" too close to the end
    }

    #[test]
    fn map_oib_files_resolves_oif_and_streams() {
        // A Storage line defines (directoryKey=Storage00001, directoryValue=dir);
        // subsequent Stream values have occurrences of the key replaced with the
        // value (Java mapOIBFiles), then are mapped to CFB stream paths.
        let info = "Storage00001=dir\nStream0000=dir/scan.oif\nStream0001=dir/s_C001.pty\n";
        let (oif, mapping) = map_oib_files(info);
        assert_eq!(oif.as_deref(), Some("dir/scan.oif"));
        assert!(mapping.contains_key("dir/scan.oif"));
        // Stream under the storage directory gets the directory key in its path.
        assert_eq!(
            mapping.get("dir/scan.oif").map(String::as_str),
            Some("Root Entry/Storage00001/Stream0000")
        );
    }

    #[test]
    fn dimension_order_parses_appended_axes() {
        assert!(matches!(
            parse_dimension_order("XYCZT"),
            DimensionOrder::XYCZT
        ));
        assert!(matches!(
            parse_dimension_order("XYZCT"),
            DimensionOrder::XYZCT
        ));
    }

    #[test]
    fn fv1000_detection_accepts_related_entries_like_java() {
        let root = temp_path("detectentry.oif");
        let dir = root.parent().unwrap();
        let stem = root.file_stem().unwrap().to_string_lossy();
        let tif = dir.join(format!("{stem}_C001.tif"));
        let pty = dir.join(format!("{stem}_C001.pty"));
        let bmp = dir.join(format!("{stem}_C001.bmp"));
        std::fs::write(&root, b"[FileInformation]\n").unwrap();
        std::fs::write(&tif, []).unwrap();
        std::fs::write(&pty, []).unwrap();
        std::fs::write(&bmp, []).unwrap();

        let reader = Fv1000Reader::new();
        assert!(reader.is_this_type_by_name(&root));
        assert!(reader.is_this_type_by_name(&tif));
        assert!(reader.is_this_type_by_name(&pty));
        assert!(!reader.is_this_type_by_name(&bmp));

        let _ = std::fs::remove_file(root);
        let _ = std::fs::remove_file(tif);
        let _ = std::fs::remove_file(pty);
        let _ = std::fs::remove_file(bmp);
    }

    #[test]
    fn fv1000_set_id_accepts_related_entries_like_java() {
        let root = temp_path("relatedentry.oif");
        let dir = root.parent().unwrap();
        let stem = root.file_stem().unwrap().to_string_lossy();
        let related_tif = dir.join(format!("{stem}_C001.tif"));
        let companion = root.with_file_name(format!("{stem}.files"));
        std::fs::create_dir_all(&companion).unwrap();

        let tiff = companion.join("plane0.tif");
        let mut tiff_meta = ImageMetadata::default();
        tiff_meta.size_x = 2;
        tiff_meta.size_y = 2;
        tiff_meta.pixel_type = PixelType::Uint8;
        tiff_meta.bits_per_pixel = 8;
        tiff_meta.image_count = 1;
        ImageWriter::save(&tiff, &tiff_meta, &[vec![1, 2, 3, 4]]).unwrap();

        std::fs::write(
            companion.join("plane0.pty"),
            "[File Info]\nDataName=plane0.tif\n",
        )
        .unwrap();
        std::fs::write(
            &root,
            "[FileInformation]\n[ProfileSaveInfo]\nIniFileName0=plane0.pty\n[Axis 0 Parameters Common]\nAxisCode=X\nMaxSize=2\n[Axis 1 Parameters Common]\nAxisCode=Y\nMaxSize=2\n[Reference Image Parameter]\nImageDepth=1\nValidBitCounts=8\n",
        )
        .unwrap();
        std::fs::write(&related_tif, []).unwrap();

        let mut reader = Fv1000Reader::new();
        reader.set_id(&related_tif).unwrap();
        assert_eq!(reader.metadata().size_x, 2);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4]);

        let _ = std::fs::remove_file(root);
        let _ = std::fs::remove_file(related_tif);
        let _ = std::fs::remove_dir_all(companion);
    }

    #[test]
    fn fv1000_bytes_detection_accepts_java_magic_strings() {
        let reader = Fv1000Reader::new();
        assert!(reader.is_this_type_by_bytes(b"prefix FileInformation suffix"));
        assert!(reader.is_this_type_by_bytes(b"prefix Acquisition Parameters suffix"));
    }

    #[test]
    fn fv1000_pty_acquisition_valid_bits_override_reference_bits() {
        let root = temp_path("validbits_override.oif");
        let companion = root.with_file_name(format!(
            "{}.files",
            root.file_stem().unwrap().to_string_lossy()
        ));
        std::fs::create_dir_all(&companion).unwrap();

        let tiff = companion.join("plane0.tif");
        let mut tiff_meta = ImageMetadata::default();
        tiff_meta.size_x = 1;
        tiff_meta.size_y = 1;
        tiff_meta.pixel_type = PixelType::Uint16;
        tiff_meta.bits_per_pixel = 16;
        tiff_meta.image_count = 1;
        ImageWriter::save(&tiff, &tiff_meta, &[vec![0, 0]]).unwrap();

        std::fs::write(
            companion.join("plane0.pty"),
            "[File Info]\nDataName=plane0.tif\n[Acquisition Parameters Common]\nValidBitCounts=10\n",
        )
        .unwrap();
        std::fs::write(
            &root,
            "[ProfileSaveInfo]\nIniFileName0=plane0.pty\n[Axis 0 Parameters Common]\nAxisCode=X\nMaxSize=1\n[Axis 1 Parameters Common]\nAxisCode=Y\nMaxSize=1\n[Reference Image Parameter]\nImageDepth=2\nValidBitCounts=12\n",
        )
        .unwrap();

        let mut reader = Fv1000Reader::new();
        reader.set_id(&root).unwrap();
        assert_eq!(reader.metadata().bits_per_pixel, 10);

        let _ = std::fs::remove_file(root);
        let _ = std::fs::remove_dir_all(companion);
    }

    #[test]
    fn oif_missing_physical_tiff_page_returns_blank_like_java() {
        let root = temp_path("repeat_plane.oif");
        let companion = root.with_file_name(format!(
            "{}.files",
            root.file_stem().unwrap().to_string_lossy()
        ));
        std::fs::create_dir_all(&companion).unwrap();

        let tiff = companion.join("plane0.tif");
        let mut tiff_meta = ImageMetadata::default();
        tiff_meta.size_x = 2;
        tiff_meta.size_y = 2;
        tiff_meta.pixel_type = PixelType::Uint8;
        tiff_meta.image_count = 1;
        ImageWriter::save(&tiff, &tiff_meta, &[vec![1, 2, 3, 4]]).unwrap();

        let pty = companion.join("plane0.pty");
        std::fs::write(
            &pty,
            "[File Info]\nDataName=plane0.tif\n[Axis 0 Parameters]\nNumber=1\n[Axis 1 Parameters]\nNumber=1\n[Axis 2 Parameters]\nNumber=2\n[Axis 3 Parameters]\nNumber=2\n",
        )
        .unwrap();

        std::fs::write(
            &root,
            "[ProfileSaveInfo]\nIniFileName0=plane0.pty\n[Axis 0 Parameters Common]\nAxisCode=X\nMaxSize=2\n[Axis 1 Parameters Common]\nAxisCode=Y\nMaxSize=2\n[Axis 2 Parameters Common]\nAxisCode=C\nMaxSize=2\n[Axis 3 Parameters Common]\nAxisCode=Z\nMaxSize=2\n[Reference Image Parameter]\nImageDepth=1\nValidBitCounts=8\n",
        )
        .unwrap();

        let mut reader = Fv1000Reader::new();
        reader.set_id(&root).unwrap();
        assert_eq!(reader.metadata().image_count, 4);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4]);
        assert_eq!(reader.open_bytes(1).unwrap(), vec![0, 0, 0, 0]);

        let _ = std::fs::remove_file(root);
        let _ = std::fs::remove_file(pty);
        let _ = std::fs::remove_file(tiff);
        let _ = std::fs::remove_dir(companion);
    }

    // -- OlympusTileReader tests --

    /// Write a single-plane 8-bit TIFF body to `path` (any extension). The TIFF
    /// is produced via the registered `.tif` writer, then copied to the desired
    /// name so the Olympus helper picks it up by its `.oir` suffix.
    fn write_tile_tiff(path: &Path, w: u32, h: u32, fill: u8) {
        let mut meta = ImageMetadata::default();
        meta.size_x = w;
        meta.size_y = h;
        meta.size_c = 1;
        meta.size_z = 1;
        meta.size_t = 1;
        meta.image_count = 1;
        meta.pixel_type = PixelType::Uint8;
        meta.bits_per_pixel = 8;
        let plane = vec![fill; (w * h) as usize];

        let tif = path.with_extension("tif");
        ImageWriter::save(&tif, &meta, &[plane]).unwrap();
        let bytes = std::fs::read(&tif).unwrap();
        std::fs::write(path, &bytes).unwrap();
        let _ = std::fs::remove_file(&tif);
    }

    fn push_oir_u32(buf: &mut Vec<u8>, value: u32) {
        buf.extend_from_slice(&value.to_le_bytes());
    }

    fn push_oir_prefix(buf: &mut Vec<u8>) {
        buf.extend_from_slice(b"OLYMPUSRAWFORMAT");
        push_oir_u32(buf, 0xffff_ffff);
        push_oir_u32(buf, 0);
    }

    fn push_oir_xml_block(buf: &mut Vec<u8>, xml: &str) {
        let total = 48 + xml.len() as u32;
        push_oir_u32(buf, total);
        push_oir_u32(buf, 0);
        buf.extend(std::iter::repeat(0).take(36));
        push_oir_u32(buf, xml.len() as u32);
        buf.extend_from_slice(xml.as_bytes());
    }

    fn push_empty_oir_xml_block(buf: &mut Vec<u8>) {
        push_oir_u32(buf, 8);
        push_oir_u32(buf, 0);
    }

    fn push_oir_pixel_block(buf: &mut Vec<u8>, uid: &str, pixels: &[u8]) {
        push_oir_u32(buf, uid.len() as u32 + 12);
        push_oir_u32(buf, 3);
        buf.extend_from_slice(&[0; 8]);
        push_oir_u32(buf, uid.len() as u32);
        buf.extend_from_slice(uid.as_bytes());
        push_oir_u32(buf, pixels.len() as u32);
        push_oir_u32(buf, 0);
        buf.extend_from_slice(pixels);
    }

    fn write_native_oir_with_companion_pixels(main: &Path, companion: &Path, pixels: &[u8]) {
        let xml = "<?xml version=\"1.0\"?>\
         <imageProperties>\
           <frameProperties>\
             <width>2</width><height>2</height><depth>1</depth><bitCounts>8</bitCounts>\
           </frameProperties>\
           <imageInfo><channel id=\"c1\" order=\"1\"/></imageInfo>\
         </imageProperties>";

        let mut main_bytes = Vec::new();
        push_oir_prefix(&mut main_bytes);
        push_oir_xml_block(&mut main_bytes, xml);
        std::fs::write(main, main_bytes).unwrap();

        let mut companion_bytes = Vec::new();
        push_oir_prefix(&mut companion_bytes);
        push_oir_u32(&mut companion_bytes, 0xffff_ffff);
        push_oir_u32(&mut companion_bytes, 0);
        push_empty_oir_xml_block(&mut companion_bytes);
        push_oir_pixel_block(&mut companion_bytes, "z001t001_c1_0", pixels);
        std::fs::write(companion, companion_bytes).unwrap();
    }

    #[test]
    fn region_intersection_and_intersects_match_java() {
        let a = Region::new(0, 0, 4, 4);
        let b = Region::new(2, 0, 4, 4);
        assert!(a.intersects(&b));
        let inter = a.intersection(&b);
        assert_eq!((inter.x, inter.y, inter.width, inter.height), (2, 0, 2, 4));

        let far = Region::new(100, 100, 4, 4);
        assert!(!a.intersects(&far));
    }

    #[test]
    fn dom_get_elements_by_tag_name_is_recursive() {
        let xml = "<matl:group xmlns:matl=\"x\"><matl:areaInfo>\
            <matl:numOfXAreas>2</matl:numOfXAreas></matl:areaInfo>\
            <matl:area><matl:xIndex>0</matl:xIndex></matl:area>\
            <matl:area><matl:xIndex>1</matl:xIndex></matl:area></matl:group>";
        let root = parse_dom(xml).unwrap();
        let mut areas = Vec::new();
        root.elements_by_tag_name("matl:area", &mut areas);
        assert_eq!(areas.len(), 2);
        let mut x = Vec::new();
        root.elements_by_tag_name("matl:numOfXAreas", &mut x);
        assert_eq!(x.len(), 1);
        assert_eq!(x[0].text, "2");
    }

    #[test]
    fn omp2info_original_metadata_uses_java_text_keys_and_keeps_attrs() {
        let mut reader = OlympusTileReader::new();
        let node = DomNode {
            name: "matl:stage".into(),
            attrs: vec![("id".into(), "s1".into())],
            text: String::new(),
            children: vec![DomNode {
                name: "matl:position".into(),
                attrs: vec![("unit".into(), "um".into())],
                text: "12".into(),
                children: Vec::new(),
            }],
        };

        reader.parse_original_metadata(&node);
        let string_value = |key: &str| match reader.global_metadata().get(key) {
            Some(MetadataValue::String(value)) => Some(value.as_str()),
            _ => None,
        };
        assert_eq!(string_value("stage id"), Some("s1"));
        assert_eq!(string_value("stage position"), Some("12"));
        assert_eq!(string_value("position unit"), Some("um"));
    }

    #[test]
    fn omp2info_set_id_metadata_and_stitched_pixels() {
        // Build a 2 (cols) x 1 (row) mosaic of 4x4 8-bit tiles saved as `.oir`
        // (TIFF exports), referenced by a synthetic `.omp2info`.
        let root = temp_path("tile.omp2info");
        let dir = root.parent().unwrap().to_path_buf();
        let stem = root.file_stem().unwrap().to_string_lossy().to_string();

        let tile0 = dir.join(format!("{stem}_tile0.oir"));
        let tile1 = dir.join(format!("{stem}_tile1.oir"));
        write_tile_tiff(&tile0, 4, 4, 10);
        write_tile_tiff(&tile1, 4, 4, 20);

        // No stage overlap / coordinates -> adjustWidth == helper sizeX (4),
        // so tile 1 lands at x=4 and the stitched image is 8x4.
        let xml = format!(
            "<?xml version=\"1.0\"?>\n\
             <matl:properties xmlns:matl=\"http://olympus/matl\" xmlns:marker=\"http://olympus/marker\">\n\
               <matl:group>\n\
                 <matl:areaInfo>\n\
                   <matl:numOfXAreas>2</matl:numOfXAreas>\n\
                   <matl:numOfYAreas>1</matl:numOfYAreas>\n\
                 </matl:areaInfo>\n\
                 <matl:area>\n\
                   <matl:image>{t0}</matl:image>\n\
                   <matl:xIndex>0</matl:xIndex>\n\
                   <matl:yIndex>0</matl:yIndex>\n\
                 </matl:area>\n\
                 <matl:area>\n\
                   <matl:image>{t1}</matl:image>\n\
                   <matl:xIndex>1</matl:xIndex>\n\
                   <matl:yIndex>0</matl:yIndex>\n\
                 </matl:area>\n\
               </matl:group>\n\
             </matl:properties>\n",
            t0 = tile0.file_name().unwrap().to_string_lossy(),
            t1 = tile1.file_name().unwrap().to_string_lossy(),
        );
        std::fs::write(&root, xml).unwrap();

        let mut reader = OlympusTileReader::new();
        assert!(reader.is_this_type_by_name(&root));
        reader.set_id(&root).unwrap();

        let meta = reader.metadata();
        assert_eq!(meta.size_x, 8, "stitched width should span both tiles");
        assert_eq!(meta.size_y, 4);
        assert_eq!(meta.pixel_type, PixelType::Uint8);
        assert_eq!(reader.series_count(), 1);

        // Tile bounding boxes recorded in global metadata.
        assert!(reader
            .global_metadata()
            .keys()
            .any(|k| k.starts_with("tile bounding box")));

        // getSeriesUsedFiles(false) lists the omp2info plus both tiles.
        let used = reader.get_series_used_files(false);
        assert!(used.iter().any(|f| f.ends_with("tile0.oir")));
        assert!(used.iter().any(|f| f.ends_with("tile1.oir")));

        // Full-plane stitch: left half == 10, right half == 20.
        let buf = reader.open_bytes(0).unwrap();
        assert_eq!(buf.len(), 8 * 4);
        for row in 0..4 {
            for col in 0..8 {
                let expected = if col < 4 { 10 } else { 20 };
                assert_eq!(buf[row * 8 + col], expected, "pixel ({col},{row}) mismatch");
            }
        }

        let _ = std::fs::remove_file(&root);
        let _ = std::fs::remove_file(&tile0);
        let _ = std::fs::remove_file(&tile1);
    }

    #[test]
    fn omp2info_used_files_include_oir_companions_like_java() {
        let root = temp_path("tile_companion.omp2info");
        let dir = root.parent().unwrap().to_path_buf();
        let tile = dir.join("tile_companion.oir");
        let companion = dir.join("tile_companion_00001");
        write_native_oir_with_companion_pixels(&tile, &companion, &[7, 8, 9, 10]);

        let xml = format!(
            "<?xml version=\"1.0\"?>\n\
             <matl:properties xmlns:matl=\"http://olympus/matl\" xmlns:marker=\"http://olympus/marker\">\n\
               <matl:group>\n\
                 <matl:areaInfo><matl:numOfXAreas>1</matl:numOfXAreas><matl:numOfYAreas>1</matl:numOfYAreas></matl:areaInfo>\n\
                 <matl:area><matl:image>{tile}</matl:image><matl:xIndex>0</matl:xIndex><matl:yIndex>0</matl:yIndex></matl:area>\n\
               </matl:group>\n\
             </matl:properties>\n",
            tile = tile.file_name().unwrap().to_string_lossy(),
        );
        std::fs::write(&root, xml).unwrap();

        let mut reader = OlympusTileReader::new();
        reader.set_id(&root).unwrap();

        let used = reader.get_series_used_files(false);
        assert!(used.iter().any(|f| f.ends_with("tile_companion_00001")));
        assert_eq!(reader.open_bytes(0).unwrap(), vec![7, 8, 9, 10]);

        let _ = std::fs::remove_file(&root);
        let _ = std::fs::remove_file(&tile);
        let _ = std::fs::remove_file(&companion);
    }
}
