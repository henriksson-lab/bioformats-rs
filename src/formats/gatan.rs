//! Gatan DM3 / DM4 format reader (electron microscopy).
//!
//! Supports DM3 (version 3) and DM4 (version 4) Digital Micrograph files.
//! Reads the tag tree to find the primary image data (ImageList entry 1).

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;

// ── DM image data types ───────────────────────────────────────────────────────
fn dm_pixel_type(dm_type: i32) -> PixelType {
    match dm_type {
        1 => PixelType::Int16,
        2 => PixelType::Float32,
        6 => PixelType::Int8,
        7 => PixelType::Int32,
        8 => PixelType::Uint16,
        9 => PixelType::Uint32,
        10 => PixelType::Float64,
        12 => PixelType::Uint8,  // bool stored as uint8
        23 => PixelType::Uint8,
        _ => PixelType::Uint16,  // fallback
    }
}

fn dm_bytes_per_pixel(dm_type: i32) -> usize {
    match dm_type {
        1 => 2,   // int16
        2 => 4,   // float32
        3 => 8,   // complex8 (2×float32)
        6 => 1,   // int8
        7 => 4,   // int32
        8 => 2,   // uint16
        9 => 4,   // uint32
        10 => 8,  // float64
        11 => 16, // complex16 (2×float64)
        12 => 1,  // bool/uint8
        14 => 1,  // bit (1 byte approx)
        15 => 3,  // bgr
        23 => 1,  // uint8
        _ => 2,
    }
}

// ── Tag value types (DM encoding) ─────────────────────────────────────────────
// info[0] encodes the tag data type:
const DM_TYPE_INT16:  u32 = 2;
const DM_TYPE_INT32:  u32 = 3;
const DM_TYPE_UINT16: u32 = 4;
const DM_TYPE_UINT32: u32 = 5;
const DM_TYPE_FLOAT32: u32 = 6;
const DM_TYPE_FLOAT64: u32 = 7;
const DM_TYPE_BOOL:   u32 = 8;
const DM_TYPE_INT8:   u32 = 9;
const DM_TYPE_UINT8:  u32 = 10;
const DM_TYPE_STRUCT: u32 = 15;
const DM_TYPE_STRING: u32 = 18;
const DM_TYPE_ARRAY:  u32 = 20;

#[derive(Debug, Clone)]
#[allow(dead_code)]
enum DmValue {
    Int(i64),
    Uint(u64),
    Float(f64),
    Bool(bool),
    Str(String),
    Group(Vec<(String, DmValue)>),
    Array(Vec<DmValue>),
    Bytes(Vec<u8>),  // raw image data
}

impl DmValue {
    fn as_i64(&self) -> Option<i64> {
        match self {
            DmValue::Int(v) => Some(*v),
            DmValue::Uint(v) => Some(*v as i64),
            _ => None,
        }
    }

    fn as_u64(&self) -> Option<u64> {
        match self {
            DmValue::Uint(v) => Some(*v),
            DmValue::Int(v) => Some(*v as u64),
            _ => None,
        }
    }

    fn as_group(&self) -> Option<&[(String, DmValue)]> {
        match self {
            DmValue::Group(v) => Some(v),
            _ => None,
        }
    }

    fn get(&self, key: &str) -> Option<&DmValue> {
        self.as_group()?.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }
}

// ── Binary reader helpers ─────────────────────────────────────────────────────
struct DmReader<R: Read + Seek> {
    r: R,
    dm4: bool,
    le: bool, // data endianness (NOT the file's fixed big-endian structure parts)
}

impl<R: Read + Seek> DmReader<R> {
    // Header fields are big-endian regardless of data endianness
    fn read_u8(&mut self) -> std::io::Result<u8> {
        let mut b = [0u8];
        self.r.read_exact(&mut b)?;
        Ok(b[0])
    }
    fn read_be_u16(&mut self) -> std::io::Result<u16> {
        let mut b = [0u8; 2];
        self.r.read_exact(&mut b)?;
        Ok(u16::from_be_bytes(b))
    }
    fn read_be_u32(&mut self) -> std::io::Result<u32> {
        let mut b = [0u8; 4];
        self.r.read_exact(&mut b)?;
        Ok(u32::from_be_bytes(b))
    }
    fn read_be_u64(&mut self) -> std::io::Result<u64> {
        let mut b = [0u8; 8];
        self.r.read_exact(&mut b)?;
        Ok(u64::from_be_bytes(b))
    }
    /// Tag count: uint32 for DM3, uint64 for DM4
    fn read_tag_count(&mut self) -> std::io::Result<u64> {
        if self.dm4 {
            self.read_be_u64()
        } else {
            Ok(self.read_be_u32()? as u64)
        }
    }

    // Data values respect the file's declared endianness
    fn read_data_i16(&mut self) -> std::io::Result<i16> {
        let mut b = [0u8; 2];
        self.r.read_exact(&mut b)?;
        Ok(if self.le { i16::from_le_bytes(b) } else { i16::from_be_bytes(b) })
    }
    fn read_data_i32(&mut self) -> std::io::Result<i32> {
        let mut b = [0u8; 4];
        self.r.read_exact(&mut b)?;
        Ok(if self.le { i32::from_le_bytes(b) } else { i32::from_be_bytes(b) })
    }
    fn read_data_u16(&mut self) -> std::io::Result<u16> {
        let mut b = [0u8; 2];
        self.r.read_exact(&mut b)?;
        Ok(if self.le { u16::from_le_bytes(b) } else { u16::from_be_bytes(b) })
    }
    fn read_data_u32(&mut self) -> std::io::Result<u32> {
        let mut b = [0u8; 4];
        self.r.read_exact(&mut b)?;
        Ok(if self.le { u32::from_le_bytes(b) } else { u32::from_be_bytes(b) })
    }
    fn read_data_f32(&mut self) -> std::io::Result<f32> {
        let mut b = [0u8; 4];
        self.r.read_exact(&mut b)?;
        Ok(if self.le { f32::from_le_bytes(b) } else { f32::from_be_bytes(b) })
    }
    fn read_data_f64(&mut self) -> std::io::Result<f64> {
        let mut b = [0u8; 8];
        self.r.read_exact(&mut b)?;
        Ok(if self.le { f64::from_le_bytes(b) } else { f64::from_be_bytes(b) })
    }
    fn read_data_u8(&mut self) -> std::io::Result<u8> {
        self.read_u8()
    }
    fn read_data_i8(&mut self) -> std::io::Result<i8> {
        Ok(self.read_u8()? as i8)
    }

    /// Read a scalar value given its DM type code.
    fn read_scalar(&mut self, type_code: u32) -> std::io::Result<DmValue> {
        match type_code {
            DM_TYPE_INT16 => Ok(DmValue::Int(self.read_data_i16()? as i64)),
            DM_TYPE_INT32 => Ok(DmValue::Int(self.read_data_i32()? as i64)),
            DM_TYPE_UINT16 => Ok(DmValue::Uint(self.read_data_u16()? as u64)),
            DM_TYPE_UINT32 => Ok(DmValue::Uint(self.read_data_u32()? as u64)),
            DM_TYPE_FLOAT32 => Ok(DmValue::Float(self.read_data_f32()? as f64)),
            DM_TYPE_FLOAT64 => Ok(DmValue::Float(self.read_data_f64()?)),
            DM_TYPE_BOOL => Ok(DmValue::Bool(self.read_data_u8()? != 0)),
            DM_TYPE_INT8 => Ok(DmValue::Int(self.read_data_i8()? as i64)),
            DM_TYPE_UINT8 => Ok(DmValue::Uint(self.read_data_u8()? as u64)),
            _ => {
                // Unknown scalar: skip 4 bytes and return placeholder
                let mut b = [0u8; 4];
                let _ = self.r.read_exact(&mut b);
                Ok(DmValue::Int(0))
            }
        }
    }

    fn type_size(type_code: u32) -> usize {
        match type_code {
            DM_TYPE_INT8 | DM_TYPE_UINT8 | DM_TYPE_BOOL => 1,
            DM_TYPE_INT16 | DM_TYPE_UINT16 => 2,
            DM_TYPE_INT32 | DM_TYPE_UINT32 | DM_TYPE_FLOAT32 => 4,
            DM_TYPE_FLOAT64 => 8,
            _ => 4,
        }
    }

    /// Parse a tag leaf data block.
    fn parse_tag_data(&mut self) -> std::io::Result<DmValue> {
        // "%%%%"  (4 bytes delimiter)
        let mut delim = [0u8; 4];
        self.r.read_exact(&mut delim)?;

        // info_array_length (big-endian uint32 in DM3; DM4 uses uint64)
        let n_info = if self.dm4 {
            self.read_be_u64()? as usize
        } else {
            self.read_be_u32()? as usize
        };

        // Read info array (each entry is a big-endian uint32)
        let mut info = vec![0u32; n_info];
        for x in info.iter_mut() {
            *x = self.read_be_u32()?;
        }

        if info.is_empty() {
            return Ok(DmValue::Int(0));
        }

        match info[0] {
            DM_TYPE_STRUCT => {
                // Struct: info[1] = num_fields, then pairs (name_len, type_code)×num_fields
                // Field name lengths in info[2..], type codes follow each name_len
                // Actual layout: info[1]=num_fields, info[2..2+num_fields*2]=(field_name_len, field_type)
                let n_fields = if info.len() > 1 { info[1] as usize } else { 0 };
                let mut fields = Vec::with_capacity(n_fields);
                for i in 0..n_fields {
                    let base = 2 + i * 2;
                    let type_code = if base + 1 < info.len() { info[base + 1] } else { DM_TYPE_INT32 };
                    let val = self.read_scalar(type_code)?;
                    fields.push((format!("field{}", i), val));
                }
                Ok(DmValue::Group(fields))
            }
            DM_TYPE_STRING => {
                // String: info[1] = byte length
                let len = if info.len() > 1 { info[1] as usize } else { 0 };
                let mut bytes = vec![0u8; len];
                self.r.read_exact(&mut bytes)?;
                // DM strings are UTF-16 BE
                let s = if len >= 2 && len % 2 == 0 {
                    let words: Vec<u16> = bytes.chunks(2)
                        .map(|c| u16::from_be_bytes([c[0], c[1]]))
                        .collect();
                    String::from_utf16_lossy(&words).to_string()
                } else {
                    String::from_utf8_lossy(&bytes).to_string()
                };
                Ok(DmValue::Str(s))
            }
            DM_TYPE_ARRAY => {
                // Array: info[1] = element type, info[2] = element count
                let elem_type = if info.len() > 1 { info[1] } else { DM_TYPE_UINT8 };
                let elem_count = if info.len() > 2 {
                    info[2] as u64
                } else {
                    0
                };

                // For large arrays, read as raw bytes (likely pixel data)
                let elem_size = Self::type_size(elem_type);
                let total_bytes = elem_count as usize * elem_size;

                if elem_count > 4096 || elem_type == DM_TYPE_UINT8 || elem_type == DM_TYPE_INT16
                    || elem_type == DM_TYPE_UINT16 || elem_type == DM_TYPE_INT32
                    || elem_type == DM_TYPE_UINT32 || elem_type == DM_TYPE_FLOAT32
                    || elem_type == DM_TYPE_FLOAT64
                {
                    // Read as raw bytes
                    let mut data = vec![0u8; total_bytes];
                    self.r.read_exact(&mut data)?;
                    return Ok(DmValue::Bytes(data));
                }

                // Small arrays: read element by element
                let mut arr = Vec::with_capacity(elem_count as usize);
                for _ in 0..elem_count {
                    arr.push(self.read_scalar(elem_type)?);
                }
                Ok(DmValue::Array(arr))
            }
            type_code => {
                // Simple scalar
                self.read_scalar(type_code)
            }
        }
    }

    /// Parse a TagGroup (branch node).
    fn parse_tag_group(&mut self, depth: usize) -> std::io::Result<DmValue> {
        if depth > 20 {
            return Ok(DmValue::Group(vec![]));
        }
        let _is_sorted = self.read_u8()?;
        let _is_open = self.read_u8()?;
        let n_tags = self.read_tag_count()?;

        let mut entries = Vec::new();
        for _ in 0..n_tags {
            let tag_type = self.read_u8()?;
            let name_len = self.read_be_u16()? as usize;
            let mut name_bytes = vec![0u8; name_len];
            self.r.read_exact(&mut name_bytes)?;
            let name = String::from_utf8_lossy(&name_bytes).to_string();

            let val = match tag_type {
                20 => self.parse_tag_data()?,  // leaf
                21 => self.parse_tag_group(depth + 1)?,  // group/branch
                _ => DmValue::Int(0),
            };
            entries.push((name, val));
        }
        Ok(DmValue::Group(entries))
    }
}

// ── Parsed image info ─────────────────────────────────────────────────────────
struct DmImage {
    width: u32,
    height: u32,
    depth: u32,  // Z planes
    dm_data_type: i32,
    pixel_data: Vec<u8>,
    name: String,
}

fn find_image_data(root: &DmValue) -> Option<DmImage> {
    // Navigate: root → "ImageList" → entry 1 (or first if 1 is absent)
    let image_list = root.get("ImageList")?;
    let entries = image_list.as_group()?;

    // Try entry at index 1 first (index 0 is often a thumbnail/reference)
    // entries are in order, try index 1 (if present) then 0
    let candidates: Vec<usize> = if entries.len() > 1 {
        vec![1, 0]
    } else {
        vec![0]
    };

    for &idx in &candidates {
        if let Some((_, image_entry)) = entries.get(idx) {
            if let Some(result) = extract_image(image_entry) {
                return Some(result);
            }
        }
    }
    None
}

fn extract_image(entry: &DmValue) -> Option<DmImage> {
    let img_data = entry.get("ImageData")?;

    // Get dimensions
    let dims = img_data.get("Dimensions")?;
    let dim_entries = dims.as_group()?;
    let width = dim_entries.get(0)?.1.as_u64()? as u32;
    let height = dim_entries.get(1).map(|(_, v)| v.as_u64().unwrap_or(1) as u32).unwrap_or(1);
    let depth = dim_entries.get(2).map(|(_, v)| v.as_u64().unwrap_or(1) as u32).unwrap_or(1);

    // Get data type
    let dm_data_type = img_data.get("DataType")
        .and_then(|v| v.as_i64())
        .unwrap_or(23) as i32;  // default to uint8

    // Get pixel data
    let data_tag = img_data.get("Data")?;
    let pixel_data = match data_tag {
        DmValue::Bytes(b) => b.clone(),
        _ => return None,
    };

    // Get image name
    let name = entry.get("Name")
        .and_then(|v| if let DmValue::Str(s) = v { Some(s.clone()) } else { None })
        .unwrap_or_default();

    Some(DmImage { width, height, depth, dm_data_type, pixel_data, name })
}

// ── Reader ────────────────────────────────────────────────────────────────────

pub struct GatanReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    pixel_data: Option<Vec<u8>>,
    dm_data_type: i32,
}

impl GatanReader {
    pub fn new() -> Self {
        GatanReader { path: None, meta: None, pixel_data: None, dm_data_type: 23 }
    }
}

impl Default for GatanReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for GatanReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension().and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("dm3") | Some("dm4") | Some("dm2"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        if header.len() < 4 { return false; }
        // DM3: first 4 bytes big-endian = 3
        // DM4: first 4 bytes big-endian = 4
        let v = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
        v == 3 || v == 4
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let f = File::open(path).map_err(BioFormatsError::Io)?;
        let r = BufReader::new(f);

        // Read header
        let mut header = [0u8; 16];
        {
            let mut rf = File::open(path).map_err(BioFormatsError::Io)?;
            rf.read_exact(&mut header).map_err(BioFormatsError::Io)?;
        }

        let version = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
        let dm4 = version == 4;

        // Byte order field (big-endian uint32): 0=big-endian, 1=little-endian
        let bo_off = if dm4 { 12 } else { 8 };
        let byte_order = u32::from_be_bytes([header[bo_off], header[bo_off+1], header[bo_off+2], header[bo_off+3]]);
        let le = byte_order == 1;

        let mut dm = DmReader { r, dm4, le };

        // Seek past the file header to the root tag group
        let _root_offset = if dm4 { 24u64 } else { 16u64 }; // version(4) + size(4/8) + byteorder(4)
        // Actually:
        // DM3: version(4) + filesize(4) + byteorder(4) = 12 bytes → root at 12
        // DM4: version(4) + filesize(8) + byteorder(4) = 16 bytes → root at 16
        let root_off = if dm4 { 16u64 } else { 12u64 };
        dm.r.seek(SeekFrom::Start(root_off)).map_err(BioFormatsError::Io)?;

        let root = dm.parse_tag_group(0).map_err(BioFormatsError::Io)?;

        let img = find_image_data(&root)
            .ok_or_else(|| BioFormatsError::Format("DM3/DM4: could not find image data in tag tree".into()))?;

        let pixel_type = dm_pixel_type(img.dm_data_type);
        let image_count = img.depth.max(1);

        let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
        if !img.name.is_empty() {
            meta_map.insert("name".into(), MetadataValue::String(img.name));
        }
        meta_map.insert("dm_version".into(), MetadataValue::Int(version as i64));
        meta_map.insert("dm_data_type".into(), MetadataValue::Int(img.dm_data_type as i64));

        let meta = ImageMetadata {
            size_x: img.width,
            size_y: img.height,
            size_z: image_count,
            size_c: 1,
            size_t: 1,
            pixel_type,
            bits_per_pixel: (dm_bytes_per_pixel(img.dm_data_type) * 8) as u8,
            image_count,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: le,
            resolution_count: 1,
            series_metadata: meta_map,
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        };

        self.meta = Some(meta);
        self.pixel_data = Some(img.pixel_data);
        self.dm_data_type = img.dm_data_type;
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.pixel_data = None;
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

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let data = self.pixel_data.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let bps = meta.pixel_type.bytes_per_sample();
        let plane_bytes = (meta.size_x * meta.size_y) as usize * bps;
        let start = plane_index as usize * plane_bytes;
        let end = start + plane_bytes;
        if end > data.len() {
            return Err(BioFormatsError::InvalidData("DM plane out of range in data".into()));
        }
        Ok(data[start..end].to_vec())
    }

    fn open_bytes_region(&mut self, plane_index: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let full = self.open_bytes(plane_index)?;
        let meta = self.meta.as_ref().unwrap();
        let bps = meta.pixel_type.bytes_per_sample();
        let row_bytes = meta.size_x as usize * bps;
        let out_row = w as usize * bps;
        let mut out = Vec::with_capacity(h as usize * out_row);
        for row in 0..h as usize {
            let src = &full[(y as usize + row) * row_bytes..];
            let s = x as usize * bps;
            out.extend_from_slice(&src[s..s + out_row]);
        }
        Ok(out)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = ((meta.size_x - tw) / 2, (meta.size_y - th) / 2);
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        use crate::common::metadata::MetadataValue;
        use crate::common::ome_metadata::OmeMetadata;
        let meta = self.meta.as_ref()?;
        let mut ome = OmeMetadata::from_image_metadata(meta);
        let img = &mut ome.images[0];
        if let Some(MetadataValue::String(n)) = meta.series_metadata.get("name") {
            img.name = Some(n.clone());
        }
        Some(ome)
    }
}

// ── Gatan DM2 Reader ──────────────────────────────────────────────────────────

pub struct Dm2Reader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    data_offset: u64,
}

impl Dm2Reader {
    pub fn new() -> Self {
        Dm2Reader { path: None, meta: None, data_offset: 32 }
    }
}

impl Default for Dm2Reader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for Dm2Reader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension().and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("dm2"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        // Extension-only detection for DM2
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
        let mut header = [0u8; 32];
        f.read_exact(&mut header).map_err(BioFormatsError::Io)?;

        let width = i32::from_le_bytes([header[4], header[5], header[6], header[7]]).max(1) as u32;
        let height = i32::from_le_bytes([header[8], header[9], header[10], header[11]]).max(1) as u32;
        let dm_data_type = i32::from_le_bytes([header[12], header[13], header[14], header[15]]);

        let pixel_type = dm_pixel_type(dm_data_type);
        let bps = dm_bytes_per_pixel(dm_data_type);

        let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
        meta_map.insert("dm_data_type".into(), MetadataValue::Int(dm_data_type as i64));

        let meta = ImageMetadata {
            size_x: width,
            size_y: height,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type,
            bits_per_pixel: (bps * 8) as u8,
            image_count: 1,
            dimension_order: DimensionOrder::XYZCT,
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
        };

        self.meta = Some(meta);
        self.data_offset = 32;
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.data_offset = 32;
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

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let bps = meta.pixel_type.bytes_per_sample();
        let plane_bytes = meta.size_x as usize * meta.size_y as usize * bps;
        let file_offset = self.data_offset + plane_index as u64 * plane_bytes as u64;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
        use std::io::Seek;
        f.seek(std::io::SeekFrom::Start(file_offset)).map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; plane_bytes];
        f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
        Ok(buf)
    }

    fn open_bytes_region(&mut self, plane_index: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let full = self.open_bytes(plane_index)?;
        let meta = self.meta.as_ref().unwrap();
        let bps = meta.pixel_type.bytes_per_sample();
        let row_bytes = meta.size_x as usize * bps;
        let out_row = w as usize * bps;
        let mut out = Vec::with_capacity(h as usize * out_row);
        for row in 0..h as usize {
            let src = &full[(y as usize + row) * row_bytes..];
            let s = x as usize * bps;
            out.extend_from_slice(&src[s..s + out_row]);
        }
        Ok(out)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = ((meta.size_x - tw) / 2, (meta.size_y - th) / 2);
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}
