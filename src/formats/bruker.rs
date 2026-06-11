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
    #[allow(dead_code)]
    timestamp: Option<String>,
    #[allow(dead_code)]
    institution: Option<String>,
    #[allow(dead_code)]
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
    current: usize,
}

impl BrukerReader {
    pub fn new() -> Self {
        BrukerReader {
            pixels_files: Vec::new(),
            metas: Vec::new(),
            image_names: Vec::new(),
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
        self.image_names = vec![None; series_count];

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
        use crate::common::ome_metadata::{OmeImage, OmeMetadata};
        let images = self
            .image_names
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let base = name.clone().unwrap_or_default();
                OmeImage {
                    name: Some(format!("{} #{}", base, i + 1)),
                    ..Default::default()
                }
            })
            .collect();
        Some(OmeMetadata {
            images,
            ..Default::default()
        })
    }
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

        let _ = fs::remove_dir_all(&root);
    }
}
