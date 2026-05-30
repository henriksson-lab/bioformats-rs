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

pub struct ImarisReader {
    path: Option<PathBuf>,
    // One ImageMetadata per resolution level. Index 0 is full-resolution.
    resolutions: Vec<ImageMetadata>,
    current_resolution: usize,
    // pixel type for raw reads
    bytes_per_sample: usize,
    // Cache of the most recently decoded [z, y, x] volume so that sequential
    // plane reads within the same dataset do not re-decode the whole volume.
    // Keyed by (resolution, t, c). Mirrors the per-Z-block buffer cache in
    // ImarisHDFReader.java (the underlying hdf5-pure 0.5 reader has no
    // hyperslab API, so we cache the whole-channel volume instead of a slab).
    cache: Option<VolumeCache>,
}

/// Cached decoded volume for one (resolution, timepoint, channel) dataset.
struct VolumeCache {
    res: usize,
    t: usize,
    c: usize,
    raw: Vec<u8>,
}

impl ImarisReader {
    pub fn new() -> Self {
        ImarisReader {
            path: None,
            resolutions: Vec::new(),
            current_resolution: 0,
            bytes_per_sample: 1,
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
fn read_str_attr(group: &hdf5_pure::Group<'_>, attr: &str) -> Option<String> {
    let attrs = group.attrs().ok()?;
    match attrs.get(attr)? {
        hdf5_pure::AttrValue::String(s) | hdf5_pure::AttrValue::AsciiString(s) => {
            Some(s.trim_matches('\0').trim().to_string())
        }
        hdf5_pure::AttrValue::StringArray(v)
        | hdf5_pure::AttrValue::AsciiStringArray(v)
        | hdf5_pure::AttrValue::VarLenAsciiArray(v) => {
            v.first().map(|s| s.trim_matches('\0').trim().to_string())
        }
        hdf5_pure::AttrValue::I32(v) => Some(v.to_string()),
        hdf5_pure::AttrValue::I64(v) => Some(v.to_string()),
        hdf5_pure::AttrValue::U32(v) => Some(v.to_string()),
        hdf5_pure::AttrValue::U64(v) => Some(v.to_string()),
        hdf5_pure::AttrValue::F64(v) => Some(v.to_string()),
        _ => None,
    }
}

fn parse_ims(path: &Path) -> Result<(Vec<ImageMetadata>, usize)> {
    let file = hdf5_pure::File::open(path)
        .map_err(|e| BioFormatsError::Format(format!("HDF5 open error: {e}")))?;

    // ── Read dimensions from DataSetInfo/Image ──────────────────────────────
    let img_group = file
        .group("DataSetInfo/Image")
        .map_err(|e| BioFormatsError::Format(format!("DataSetInfo/Image missing: {e}")))?;

    let size_x = read_required_positive_attr(&img_group, "X")?;
    let size_y = read_required_positive_attr(&img_group, "Y")?;
    let size_z = read_required_positive_attr(&img_group, "Z")?;

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
        // the integer types of the same element size by inspecting the dtype.
        match dtype {
            hdf5_pure::DType::F32 => (PixelType::Float32, 4usize),
            hdf5_pure::DType::F64 => (PixelType::Float64, 8usize),
            other => match hdf5_dtype_size(other) {
                1 => (PixelType::Uint8, 1usize),
                2 => (PixelType::Uint16, 2usize),
                4 => (PixelType::Uint32, 4usize),
                size => {
                    return Err(BioFormatsError::UnsupportedFormat(format!(
                        "Imaris: unsupported dtype size {size} for {data_path}"
                    )));
                }
            },
        }
    };
    validate_ims_data_dataset(&file, data_path, size_x, size_y, size_z, bytes_per_sample)?;

    // ── Collect channel metadata ────────────────────────────────────────────
    let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
    meta_map.insert("format".into(), MetadataValue::String("Imaris IMS".into()));
    for c in 0..size_c {
        if let Ok(ch_group) = file.group(&format!("DataSetInfo/Channel {c}")) {
            if let Some(name) = read_str_attr(&ch_group, "Name") {
                meta_map.insert(format!("channel_{c}_name"), MetadataValue::String(name));
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
        let g = file.group(&group_path).map_err(|e| {
            BioFormatsError::UnsupportedFormat(format!("Imaris: missing {group_path}: {e}"))
        })?;
        if let Some(v) = read_int_attr(&g, "ImageSizeX") {
            if v == 0 {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "Imaris: non-positive ImageSizeX for resolution {level}"
                )));
            }
            lvl.size_x = v;
        }
        if let Some(v) = read_int_attr(&g, "ImageSizeY") {
            if v == 0 {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "Imaris: non-positive ImageSizeY for resolution {level}"
                )));
            }
            lvl.size_y = v;
        }
        if let Some(v) = read_int_attr(&g, "ImageSizeZ") {
            if v == 0 {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "Imaris: non-positive ImageSizeZ for resolution {level}"
                )));
            }
            lvl.size_z = v;
        }
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

    Ok((resolutions, bytes_per_sample))
}

/// Read an integer attribute (string- or numeric-encoded) from an HDF5 group.
fn read_int_attr(group: &hdf5_pure::Group<'_>, attr: &str) -> Option<u32> {
    read_str_attr(group, attr).and_then(|s| s.trim().parse::<u32>().ok())
}

fn read_required_positive_attr(group: &hdf5_pure::Group<'_>, attr: &str) -> Result<u32> {
    let value = read_str_attr(group, attr).ok_or_else(|| {
        BioFormatsError::UnsupportedFormat(format!("Imaris: missing Image {attr} attribute"))
    })?;
    let parsed = value.trim().parse::<u32>().map_err(|_| {
        BioFormatsError::UnsupportedFormat(format!(
            "Imaris: invalid Image {attr} attribute {value:?}"
        ))
    })?;
    if parsed == 0 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "Imaris: non-positive Image {attr} attribute"
        )));
    }
    Ok(parsed)
}

fn checked_image_count(size_z: u32, size_c: u32, size_t: u32, label: &str) -> Result<u32> {
    size_z
        .checked_mul(size_c)
        .and_then(|v| v.checked_mul(size_t))
        .ok_or_else(|| BioFormatsError::Format(format!("Imaris {label} image count overflows")))
}

fn validate_ims_data_dataset(
    file: &hdf5_pure::File,
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
    let dtype_size = ds.dtype().map(hdf5_dtype_size).map_err(|e| {
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
    group: &hdf5_pure::Group<'_>,
) -> std::result::Result<Vec<String>, hdf5_pure::Error> {
    let mut members = group.groups()?;
    members.extend(group.datasets()?);
    Ok(members)
}

fn hdf5_dtype_size(dtype: hdf5_pure::DType) -> usize {
    match dtype {
        hdf5_pure::DType::F32
        | hdf5_pure::DType::I32
        | hdf5_pure::DType::U32
        | hdf5_pure::DType::Enum(_) => 4,
        hdf5_pure::DType::F64
        | hdf5_pure::DType::I64
        | hdf5_pure::DType::U64
        | hdf5_pure::DType::ObjectReference => 8,
        hdf5_pure::DType::I16 | hdf5_pure::DType::U16 => 2,
        hdf5_pure::DType::I8 | hdf5_pure::DType::U8 => 1,
        hdf5_pure::DType::Array(base, dims) => {
            hdf5_dtype_size(*base) * dims.iter().copied().product::<u32>() as usize
        }
        _ => 0,
    }
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
        let (resolutions, bps) = parse_ims(path)?;
        self.resolutions = resolutions;
        self.path = Some(path.to_path_buf());
        self.current_resolution = 0;
        self.bytes_per_sample = bps;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.resolutions.clear();
        self.current_resolution = 0;
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

        // Reuse the cached volume if it is for the same (resolution, t, c).
        let need_load = match &self.cache {
            Some(cache) => cache.res != res || cache.t != t || cache.c != c,
            None => true,
        };
        if need_load {
            let data_path = format!("DataSet/ResolutionLevel {res}/TimePoint {t}/Channel {c}/Data");
            let path = self
                .path
                .as_ref()
                .ok_or(BioFormatsError::NotInitialized)?
                .clone();
            let file = hdf5_pure::File::open(&path)
                .map_err(|e| BioFormatsError::Format(format!("HDF5: {e}")))?;
            let ds = file
                .dataset(&data_path)
                .map_err(|e| BioFormatsError::Format(format!("dataset {data_path}: {e}")))?;

            // hdf5-pure 0.5 has no hyperslab API, so read the whole [z,y,x]
            // channel volume once and cache it; subsequent z-planes are served
            // from the cache without re-decoding.
            let raw: Vec<u8> = match bps {
                1 => ds
                    .read_u8()
                    .map_err(|e| BioFormatsError::Format(format!("HDF5 read: {e}")))?,
                2 => {
                    let words: Vec<u16> = ds
                        .read_u16()
                        .map_err(|e| BioFormatsError::Format(format!("HDF5 read: {e}")))?;
                    words.iter().flat_map(|w| w.to_le_bytes()).collect()
                }
                4 => {
                    let dwords: Vec<u32> = ds
                        .read_u32()
                        .map_err(|e| BioFormatsError::Format(format!("HDF5 read: {e}")))?;
                    dwords.iter().flat_map(|d| d.to_le_bytes()).collect()
                }
                _ => ds
                    .read_u8()
                    .map_err(|e| BioFormatsError::Format(format!("HDF5 read: {e}")))?,
            };
            self.cache = Some(VolumeCache { res, t, c, raw });
        }

        let raw = &self.cache.as_ref().unwrap().raw;
        // raw is stored [z, y, x]; extract plane z
        let offset = z * plane_bytes;
        if offset + plane_bytes <= raw.len() {
            Ok(raw[offset..offset + plane_bytes].to_vec())
        } else {
            Err(BioFormatsError::UnsupportedFormat(format!(
                "Imaris ResolutionLevel {res}/TimePoint {t}/Channel {c} is shorter than \
                 declared plane {plane_index} (need {} bytes, have {})",
                offset + plane_bytes,
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
        if let Ok(file) = hdf5_pure::File::open(&path) {
            if let Ok(ds) = file.dataset("Thumbnail/Data") {
                if let Ok(data) = ds.read_u8() {
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
}
