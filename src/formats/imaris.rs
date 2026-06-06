//! Imaris IMS format reader (HDF5-based).
//!
//! Reads Bitplane/Oxford Instruments Imaris .ims files.
//! These are HDF5 files containing multi-channel, multi-timepoint,
//! multi-resolution 3-D fluorescence microscopy volumes.
//!
//! Group layout:
//!   DataSetInfo/Image — attributes X, Y, Z (string), ExtMin*/ExtMax* (physical size)
//!   DataSetInfo/Channel N — attribute Name, Color
//!   DataSet/ResolutionLevel R/TimePoint T/Channel C/Data — uint8 or uint16 [z,y,x]

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;
use hdf5_pure_rust::format::messages::datatype::DatatypeClass;
use hdf5_pure_rust::{HyperslabDim, Selection};

pub struct ImarisReader {
    path: Option<PathBuf>,
    // One ImageMetadata per resolution level. Index 0 is full-resolution.
    resolutions: Vec<ImageMetadata>,
    current_resolution: usize,
    // pixel type for raw reads
    bytes_per_sample: usize,
    // Spatial extents from DataSetInfo/Image: [minX,minY,minZ,maxX,maxY,maxZ].
    extents: Option<[f64; 6]>,
    // Per-channel names from DataSetInfo/Channel N.
    channel_names: Vec<Option<String>>,
    // Cache of the most recently decoded plane so that repeated reads of the
    // same plane do not re-read from disk. Keyed by (resolution, t, c, z).
    // Mirrors the per-Z-block buffer cache in ImarisHDFReader.java. The new
    // hdf5-pure-rust crate supports hyperslab partial I/O, so we read only the
    // requested z-plane via read_slice instead of the whole channel volume.
    cache: Option<VolumeCache>,
}

/// Cached decoded plane for one (resolution, timepoint, channel, z) location.
struct VolumeCache {
    res: usize,
    t: usize,
    c: usize,
    z: usize,
    raw: Vec<u8>,
}

impl ImarisReader {
    pub fn new() -> Self {
        ImarisReader {
            path: None,
            resolutions: Vec::new(),
            current_resolution: 0,
            bytes_per_sample: 1,
            extents: None,
            channel_names: Vec::new(),
            cache: None,
        }
    }
}

impl Default for ImarisReader {
    fn default() -> Self {
        Self::new()
    }
}

/// Read a string attribute from an HDF5 group (tries VarLenAscii then FixedAscii).
fn read_str_attr(group: &hdf5_pure_rust::Group, attr: &str) -> Option<String> {
    let a = group.attr(attr).ok()?;
    // String-typed attributes: read directly. Imaris stores strings as arrays
    // of single-character (|S1) elements, so read_strings() returns one entry
    // per character — concatenate them all rather than taking just the first.
    if let Ok(v) = a.read_strings() {
        if !v.is_empty() {
            let joined: String = v.concat();
            let trimmed = joined.trim_matches('\0').trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    let s = a.read_string();
    if !s.is_empty() {
        return Some(s.trim_matches('\0').trim().to_string());
    }
    // Numeric attributes: format the scalar value as a string.
    if let Some(i) = a.read_scalar_i64() {
        return Some(i.to_string());
    }
    if let Some(f) = a.read_scalar_f64() {
        return Some(f.to_string());
    }
    None
}

struct ImsParse {
    resolutions: Vec<ImageMetadata>,
    bytes_per_sample: usize,
    extents: Option<[f64; 6]>,
    channel_names: Vec<Option<String>>,
}

fn parse_ims(path: &Path) -> Result<ImsParse> {
    let file = hdf5_pure_rust::File::open(path)
        .map_err(|e| BioFormatsError::Format(format!("HDF5 open error: {e}")))?;

    // ── Read dimensions from DataSetInfo/Image ──────────────────────────────
    let img_group = file
        .group("DataSetInfo/Image")
        .map_err(|e| BioFormatsError::Format(format!("DataSetInfo/Image missing: {e}")))?;

    // Spatial extents (ExtMin0..2 / ExtMax0..2) for deriving physical sizes.
    let ext_val = |attr: &str| -> Option<f64> {
        read_str_attr(&img_group, attr).and_then(|s| s.trim().parse::<f64>().ok())
    };
    let extents = match (
        ext_val("ExtMin0"),
        ext_val("ExtMin1"),
        ext_val("ExtMin2"),
        ext_val("ExtMax0"),
        ext_val("ExtMax1"),
        ext_val("ExtMax2"),
    ) {
        (Some(a), Some(b), Some(c), Some(d), Some(e), Some(f)) => Some([a, b, c, d, e, f]),
        _ => None,
    };

    // The DataSetInfo/Image X/Y/Z attributes are advisory and unreliable — some
    // writers store 1/1/1 (observed in real .ims files). The authoritative pixel
    // dimensions are the full-resolution Data dataset shape [z, y, x], so derive
    // X/Y/Z from it instead of the attributes.
    let (size_z, size_y, size_x) = ims_level_dims(&file, 0)?;

    // ── Count channels ──────────────────────────────────────────────────────
    // Count groups named "Channel N" under DataSetInfo
    let ds_info = file
        .group("DataSetInfo")
        .map_err(|e| BioFormatsError::Format(format!("DataSetInfo missing: {e}")))?;
    let mut size_c: u32 = 0;
    if let Ok(members) = hdf5_group_members(&ds_info) {
        size_c = members.iter().filter(|n| n.starts_with("Channel ")).count() as u32;
    }
    if size_c == 0 {
        let tp0 = file
            .group("DataSet/ResolutionLevel 0/TimePoint 0")
            .map_err(|e| {
                BioFormatsError::UnsupportedFormat(format!(
                    "Imaris: no channel metadata and TimePoint 0 missing: {e}"
                ))
            })?;
        size_c = hdf5_group_members(&tp0)
            .unwrap_or_default()
            .iter()
            .filter(|n| n.starts_with("Channel "))
            .count() as u32;
        if size_c == 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "Imaris: no channels found".into(),
            ));
        }
    }

    // ── Count timepoints from DataSet/ResolutionLevel 0 ────────────────────
    let size_t: u32 = if let Ok(rl0) = file.group("DataSet/ResolutionLevel 0") {
        if let Ok(members) = hdf5_group_members(&rl0) {
            let n = members
                .iter()
                .filter(|n| n.starts_with("TimePoint "))
                .count() as u32;
            n
        } else {
            0
        }
    } else {
        0
    };
    if size_t == 0 {
        return Err(BioFormatsError::UnsupportedFormat(
            "Imaris: no timepoints found".into(),
        ));
    }

    // ── Count resolution levels ─────────────────────────────────────────────
    let n_resolutions: usize = if let Ok(ds_group) = file.group("DataSet") {
        if let Ok(members) = hdf5_group_members(&ds_group) {
            let n = members
                .iter()
                .filter(|n| n.starts_with("ResolutionLevel "))
                .count();
            n
        } else {
            0
        }
    } else {
        0
    };
    if n_resolutions == 0 {
        return Err(BioFormatsError::UnsupportedFormat(
            "Imaris: no resolution levels found".into(),
        ));
    }

    // ── Determine pixel type from first Data dataset ────────────────────────
    let data_path = "DataSet/ResolutionLevel 0/TimePoint 0/Channel 0/Data";
    let ds = file.dataset(data_path).map_err(|e| {
        BioFormatsError::UnsupportedFormat(format!("Imaris: missing {data_path}: {e}"))
    })?;
    let (pixel_type, bytes_per_sample) = {
        let dtype = ds.dtype().map_err(|e| {
            BioFormatsError::Format(format!("Imaris: cannot read dtype for {data_path}: {e}"))
        })?;
        // Java ImarisHDFReader.java:336-337 maps the sample array type to the
        // pixel type, including FLOAT and DOUBLE. Distinguish float/double from
        // the integer types of the same element size by inspecting the dtype
        // class and element size.
        let class = dtype.class();
        let size = dtype.size();
        let signed = dtype.is_signed().unwrap_or(false);
        match (class, size) {
            (DatatypeClass::FloatingPoint, 4) => (PixelType::Float32, 4usize),
            (DatatypeClass::FloatingPoint, 8) => (PixelType::Float64, 8usize),
            (DatatypeClass::FixedPoint, 1) => {
                if signed {
                    (PixelType::Int8, 1usize)
                } else {
                    (PixelType::Uint8, 1usize)
                }
            }
            (DatatypeClass::FixedPoint, 2) => {
                if signed {
                    (PixelType::Int16, 2usize)
                } else {
                    (PixelType::Uint16, 2usize)
                }
            }
            (DatatypeClass::FixedPoint, 4) => {
                if signed {
                    (PixelType::Int32, 4usize)
                } else {
                    (PixelType::Uint32, 4usize)
                }
            }
            _ => {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "Imaris: unsupported dtype (class {class:?}, size {size}) for {data_path}"
                )));
            }
        }
    };
    validate_ims_data_dataset(&file, data_path, size_x, size_y, size_z, bytes_per_sample)?;

    // ── Collect channel metadata ────────────────────────────────────────────
    let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
    meta_map.insert("format".into(), MetadataValue::String("Imaris IMS".into()));
    let mut channel_names: Vec<Option<String>> = vec![None; size_c as usize];
    for c in 0..size_c {
        if let Ok(ch_group) = file.group(&format!("DataSetInfo/Channel {c}")) {
            if let Some(name) = read_str_attr(&ch_group, "Name") {
                meta_map.insert(format!("channel_{c}_name"), MetadataValue::String(name.clone()));
                channel_names[c as usize] = Some(name);
            }
            if let Some(color) = read_str_attr(&ch_group, "Color") {
                meta_map.insert(format!("channel_{c}_color"), MetadataValue::String(color));
            }
        }
    }

    // ── Build per-resolution-level metadata ─────────────────────────────────
    // Java reads ImageSizeX/Y/Z attributes from the group
    // DataSet/ResolutionLevel_N/TimePoint_0/Channel_0 for each sub-resolution
    // (level 0 uses the DataSetInfo/Image dimensions). sizeC and sizeT are
    // shared across all levels.
    let image_count0 = checked_image_count(size_z, size_c, size_t, "base")?;
    let base_meta = ImageMetadata {
        size_x,
        size_y,
        size_z,
        size_c,
        size_t,
        pixel_type,
        bits_per_pixel: (bytes_per_sample * 8) as u8,
        image_count: image_count0,
        dimension_order: DimensionOrder::XYZCT,
        is_rgb: false,
        is_interleaved: false,
        is_indexed: false,
        is_little_endian: true,
        resolution_count: n_resolutions as u32,
        series_metadata: meta_map,
        lookup_table: None,
        modulo_z: None,
        modulo_c: None,
        modulo_t: None,
    };

    let mut resolutions = Vec::with_capacity(n_resolutions);
    resolutions.push(base_meta.clone());
    for level in 1..n_resolutions {
        let group_path = format!("DataSet/ResolutionLevel {level}/TimePoint 0/Channel 0");
        let mut lvl = base_meta.clone();
        let _ = &group_path; // (kept for error context below)
        // Derive this level's dimensions from its own Data dataset shape rather
        // than the ImageSize* attributes (same rationale as level 0).
        let (lz, ly, lx) = ims_level_dims(&file, level)?;
        lvl.size_z = lz;
        lvl.size_y = ly;
        lvl.size_x = lx;
        validate_ims_data_dataset(
            &file,
            &format!("DataSet/ResolutionLevel {level}/TimePoint 0/Channel 0/Data"),
            lvl.size_x,
            lvl.size_y,
            lvl.size_z,
            bytes_per_sample,
        )?;
        lvl.image_count = checked_image_count(lvl.size_z, lvl.size_c, lvl.size_t, "resolution")?;
        lvl.resolution_count = n_resolutions as u32;
        resolutions.push(lvl);
    }

    Ok(ImsParse {
        resolutions,
        bytes_per_sample,
        extents,
        channel_names,
    })
}

/// Read an integer attribute (string- or numeric-encoded) from an HDF5 group.

fn checked_image_count(size_z: u32, size_c: u32, size_t: u32, label: &str) -> Result<u32> {
    size_z
        .checked_mul(size_c)
        .and_then(|v| v.checked_mul(size_t))
        .ok_or_else(|| BioFormatsError::Format(format!("Imaris {label} image count overflows")))
}

/// Read the (z, y, x) pixel dimensions of a resolution level from its
/// full-resolution Channel-0 `Data` dataset shape (the authoritative source,
/// vs. the unreliable DataSetInfo X/Y/Z and per-level ImageSize* attributes).
fn ims_level_dims(file: &hdf5_pure_rust::File, level: usize) -> Result<(u32, u32, u32)> {
    let path = format!("DataSet/ResolutionLevel {level}/TimePoint 0/Channel 0/Data");
    let ds = file
        .dataset(&path)
        .map_err(|e| BioFormatsError::UnsupportedFormat(format!("Imaris: missing {path}: {e}")))?;
    let shape = ds
        .shape()
        .map_err(|e| BioFormatsError::Format(format!("Imaris: cannot read shape for {path}: {e}")))?;
    if shape.len() != 3 || shape.iter().any(|&d| d == 0) {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "Imaris: unsupported Data shape {shape:?} for {path}"
        )));
    }
    let to_u32 = |d: u64| -> Result<u32> {
        u32::try_from(d).map_err(|_| BioFormatsError::Format("Imaris dimension overflows u32".into()))
    };
    Ok((to_u32(shape[0])?, to_u32(shape[1])?, to_u32(shape[2])?))
}

fn validate_ims_data_dataset(
    file: &hdf5_pure_rust::File,
    path: &str,
    size_x: u32,
    size_y: u32,
    size_z: u32,
    bytes_per_sample: usize,
) -> Result<()> {
    let ds = file
        .dataset(path)
        .map_err(|e| BioFormatsError::UnsupportedFormat(format!("Imaris: missing {path}: {e}")))?;
    let shape = ds.shape().map_err(|e| {
        BioFormatsError::Format(format!("Imaris: cannot read shape for {path}: {e}"))
    })?;
    if shape.len() != 3 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "Imaris: {path} has unsupported rank {}",
            shape.len()
        )));
    }
    if shape[0] == 0 || shape[1] == 0 || shape[2] == 0 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "Imaris: {path} has zero dataset axis"
        )));
    }
    let declared = [size_z as u64, size_y as u64, size_x as u64];
    if shape != declared {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "Imaris: {path} shape {shape:?} does not match declared {declared:?}"
        )));
    }
    let dtype_size = ds
        .dtype()
        .map(|dt| hdf5_dtype_size(&dt))
        .map_err(|e| {
            BioFormatsError::Format(format!("Imaris: cannot read dtype for {path}: {e}"))
        })?;
    if dtype_size != bytes_per_sample {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "Imaris: {path} dtype size {dtype_size} does not match declared {bytes_per_sample}"
        )));
    }
    Ok(())
}

fn hdf5_group_members(
    group: &hdf5_pure_rust::Group,
) -> std::result::Result<Vec<String>, hdf5_pure_rust::Error> {
    group.member_names()
}

/// Element byte size of an HDF5 datatype (the `size()` already reported by the
/// crate, which for Array types is the total array byte size).
fn hdf5_dtype_size(dtype: &hdf5_pure_rust::Datatype) -> usize {
    dtype.size()
}

impl FormatReader for ImarisReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("ims"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // HDF5 signature: bytes 0-7 = \x89HDF\r\n\x1a\n
        header.len() >= 8 && header[0..8] == [0x89, 0x48, 0x44, 0x46, 0x0d, 0x0a, 0x1a, 0x0a]
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let parsed = parse_ims(path)?;
        self.resolutions = parsed.resolutions;
        self.path = Some(path.to_path_buf());
        self.current_resolution = 0;
        self.bytes_per_sample = parsed.bytes_per_sample;
        self.extents = parsed.extents;
        self.channel_names = parsed.channel_names;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.resolutions.clear();
        self.current_resolution = 0;
        self.extents = None;
        self.channel_names.clear();
        self.cache = None;
        Ok(())
    }

    fn series_count(&self) -> usize {
        usize::from(!self.resolutions.is_empty())
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.resolutions.is_empty() {
            return Err(BioFormatsError::NotInitialized);
        }
        if s != 0 {
            Err(BioFormatsError::SeriesOutOfRange(s))
        } else {
            Ok(())
        }
    }

    fn series(&self) -> usize {
        0
    }

    fn metadata(&self) -> &ImageMetadata {
        self.resolutions
            .get(self.current_resolution)
            .or_else(|| self.resolutions.first())
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn resolution_count(&self) -> usize {
        self.resolutions.len()
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if level >= self.resolutions.len() {
            return Err(BioFormatsError::Format(format!(
                "resolution {level} out of range"
            )));
        }
        self.current_resolution = level;
        Ok(())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let res = self.current_resolution;
        let meta = self
            .resolutions
            .get(res)
            .ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }

        // Decode plane_index → (z, c, t) for XYZCT order using this level's dims
        let sz = meta.size_z as usize;
        let sc = meta.size_c as usize;
        let z = (plane_index as usize) % sz;
        let c = (plane_index as usize / sz) % sc;
        let t = (plane_index as usize) / (sz * sc);
        let size_x = meta.size_x as usize;
        let size_y = meta.size_y as usize;
        let bps = self.bytes_per_sample;
        let plane_bytes = size_x * size_y * bps;

        // Reuse the cached plane if it is for the same (resolution, t, c, z).
        let need_load = match &self.cache {
            Some(cache) => {
                cache.res != res || cache.t != t || cache.c != c || cache.z != z
            }
            None => true,
        };
        if need_load {
            let data_path = format!("DataSet/ResolutionLevel {res}/TimePoint {t}/Channel {c}/Data");
            let path = self
                .path
                .as_ref()
                .ok_or(BioFormatsError::NotInitialized)?
                .clone();
            let file = hdf5_pure_rust::File::open(&path)
                .map_err(|e| BioFormatsError::Format(format!("HDF5: {e}")))?;
            let ds = file
                .dataset(&data_path)
                .map_err(|e| BioFormatsError::Format(format!("dataset {data_path}: {e}")))?;

            // The dataset is shaped [z, y, x]; use a hyperslab selection to read
            // ONLY the requested z-plane. The returned vec is exactly that plane
            // (Y*X elements) indexed from 0, so it is cached and returned whole.
            let sel = Selection::Hyperslab(vec![
                HyperslabDim::new(z as u64, 1, 1, 1), // single z slice
                HyperslabDim::new(0, 1, size_y as u64, 1), // all rows
                HyperslabDim::new(0, 1, size_x as u64, 1), // all cols
            ]);

            let raw: Vec<u8> = match bps {
                1 => ds
                    .read_slice::<u8, _>(sel)
                    .map_err(|e| BioFormatsError::Format(format!("HDF5 read: {e}")))?,
                2 => {
                    let words: Vec<u16> = ds
                        .read_slice::<u16, _>(sel)
                        .map_err(|e| BioFormatsError::Format(format!("HDF5 read: {e}")))?;
                    words.iter().flat_map(|w| w.to_le_bytes()).collect()
                }
                4 => {
                    let dwords: Vec<u32> = ds
                        .read_slice::<u32, _>(sel)
                        .map_err(|e| BioFormatsError::Format(format!("HDF5 read: {e}")))?;
                    dwords.iter().flat_map(|d| d.to_le_bytes()).collect()
                }
                _ => ds
                    .read_slice::<u8, _>(sel)
                    .map_err(|e| BioFormatsError::Format(format!("HDF5 read: {e}")))?,
            };
            self.cache = Some(VolumeCache { res, t, c, z, raw });
        }

        let raw = &self.cache.as_ref().unwrap().raw;
        // raw is now exactly plane z, indexed from offset 0.
        if plane_bytes <= raw.len() {
            Ok(raw[..plane_bytes].to_vec())
        } else {
            Err(BioFormatsError::UnsupportedFormat(format!(
                "Imaris ResolutionLevel {res}/TimePoint {t}/Channel {c} plane {plane_index} is \
                 shorter than declared (need {} bytes, have {})",
                plane_bytes,
                raw.len()
            )))
        }
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
            .resolutions
            .get(self.current_resolution)
            .ok_or(BioFormatsError::NotInitialized)?;
        crop_full_plane("Imaris", &full, meta, 1, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
        // Try to read the Imaris built-in thumbnail
        let path = self
            .path
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?
            .clone();
        if let Ok(file) = hdf5_pure_rust::File::open(&path) {
            if let Ok(ds) = file.dataset("Thumbnail/Data") {
                if let Ok(data) = ds.read::<u8>() {
                    return Ok(data);
                }
            }
        }
        // Fall back to center crop of plane 0
        let meta = self
            .resolutions
            .get(self.current_resolution)
            .ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(0, tx, ty, tw, th)
    }

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        let meta = self.resolutions.get(self.current_resolution)?;
        let mut ome = crate::common::ome_metadata::OmeMetadata::from_image_metadata(meta);
        let img = ome.images.get_mut(0)?;

        // Image name = "<basename> Resolution Level <level+1>" (Java
        // ImarisHDFReader sets the name per series/resolution-level).
        if let Some(path) = self.path.as_ref() {
            let base = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            img.name = Some(format!(
                "{base} Resolution Level {}",
                self.current_resolution + 1
            ));
        }

        // Physical pixel size: Java uses RecordingEntry*Spacing when set,
        // otherwise (ExtMax - ExtMin) / size. We only have the extents path.
        if let Some(ext) = self.extents {
            let span = |hi: f64, lo: f64, n: u32| {
                if n > 0 {
                    Some((hi - lo) / n as f64)
                } else {
                    None
                }
            };
            img.physical_size_x = span(ext[3], ext[0], meta.size_x);
            img.physical_size_y = span(ext[4], ext[1], meta.size_y);
            img.physical_size_z = span(ext[5], ext[2], meta.size_z);
        }

        // Per-channel names.
        for (ci, ch) in img.channels.iter_mut().enumerate() {
            if let Some(Some(name)) = self.channel_names.get(ci) {
                ch.name = Some(name.clone());
            }
        }

        Some(ome)
    }
}
