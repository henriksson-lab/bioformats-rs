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

pub struct OifReader {
    source: FileSource,
    /// Companion directory for OIF (path prefix); empty for OIB.
    path_prefix: String,
    meta: Option<ImageMetadata>,
    /// Resolved TIFF logical names, in plane order.
    tiffs: Vec<String>,
    #[allow(dead_code)]
    planes: Vec<PlaneData>,
}

impl OifReader {
    pub fn new() -> Self {
        OifReader {
            source: FileSource::Disk,
            path_prefix: String::new(),
            meta: None,
            tiffs: Vec::new(),
            planes: Vec::new(),
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
        }

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

            let pty_bytes = match self.source.read_bytes(&file) {
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
        if let Some(first) = tiffs.first() {
            if let Ok(bytes) = self.source.read_bytes(first) {
                if let Some(tm) = probe_tiff(&bytes) {
                    pixel_type = tm.0;
                    is_little_endian = tm.1;
                    is_rgb = tm.2;
                    if tm.3 > 0 {
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
        let images_per_file = if tiffs.is_empty() {
            0
        } else if image_count % tiffs.len() == 0 {
            image_count / tiffs.len()
        } else {
            return Err(BioFormatsError::Format(format!(
                "OIF/OIB: image count {image_count} is not divisible by {} TIFF file(s)",
                tiffs.len()
            )));
        };
        for tiff in &tiffs {
            let bytes = self.source.read_bytes(tiff)?;
            let tm = probe_tiff_metadata(&bytes).ok_or_else(|| {
                BioFormatsError::Format(format!("OIF/OIB: companion TIFF {tiff} could not be read"))
            })?;
            if tm.size_x != size_x || tm.size_y != size_y {
                return Err(BioFormatsError::Format(format!(
                    "OIF/OIB: companion TIFF {tiff} has dimensions {}x{}, expected {size_x}x{size_y}",
                    tm.size_x, tm.size_y
                )));
            }
            if tm.image_count.max(1) < images_per_file as u32 {
                return Err(BioFormatsError::Format(format!(
                    "OIF/OIB: companion TIFF {tiff} has {} page(s), expected at least {images_per_file}",
                    tm.image_count.max(1)
                )));
            }
        }

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
                reader.set_id(Path::new(&tiff_name))?;
                let inner = reader.metadata().image_count.max(1);
                if image as u32 >= inner {
                    return Err(BioFormatsError::Format(format!(
                        "OIF/OIB: logical plane {plane_index} maps to TIFF page {image}, but {tiff_name} has {inner} page(s)"
                    )));
                }
                reader.open_bytes(image as u32)
            }
            FileSource::Oib { .. } => {
                // Read the embedded TIFF into a temp file, then parse it. The
                // TiffReader requires a path; OIB embeds full TIFF streams.
                let bytes = self.source.read_bytes(&tiff_name)?;
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
                        return Err(BioFormatsError::Format(format!(
                            "OIF/OIB: logical plane {plane_index} maps to TIFF page {image}, but {tiff_name} has {inner} page(s)"
                        )));
                    }
                    reader.open_bytes(image as u32)
                });
                let _ = std::fs::remove_file(&tmp);
                r
            }
        }
    }
}

impl Default for OifReader {
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
    let stem = oif_path.file_stem()?;
    let parent = oif_path.parent()?;
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

fn probe_tiff_metadata(bytes: &[u8]) -> Option<ImageMetadata> {
    let mut tmp = std::env::temp_dir();
    tmp.push(format!("bioformats_oib_meta_{}.tif", rand_suffix(bytes)));
    std::fs::write(&tmp, bytes).ok()?;
    let mut r = TiffReader::new();
    let res = r.set_id(&tmp).ok().map(|_| r.metadata().clone());
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

impl FormatReader for OifReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("oif") || e.eq_ignore_ascii_case("oib"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // Only sniff the OIF text variant by magic. The OIB variant is an OLE2
        // compound document whose magic is shared with ZVI/XRM and other CFB
        // formats; matching it here would cause this reader to intercept those
        // files. OIB is instead detected via its `.oib` extension (Java
        // FV1000Reader.isThisType relies on the suffix for OIB), so non-OIB
        // OLE2 readers get the first attempt during the magic pass.
        let s = std::str::from_utf8(&header[..header.len().min(256)]).unwrap_or("");
        s.contains("[FileInformation]") || s.contains("[File Info]") || s.contains("[Version Info]")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let is_oib = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("oib"))
            .unwrap_or(false);
        if is_oib {
            self.init_oib(path)
        } else {
            self.init_oif(path)
        }
    }

    fn close(&mut self) -> Result<()> {
        self.source = FileSource::Disk;
        self.path_prefix.clear();
        self.meta = None;
        self.tiffs.clear();
        self.planes.clear();
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
        crate::formats::lei::crop_region(&full, meta, x, y, w, h)
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
    fn oif_rejects_logical_plane_without_physical_tiff_page() {
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

        let mut reader = OifReader::new();
        let err = reader.set_id(&root).unwrap_err();
        assert!(
            err.to_string().contains("companion TIFF")
                && err.to_string().contains("expected at least 4"),
            "unexpected error: {err}"
        );

        let _ = std::fs::remove_file(root);
        let _ = std::fs::remove_file(pty);
        let _ = std::fs::remove_file(tiff);
        let _ = std::fs::remove_dir(companion);
    }
}
