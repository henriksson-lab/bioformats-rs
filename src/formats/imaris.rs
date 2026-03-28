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

pub struct ImarisReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    n_resolutions: usize,
    current_resolution: usize,
    // pixel type for raw reads
    bytes_per_sample: usize,
}

impl ImarisReader {
    pub fn new() -> Self {
        ImarisReader {
            path: None,
            meta: None,
            n_resolutions: 1,
            current_resolution: 0,
            bytes_per_sample: 1,
        }
    }
}

impl Default for ImarisReader { fn default() -> Self { Self::new() } }

/// Read a string attribute from an HDF5 group (tries VarLenAscii then FixedAscii).
fn read_str_attr(group: &hdf5::Group, attr: &str) -> Option<String> {
    let a = group.attr(attr).ok()?;
    // Try VarLenAscii
    if let Ok(s) = a.read_scalar::<hdf5::types::VarLenAscii>() {
        return Some(s.as_str().trim_matches('\0').trim().to_string());
    }
    // Try FixedAscii<64>
    if let Ok(s) = a.read_scalar::<hdf5::types::FixedAscii<64>>() {
        return Some(s.as_str().trim_matches('\0').trim().to_string());
    }
    None
}

fn parse_ims(path: &Path) -> Result<(ImageMetadata, usize, usize)> {
    let file = hdf5::File::open(path)
        .map_err(|e| BioFormatsError::Format(format!("HDF5 open error: {e}")))?;

    // ── Read dimensions from DataSetInfo/Image ──────────────────────────────
    let img_group = file.group("DataSetInfo/Image")
        .map_err(|e| BioFormatsError::Format(format!("DataSetInfo/Image missing: {e}")))?;

    let size_x = read_str_attr(&img_group, "X")
        .and_then(|s| s.parse::<u32>().ok()).unwrap_or(512);
    let size_y = read_str_attr(&img_group, "Y")
        .and_then(|s| s.parse::<u32>().ok()).unwrap_or(512);
    let size_z = read_str_attr(&img_group, "Z")
        .and_then(|s| s.parse::<u32>().ok()).unwrap_or(1);

    // ── Count channels ──────────────────────────────────────────────────────
    // Count groups named "Channel N" under DataSetInfo
    let ds_info = file.group("DataSetInfo")
        .map_err(|e| BioFormatsError::Format(format!("DataSetInfo missing: {e}")))?;
    let mut size_c: u32 = 0;
    if let Ok(members) = ds_info.member_names() {
        size_c = members.iter().filter(|n| n.starts_with("Channel ")).count() as u32;
    }
    if size_c == 0 { size_c = 1; }

    // ── Count timepoints from DataSet/ResolutionLevel 0 ────────────────────
    let size_t: u32 = if let Ok(rl0) = file.group("DataSet/ResolutionLevel 0") {
        if let Ok(members) = rl0.member_names() {
            let n = members.iter().filter(|n| n.starts_with("TimePoint ")).count() as u32;
            if n == 0 { 1 } else { n }
        } else { 1 }
    } else { 1 };

    // ── Count resolution levels ─────────────────────────────────────────────
    let n_resolutions: usize = if let Ok(ds_group) = file.group("DataSet") {
        if let Ok(members) = ds_group.member_names() {
            let n = members.iter().filter(|n| n.starts_with("ResolutionLevel ")).count();
            if n == 0 { 1 } else { n }
        } else { 1 }
    } else { 1 };

    // ── Determine pixel type from first Data dataset ────────────────────────
    let data_path = "DataSet/ResolutionLevel 0/TimePoint 0/Channel 0/Data";
    let (pixel_type, bytes_per_sample) = if let Ok(ds) = file.dataset(data_path) {
        match ds.dtype().map(|d| d.size()).unwrap_or(1) {
            1 => (PixelType::Uint8,  1usize),
            2 => (PixelType::Uint16, 2usize),
            4 => (PixelType::Uint32, 4usize),
            _ => (PixelType::Uint8,  1usize),
        }
    } else {
        (PixelType::Uint8, 1usize)
    };

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

    let image_count = size_z * size_c * size_t;
    let meta = ImageMetadata {
        size_x, size_y, size_z, size_c, size_t,
        pixel_type,
        bits_per_pixel: (bytes_per_sample * 8) as u8,
        image_count,
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

    Ok((meta, n_resolutions, bytes_per_sample))
}

impl FormatReader for ImarisReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("ims"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // HDF5 signature: bytes 0-7 = \x89HDF\r\n\x1a\n
        header.len() >= 8 && header[0..8] == [0x89, 0x48, 0x44, 0x46, 0x0d, 0x0a, 0x1a, 0x0a]
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let (meta, n_resolutions, bps) = parse_ims(path)?;
        self.meta = Some(meta);
        self.path = Some(path.to_path_buf());
        self.n_resolutions = n_resolutions;
        self.current_resolution = 0;
        self.bytes_per_sample = bps;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.n_resolutions = 1;
        self.current_resolution = 0;
        Ok(())
    }

    fn series_count(&self) -> usize { 1 }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if s != 0 { Err(BioFormatsError::SeriesOutOfRange(s)) } else { Ok(()) }
    }

    fn series(&self) -> usize { 0 }

    fn metadata(&self) -> &ImageMetadata {
        self.meta.as_ref().expect("set_id not called")
    }

    fn resolution_count(&self) -> usize { self.n_resolutions }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if level >= self.n_resolutions {
            return Err(BioFormatsError::Format(format!("resolution {level} out of range")));
        }
        self.current_resolution = level;
        Ok(())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }

        // Decode plane_index → (z, c, t) for XYZCT order
        let sz = meta.size_z as usize;
        let sc = meta.size_c as usize;
        let z = (plane_index as usize) % sz;
        let c = (plane_index as usize / sz) % sc;
        let t = (plane_index as usize) / (sz * sc);

        let res = self.current_resolution;
        let data_path = format!("DataSet/ResolutionLevel {res}/TimePoint {t}/Channel {c}/Data");

        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?.clone();
        let file = hdf5::File::open(&path)
            .map_err(|e| BioFormatsError::Format(format!("HDF5: {e}")))?;
        let ds = file.dataset(&data_path)
            .map_err(|e| BioFormatsError::Format(format!("dataset {data_path}: {e}")))?;

        // Read full volume as raw bytes then extract the z-plane
        let plane_pixels = meta.size_x as usize * meta.size_y as usize;
        let plane_bytes  = plane_pixels * self.bytes_per_sample;
        let _sz_usize    = sz;

        let raw: Vec<u8> = match self.bytes_per_sample {
            1 => ds.read_raw::<u8>()
                    .map_err(|e| BioFormatsError::Format(format!("HDF5 read: {e}")))?,
            2 => {
                let words: Vec<u16> = ds.read_raw::<u16>()
                    .map_err(|e| BioFormatsError::Format(format!("HDF5 read: {e}")))?;
                words.iter().flat_map(|w| w.to_le_bytes()).collect()
            }
            4 => {
                let dwords: Vec<u32> = ds.read_raw::<u32>()
                    .map_err(|e| BioFormatsError::Format(format!("HDF5 read: {e}")))?;
                dwords.iter().flat_map(|d| d.to_le_bytes()).collect()
            }
            _ => ds.read_raw::<u8>()
                    .map_err(|e| BioFormatsError::Format(format!("HDF5 read: {e}")))?,
        };

        // raw is stored [z, y, x]; extract plane z
        let offset = z * plane_bytes;
        if offset + plane_bytes <= raw.len() {
            Ok(raw[offset..offset + plane_bytes].to_vec())
        } else {
            // Return zeros if data is smaller than expected
            Ok(vec![0u8; plane_bytes])
        }
    }

    fn open_bytes_region(&mut self, plane_index: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let full = self.open_bytes(plane_index)?;
        let meta = self.meta.as_ref().unwrap();
        let bps = self.bytes_per_sample;
        let row_bytes = meta.size_x as usize * bps;
        let out_row = w as usize * bps;
        let mut out = Vec::with_capacity(h as usize * out_row);
        for r in 0..h as usize {
            let src_start = (y as usize + r) * row_bytes + x as usize * bps;
            out.extend_from_slice(&full[src_start..src_start + out_row]);
        }
        Ok(out)
    }

    fn open_thumb_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
        // Try to read the Imaris built-in thumbnail
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?.clone();
        if let Ok(file) = hdf5::File::open(&path) {
            if let Ok(ds) = file.dataset("Thumbnail/Data") {
                if let Ok(data) = ds.read_raw::<u8>() {
                    return Ok(data);
                }
            }
        }
        // Fall back to center crop of plane 0
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(0, tx, ty, tw, th)
    }
}
