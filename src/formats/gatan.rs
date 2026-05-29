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
use crate::common::region::crop_full_plane;

// ── DM image data types ───────────────────────────────────────────────────────
fn dm_pixel_type_and_bytes(dm_type: i32) -> Result<(PixelType, usize)> {
    match dm_type {
        1 => Ok((PixelType::Int16, 2)),
        2 => Ok((PixelType::Float32, 4)),
        6 => Ok((PixelType::Uint8, 1)),
        7 => Ok((PixelType::Int32, 4)),
        9 => Ok((PixelType::Int8, 1)),
        10 => Ok((PixelType::Uint16, 2)),
        11 => Ok((PixelType::Uint32, 4)),
        12 => Ok((PixelType::Float64, 8)),
        23 => Ok((PixelType::Uint8, 1)),
        other => Err(BioFormatsError::UnsupportedFormat(format!(
            "Gatan DM unsupported data type {other}"
        ))),
    }
}

// ── Tag value types (DM encoding) ─────────────────────────────────────────────
// info[0] encodes the tag data type:
const DM_TYPE_INT16: u32 = 2;
const DM_TYPE_INT32: u32 = 3;
const DM_TYPE_UINT16: u32 = 4;
const DM_TYPE_UINT32: u32 = 5;
const DM_TYPE_FLOAT32: u32 = 6;
const DM_TYPE_FLOAT64: u32 = 7;
const DM_TYPE_INT8: u32 = 8;
const DM_TYPE_UINT8: u32 = 9;
const DM_TYPE_CHAR: u32 = 10;
const DM_TYPE_INT64: u32 = 11;
const DM_TYPE_UINT64: u32 = 12;
const DM_TYPE_STRUCT: u32 = 15;
const DM_TYPE_ARRAY: u32 = 20;

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
    Bytes(Vec<u8>), // raw image data
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
        self.as_group()?
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
    }
}

// ── Binary reader helpers ─────────────────────────────────────────────────────
struct DmReader<R: Read + Seek> {
    r: R,
    dm4: bool,
    le: bool, // declared file byte order (m.littleEndian in Java)
    // Java: when adjust_endianness is true, 4/8-byte structural scalars and the
    // Dimensions ints are read with the opposite byte order (in.order(!le)).
    adjust_endianness: bool,
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
    fn skip_dm4_padding(&mut self) -> std::io::Result<()> {
        if self.dm4 {
            self.r.seek(SeekFrom::Current(4))?;
        }
        Ok(())
    }

    fn skip_bytes(&mut self, n: u64) -> std::io::Result<()> {
        self.r.seek(SeekFrom::Current(n as i64))?;
        Ok(())
    }

    fn stream_len(&mut self) -> std::io::Result<u64> {
        let pos = self.r.stream_position()?;
        let len = self.r.seek(SeekFrom::End(0))?;
        self.r.seek(SeekFrom::Start(pos))?;
        Ok(len)
    }

    /// Effective endianness for a 4/8-byte structural scalar value: Java flips
    /// the byte order (in.order(!le)) when adjust_endianness is set.
    fn flipped_le(&self) -> bool {
        if self.adjust_endianness {
            !self.le
        } else {
            self.le
        }
    }

    // Data values respect the file's declared endianness
    fn read_data_i16(&mut self) -> std::io::Result<i16> {
        let mut b = [0u8; 2];
        self.r.read_exact(&mut b)?;
        Ok(if self.le {
            i16::from_le_bytes(b)
        } else {
            i16::from_be_bytes(b)
        })
    }
    #[allow(dead_code)]
    fn read_data_i32(&mut self) -> std::io::Result<i32> {
        let mut b = [0u8; 4];
        self.r.read_exact(&mut b)?;
        Ok(if self.le {
            i32::from_le_bytes(b)
        } else {
            i32::from_be_bytes(b)
        })
    }
    fn read_data_u16(&mut self) -> std::io::Result<u16> {
        let mut b = [0u8; 2];
        self.r.read_exact(&mut b)?;
        Ok(if self.le {
            u16::from_le_bytes(b)
        } else {
            u16::from_be_bytes(b)
        })
    }
    #[allow(dead_code)]
    fn read_data_u32(&mut self) -> std::io::Result<u32> {
        let mut b = [0u8; 4];
        self.r.read_exact(&mut b)?;
        Ok(if self.le {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        })
    }
    #[allow(dead_code)]
    fn read_data_f32(&mut self) -> std::io::Result<f32> {
        let mut b = [0u8; 4];
        self.r.read_exact(&mut b)?;
        Ok(if self.le {
            f32::from_le_bytes(b)
        } else {
            f32::from_be_bytes(b)
        })
    }
    #[allow(dead_code)]
    fn read_data_f64(&mut self) -> std::io::Result<f64> {
        let mut b = [0u8; 8];
        self.r.read_exact(&mut b)?;
        Ok(if self.le {
            f64::from_le_bytes(b)
        } else {
            f64::from_be_bytes(b)
        })
    }
    fn read_data_u8(&mut self) -> std::io::Result<u8> {
        self.read_u8()
    }
    fn read_data_i8(&mut self) -> std::io::Result<i8> {
        Ok(self.read_u8()? as i8)
    }

    /// Read a scalar value given its DM type code, matching Java readValue().
    /// 1/2-byte values use the base byte order; 4/8-byte values are read with
    /// the flipped order when adjust_endianness is set.
    fn read_scalar(&mut self, type_code: u32) -> std::io::Result<DmValue> {
        let flip = self.flipped_le();
        match type_code {
            // 2-byte: base order (no flip)
            DM_TYPE_INT16 => Ok(DmValue::Int(self.read_data_i16()? as i64)),
            DM_TYPE_UINT16 => Ok(DmValue::Uint(self.read_data_u16()? as u64)),
            // 4-byte: flipped order
            DM_TYPE_INT32 => {
                let mut b = [0u8; 4];
                self.r.read_exact(&mut b)?;
                let v = if flip {
                    i32::from_le_bytes(b)
                } else {
                    i32::from_be_bytes(b)
                };
                Ok(DmValue::Int(v as i64))
            }
            DM_TYPE_UINT32 => {
                let mut b = [0u8; 4];
                self.r.read_exact(&mut b)?;
                let v = if flip {
                    u32::from_le_bytes(b)
                } else {
                    u32::from_be_bytes(b)
                };
                Ok(DmValue::Uint(v as u64))
            }
            DM_TYPE_FLOAT32 => {
                let mut b = [0u8; 4];
                self.r.read_exact(&mut b)?;
                let v = if flip {
                    f32::from_le_bytes(b)
                } else {
                    f32::from_be_bytes(b)
                };
                Ok(DmValue::Float(v as f64))
            }
            // 8-byte: flipped order
            DM_TYPE_FLOAT64 => {
                let mut b = [0u8; 8];
                self.r.read_exact(&mut b)?;
                let v = if flip {
                    f64::from_le_bytes(b)
                } else {
                    f64::from_be_bytes(b)
                };
                Ok(DmValue::Float(v))
            }
            // 1-byte: base order (no flip)
            DM_TYPE_INT8 => Ok(DmValue::Int(self.read_data_i8()? as i64)),
            DM_TYPE_UINT8 => Ok(DmValue::Uint(self.read_data_u8()? as u64)),
            DM_TYPE_CHAR => Ok(DmValue::Uint(self.read_data_u8()? as u64)),
            // 8-byte unknown types: flipped order
            DM_TYPE_INT64 => {
                let mut b = [0u8; 8];
                self.r.read_exact(&mut b)?;
                Ok(DmValue::Int(if flip {
                    i64::from_le_bytes(b)
                } else {
                    i64::from_be_bytes(b)
                }))
            }
            DM_TYPE_UINT64 => {
                let mut b = [0u8; 8];
                self.r.read_exact(&mut b)?;
                Ok(DmValue::Uint(if flip {
                    u64::from_le_bytes(b)
                } else {
                    u64::from_be_bytes(b)
                }))
            }
            other => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unsupported DM scalar type {other}"),
            )),
        }
    }

    fn type_size(type_code: u32) -> usize {
        match type_code {
            DM_TYPE_INT8 | DM_TYPE_UINT8 | DM_TYPE_CHAR => 1,
            DM_TYPE_INT16 | DM_TYPE_UINT16 => 2,
            DM_TYPE_INT32 | DM_TYPE_UINT32 | DM_TYPE_FLOAT32 => 4,
            DM_TYPE_FLOAT64 | DM_TYPE_INT64 | DM_TYPE_UINT64 => 8,
            _ => 4,
        }
    }

    /// Parse a tag leaf data block.
    fn parse_tag_data(&mut self, label: &str) -> std::io::Result<DmValue> {
        self.skip_dm4_padding()?;
        self.skip_dm4_padding()?;

        // "%%%%"  (4 bytes delimiter)
        let mut delim = [0u8; 4];
        self.r.read_exact(&mut delim)?;

        self.skip_dm4_padding()?;
        let n_info = self.read_be_u32()?;
        self.skip_dm4_padding()?;
        let data_type = self.read_be_u32()?;

        match n_info {
            0 => Ok(DmValue::Int(0)),
            1 => self.read_scalar(data_type),
            2 => {
                let len = self.read_be_u32()? as usize;
                let mut bytes = vec![0u8; len];
                self.r.read_exact(&mut bytes)?;
                Ok(DmValue::Str(String::from_utf8_lossy(&bytes).to_string()))
            }
            3 if data_type == DM_TYPE_ARRAY => {
                self.skip_dm4_padding()?;
                let elem_type = self.read_be_u32()?;
                let elem_count = if self.dm4 {
                    self.read_be_u64()?
                } else {
                    self.read_be_u32()? as u64
                };
                let total_bytes = elem_count
                    .checked_mul(Self::type_size(elem_type) as u64)
                    .ok_or_else(|| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "DM array byte overflow",
                        )
                    })?;
                let pos = self.r.stream_position()?;
                let len = self.stream_len()?;
                if total_bytes > len.saturating_sub(pos) {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "DM array length exceeds remaining file bytes",
                    ));
                }

                if label == "Data" {
                    let mut data = vec![0u8; total_bytes as usize];
                    self.r.read_exact(&mut data)?;
                    Ok(DmValue::Bytes(data))
                } else {
                    self.skip_bytes(total_bytes)?;
                    Ok(DmValue::Int(0))
                }
            }
            _ if data_type == DM_TYPE_STRUCT => {
                self.skip_bytes(4)?;
                self.skip_dm4_padding()?;
                self.skip_dm4_padding()?;
                let n_fields = self.read_be_u32()? as usize;
                let start_fp = self.r.stream_position()?;
                self.skip_bytes(4)?;
                self.skip_dm4_padding()?;
                let mut base_fp = self.r.stream_position()?;
                if self.dm4 {
                    base_fp += 4;
                }
                let width = if self.dm4 { 16 } else { 8 };
                let mut field_types = Vec::with_capacity(n_fields);
                for i in 0..n_fields {
                    self.r.seek(SeekFrom::Start(base_fp + i as u64 * width))?;
                    field_types.push(self.read_be_u32()?);
                }
                self.r
                    .seek(SeekFrom::Start(start_fp + n_fields as u64 * width))?;
                let mut fields = Vec::with_capacity(n_fields);
                for (i, type_code) in field_types.into_iter().enumerate() {
                    let val = self.read_scalar(type_code)?;
                    fields.push((format!("field{}", i), val));
                }
                Ok(DmValue::Group(fields))
            }
            _ if data_type == DM_TYPE_ARRAY => {
                self.skip_dm4_padding()?;
                let nested_type = self.read_be_u32()?;
                if nested_type == DM_TYPE_STRUCT {
                    self.skip_bytes(4)?;
                    self.skip_dm4_padding()?;
                    self.skip_dm4_padding()?;
                    let n_fields = self.read_be_u32()? as usize;
                    let mut field_types = Vec::with_capacity(n_fields);
                    let mut base_fp = self.r.stream_position()? + 12;
                    if self.dm4 {
                        base_fp = self.r.stream_position()? + 12;
                    }
                    for i in 0..n_fields {
                        self.skip_bytes(4)?;
                        if self.dm4 {
                            self.r.seek(SeekFrom::Start(base_fp + i as u64 * 16))?;
                        }
                        field_types.push(self.read_be_u32()?);
                    }
                    self.skip_dm4_padding()?;
                    let len = self.read_be_u32()? as usize;
                    for _ in 0..len {
                        for &type_code in &field_types {
                            let _ = self.read_scalar(type_code)?;
                        }
                    }
                }
                Ok(DmValue::Int(0))
            }
            _ => Ok(DmValue::Int(0)),
        }
    }

    /// Parse a TagGroup (branch node).
    fn parse_tag_group(&mut self, depth: usize) -> std::io::Result<DmValue> {
        if depth > 20 {
            return Ok(DmValue::Group(vec![]));
        }
        let _is_sorted = self.read_u8()?;
        let _is_open = self.read_u8()?;
        self.skip_dm4_padding()?;
        if depth > 0 {
            self.skip_dm4_padding()?;
            self.skip_dm4_padding()?;
        }
        let n_tags = self.read_be_u32()? as u64;

        // Java: if numTags > in.length() the declared byte order is wrong, so
        // flip m.littleEndian and disable adjust_endianness.
        if depth == 0 {
            let len = self.stream_len()?;
            if n_tags > len {
                self.le = !self.le;
                self.adjust_endianness = false;
            }
        }

        let mut entries = Vec::new();
        for _ in 0..n_tags {
            let tag_type = self.read_u8()?;
            let name_len = self.read_be_u16()? as usize;
            let mut name_bytes = vec![0u8; name_len];
            self.r.read_exact(&mut name_bytes)?;
            let name = String::from_utf8_lossy(&name_bytes).to_string();

            let val = match tag_type {
                20 => self.parse_tag_group(depth + 1)?, // group/branch
                21 => self.parse_tag_data(&name)?,      // leaf
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
    depth: u32, // Z planes
    dm_data_type: i32,
    pixel_data: Vec<u8>,
    name: String,
}

fn find_image_data(root: &DmValue) -> Result<DmImage> {
    // Navigate: root → "ImageList" → entry 1 (or first if 1 is absent)
    let image_list = root
        .get("ImageList")
        .ok_or_else(|| BioFormatsError::Format("DM3/DM4: ImageList missing".into()))?;
    let entries = image_list
        .as_group()
        .ok_or_else(|| BioFormatsError::Format("DM3/DM4: ImageList is not a group".into()))?;

    // Try entry at index 1 first (index 0 is often a thumbnail/reference)
    // entries are in order, try index 1 (if present) then 0
    let candidates: Vec<usize> = if entries.len() > 1 {
        vec![1, 0]
    } else {
        vec![0]
    };

    for &idx in &candidates {
        if let Some((_, image_entry)) = entries.get(idx) {
            match extract_image(image_entry)? {
                Some(result) => return Ok(result),
                None => {}
            }
        }
    }
    Err(BioFormatsError::Format(
        "DM3/DM4: could not find image data in tag tree".into(),
    ))
}

fn extract_image(entry: &DmValue) -> Result<Option<DmImage>> {
    let img_data = match entry.get("ImageData") {
        Some(value) => value,
        None => return Ok(None),
    };

    // Get dimensions
    let dims = img_data.get("Dimensions").ok_or_else(|| {
        BioFormatsError::UnsupportedFormat("Gatan DM ImageData has no Dimensions".into())
    })?;
    let dim_entries = dims.as_group().ok_or_else(|| {
        BioFormatsError::UnsupportedFormat("Gatan DM Dimensions is not a group".into())
    })?;
    let width = dim_entries
        .first()
        .and_then(|(_, v)| v.as_u64())
        .ok_or_else(|| BioFormatsError::UnsupportedFormat("Gatan DM width missing".into()))?;
    let height = dim_entries
        .get(1)
        .and_then(|(_, v)| v.as_u64())
        .ok_or_else(|| BioFormatsError::UnsupportedFormat("Gatan DM height missing".into()))?;
    let depth = dim_entries
        .get(2)
        .and_then(|(_, v)| v.as_u64())
        .unwrap_or(1);
    if width == 0 || height == 0 || depth == 0 {
        return Err(BioFormatsError::UnsupportedFormat(
            "Gatan DM image has non-positive dimensions".into(),
        ));
    }
    let width = u32::try_from(width)
        .map_err(|_| BioFormatsError::Format("Gatan DM width overflows".into()))?;
    let height = u32::try_from(height)
        .map_err(|_| BioFormatsError::Format("Gatan DM height overflows".into()))?;
    let depth = u32::try_from(depth)
        .map_err(|_| BioFormatsError::Format("Gatan DM depth overflows".into()))?;

    // Get data type
    let dm_data_type = i32::try_from(
        img_data
            .get("DataType")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| {
                BioFormatsError::UnsupportedFormat("Gatan DM ImageData has no DataType".into())
            })?,
    )
    .map_err(|_| BioFormatsError::Format("Gatan DM data type overflows".into()))?;

    // Get pixel data
    let data_tag = img_data.get("Data").ok_or_else(|| {
        BioFormatsError::UnsupportedFormat("Gatan DM ImageData has no Data".into())
    })?;
    let pixel_data = match data_tag {
        DmValue::Bytes(b) => b.clone(),
        _ => return Ok(None),
    };

    // Get image name
    let name = entry
        .get("Name")
        .and_then(|v| {
            if let DmValue::Str(s) = v {
                Some(s.clone())
            } else {
                None
            }
        })
        .unwrap_or_default();

    Ok(Some(DmImage {
        width,
        height,
        depth,
        dm_data_type,
        pixel_data,
        name,
    }))
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
        GatanReader {
            path: None,
            meta: None,
            pixel_data: None,
            dm_data_type: 23,
        }
    }
}

impl Default for GatanReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for GatanReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("dm3") | Some("dm4") | Some("dm2"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        if header.len() < 16 {
            return false;
        }
        // DM3: first 4 bytes big-endian = 3
        // DM4: first 4 bytes big-endian = 4
        let v = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
        match v {
            3 => {
                let byte_order = u32::from_be_bytes([header[8], header[9], header[10], header[11]]);
                byte_order <= 1
            }
            4 => {
                let byte_order =
                    u32::from_be_bytes([header[12], header[13], header[14], header[15]]);
                byte_order <= 1
            }
            _ => false,
        }
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
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

        // Byte order field: Java does `m.littleEndian = in.readInt() != 1`.
        // So a value of 1 means big-endian; anything else means little-endian.
        let bo_off = if dm4 { 12 } else { 8 };
        let byte_order = u32::from_be_bytes([
            header[bo_off],
            header[bo_off + 1],
            header[bo_off + 2],
            header[bo_off + 3],
        ]);
        let le = byte_order != 1;

        // `adjustEndianness` is true unless the tag count looks invalid (see Java
        // GatanReader.initFile). When set, 4/8-byte structural scalars and the
        // Dimensions ints are read with the opposite byte order via flip helpers.
        let mut dm = DmReader {
            r,
            dm4,
            le,
            adjust_endianness: true,
        };

        // Seek past the file header to the root tag group
        let _root_offset = if dm4 { 24u64 } else { 16u64 }; // version(4) + size(4/8) + byteorder(4)
                                                            // Actually:
                                                            // DM3: version(4) + filesize(4) + byteorder(4) = 12 bytes → root at 12
                                                            // DM4: version(4) + filesize(8) + byteorder(4) = 16 bytes → root at 16
        let root_off = if dm4 { 16u64 } else { 12u64 };
        dm.r.seek(SeekFrom::Start(root_off))
            .map_err(BioFormatsError::Io)?;

        let root = dm.parse_tag_group(0).map_err(BioFormatsError::Io)?;

        let img = find_image_data(&root)?;

        let (pixel_type, bytes_per_pixel) = dm_pixel_type_and_bytes(img.dm_data_type)?;
        let image_count = img.depth;
        let expected_len = (img.width as usize)
            .checked_mul(img.height as usize)
            .and_then(|px| px.checked_mul(image_count as usize))
            .and_then(|px| px.checked_mul(bytes_per_pixel))
            .ok_or_else(|| BioFormatsError::Format("Gatan DM pixel payload overflows".into()))?;
        if img.pixel_data.len() < expected_len {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "Gatan DM pixel payload is shorter than declared ({} < {expected_len})",
                img.pixel_data.len()
            )));
        }

        let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
        if !img.name.is_empty() {
            meta_map.insert("name".into(), MetadataValue::String(img.name));
        }
        meta_map.insert("dm_version".into(), MetadataValue::Int(version as i64));
        meta_map.insert(
            "dm_data_type".into(),
            MetadataValue::Int(img.dm_data_type as i64),
        );

        let meta = ImageMetadata {
            size_x: img.width,
            size_y: img.height,
            size_z: image_count,
            size_c: 1,
            size_t: 1,
            pixel_type,
            bits_per_pixel: (bytes_per_pixel * 8) as u8,
            image_count,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            // Java forces m.littleEndian = true before populating pixels
            // (GatanReader.initFile line 242); pixel data is always little-endian.
            is_little_endian: true,
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

    fn series_count(&self) -> usize {
        usize::from(self.meta.is_some())
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.meta.is_none() {
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
        self.meta
            .as_ref()
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let data = self
            .pixel_data
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?;
        let bps = meta.pixel_type.bytes_per_sample();
        let plane_bytes = (meta.size_x * meta.size_y) as usize * bps;
        let start = plane_index as usize * plane_bytes;
        let end = start + plane_bytes;
        if end > data.len() {
            return Err(BioFormatsError::InvalidData(
                "DM plane out of range in data".into(),
            ));
        }
        Ok(data[start..end].to_vec())
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
        crop_full_plane("Gatan", &full, meta, 1, x, y, w, h)
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

/// Magic int at the start of a DM2 file (big-endian): 0x003d0000.
const DM2_MAGIC_BYTES: i32 = 0x3d_0000;
/// DM2 pixel data offset (GatanDM2Reader.HEADER_SIZE).
const DM2_HEADER_SIZE: u64 = 24;

/// Map (bytes-per-pixel, signed) to a PixelType, matching
/// FormatTools.pixelTypeFromBytes(bpp, signed, /*fp=*/true) for the byte sizes
/// that occur in DM2 (the fp flag only selects float for 4/8-byte data).
fn dm2_pixel_type_from_bytes(bpp: i32, signed: bool) -> Result<PixelType> {
    match (bpp, signed) {
        (1, false) => Ok(PixelType::Uint8),
        (1, true) => Ok(PixelType::Int8),
        (2, false) => Ok(PixelType::Uint16),
        (2, true) => Ok(PixelType::Int16),
        // GatanDM2Reader passes fp=true, so 4-byte data is treated as float.
        (4, _) => Ok(PixelType::Float32),
        other => Err(BioFormatsError::Format(format!(
            "DM2: unsupported bytes-per-pixel/signed combination {other:?}"
        ))),
    }
}

/// Cursor over a big-endian byte slice mirroring the subset of
/// RandomAccessInputStream operations used by GatanDM2Reader.
struct Be<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Be<'a> {
    fn new(data: &'a [u8], pos: usize) -> Self {
        Be { data, pos }
    }
    fn len(&self) -> usize {
        self.data.len()
    }
    fn pointer(&self) -> usize {
        self.pos
    }
    fn seek(&mut self, p: usize) {
        self.pos = p.min(self.data.len());
    }
    fn skip(&mut self, n: usize) {
        self.pos = (self.pos + n).min(self.data.len());
    }
    fn read_u8(&mut self) -> i32 {
        if self.pos >= self.data.len() {
            return -1;
        }
        let v = self.data[self.pos] as i32;
        self.pos += 1;
        v
    }
    fn read_short(&mut self) -> i32 {
        if self.pos + 2 > self.data.len() {
            self.pos = self.data.len();
            return 0;
        }
        let v = i16::from_be_bytes([self.data[self.pos], self.data[self.pos + 1]]) as i32;
        self.pos += 2;
        v
    }
    fn read_int(&mut self) -> i32 {
        if self.pos + 4 > self.data.len() {
            self.pos = self.data.len();
            return 0;
        }
        let v = i32::from_be_bytes([
            self.data[self.pos],
            self.data[self.pos + 1],
            self.data[self.pos + 2],
            self.data[self.pos + 3],
        ]);
        self.pos += 4;
        v
    }
    /// Read `len` bytes as an ISO-8859-1 string.
    fn read_string(&mut self, len: usize) -> String {
        let end = (self.pos + len).min(self.data.len());
        let s: String = self.data[self.pos..end]
            .iter()
            .map(|&b| b as char)
            .collect();
        self.pos = end;
        s
    }
}

/// Port of the GatanDM2Reader.initFile label/value scan plus parseExtraTags.
/// Collects all label/value pairs into `meta` and returns (name, date, time).
fn parse_dm2_metadata(
    bytes: &[u8],
    start: usize,
    meta: &mut HashMap<String, MetadataValue>,
) -> (Option<String>, Option<String>, Option<String>) {
    let mut s = Be::new(bytes, start);
    let mut date: Option<String> = None;
    let mut time: Option<String> = None;
    let mut name: Option<String> = None;

    while s.pointer() < s.len() {
        let mut strlen = s.read_short();
        if strlen == 0 || strlen > 255 {
            s.skip(35);
            strlen = s.read_short();
            if strlen < 0 || (strlen as usize) + s.pointer() >= s.len() {
                let back = s.pointer().saturating_sub(10);
                s.seek(back);
                strlen = s.read_short();
            }
        }
        if strlen < 0 || (strlen as usize) + s.pointer() >= s.len() {
            break;
        }
        let mut label = s.read_string(strlen as usize);
        let mut value = String::new();

        let mut block = s.read_int();
        if block == 5 {
            s.skip(33);
            if s.read_short() == 0 {
                if s.read_short() == 39 {
                    s.skip(1);
                } else {
                    s.skip(2);
                }
            } else {
                let back = s.pointer().saturating_sub(2);
                s.seek(back);
                continue;
            }
        } else if block == 0 || (block as i64 > 0xffff && block < 0x0100_0000) {
            if block != 0 && strlen > 0 {
                let back = s.pointer().saturating_sub(4);
                s.seek(back);
                value.push_str(&label);
                label = "Description".to_string();
                meta.insert(label.clone(), MetadataValue::String(value.clone()));
            } else if block != 0 {
                s.skip(15);
            }
            parse_dm2_extra_tags(&mut s, meta);
            continue;
        } else if block >= 0x0100_0000 {
            s.skip(34);
            strlen = s.read_short();
            if strlen < 0 || (strlen as usize) + s.pointer() >= s.len() {
                break;
            }
            label = s.read_string(strlen as usize);
            block = s.read_int();
            if block == 5 {
                s.skip(33);
                continue;
            }
        }

        let len = s.read_int();
        if len < 0 || (len as usize) + s.pointer() >= s.len() {
            break;
        }
        let type_str = s.read_string(len as usize);
        let _extra = s.read_int() - 2;
        let mut count = s.read_int();

        match type_str.as_str() {
            "TEXT" => {
                value.push_str(&s.read_string(count.max(0) as usize));
                if block == 5 {
                    s.skip(22);
                    if s.read_int() == 4 {
                        if s.read_string(4) == "TEXT" {
                            s.skip(4);
                            count = s.read_int();
                            value.push_str(", ");
                            value.push_str(&s.read_string(count.max(0) as usize));
                            s.skip(37);
                        } else {
                            s.skip(7);
                        }
                    } else {
                        s.skip(11);
                    }
                }
            }
            "long" => {
                count /= 8;
                for i in 0..count {
                    if s.pointer() + 8 > s.len() {
                        break;
                    }
                    let v = i64::from_be_bytes([
                        bytes[s.pointer()],
                        bytes[s.pointer() + 1],
                        bytes[s.pointer() + 2],
                        bytes[s.pointer() + 3],
                        bytes[s.pointer() + 4],
                        bytes[s.pointer() + 5],
                        bytes[s.pointer() + 6],
                        bytes[s.pointer() + 7],
                    ]);
                    s.skip(8);
                    value.push_str(&v.to_string());
                    if i < count - 1 {
                        value.push_str(", ");
                    }
                }
                s.skip(4);
            }
            "bool" => {
                for i in 0..count {
                    let v = s.read_u8() == 1;
                    value.push_str(&v.to_string());
                    if i < count - 1 {
                        value.push_str(", ");
                    }
                }
            }
            "shor" => {
                count /= 2;
                for i in 0..count {
                    value.push_str(&s.read_short().to_string());
                    if i < count - 1 {
                        value.push_str(", ");
                    }
                }
            }
            "sing" => {
                count /= 4;
                for i in 0..count {
                    if s.pointer() + 4 > s.len() {
                        break;
                    }
                    let v = f32::from_be_bytes([
                        bytes[s.pointer()],
                        bytes[s.pointer() + 1],
                        bytes[s.pointer() + 2],
                        bytes[s.pointer() + 3],
                    ]);
                    s.skip(4);
                    value.push_str(&v.to_string());
                    if i < count - 1 {
                        value.push_str(", ");
                    }
                }
            }
            _ => {
                if count < 0 || (count as usize) + s.pointer() > s.len() {
                    break;
                }
                s.skip(count as usize);
            }
        }

        s.skip(16);
        meta.insert(label.clone(), MetadataValue::String(value.clone()));

        match label.as_str() {
            "Acquisition Date" => {
                let mut d = value.clone();
                if let Some(slash) = d.rfind('/') {
                    let year = &d[slash + 1..];
                    if year.len() < 2 {
                        d = format!("{}0{}", &d[..slash + 1], year);
                    }
                }
                date = Some(d);
            }
            "Acquisition Time" => time = Some(value.clone()),
            "Name" => name = Some(value.clone()),
            _ => {}
        }
    }

    (name, date, time)
}

/// Port of GatanDM2Reader.parseExtraTags (reads to EOF).
fn parse_dm2_extra_tags(s: &mut Be<'_>, meta: &mut HashMap<String, MetadataValue>) {
    while s.pointer() < s.len() {
        let tag = s.read_short();
        let length = s.read_int();
        let value = if length == 4 {
            let p = s.pointer();
            if p + 4 > s.len() {
                break;
            }
            let v = f32::from_be_bytes([s.data[p], s.data[p + 1], s.data[p + 2], s.data[p + 3]]);
            s.skip(4);
            v.to_string()
        } else if length == 2 {
            s.read_short().to_string()
        } else if length == 1 {
            s.read_u8().to_string()
        } else {
            if length < 0 {
                break;
            }
            let raw = s.read_string(length as usize);
            match raw.find('\0') {
                Some(i) => raw[..i].to_string(),
                None => raw,
            }
        };
        let value = value.trim().to_string();

        let label = match tag {
            17 => "BlackContrastLimit".to_string(),
            18 => "WhiteContrastLimit".to_string(),
            22 => "Scale".to_string(),
            27 => "MaxPixelValue".to_string(),
            28 => "MinPixelValue".to_string(),
            31 => "Physical width".to_string(),
            32 => "Physical height".to_string(),
            37 => "Image label".to_string(),
            38 => "MinimumContrast".to_string(),
            53 => "Physical size units".to_string(),
            62 => "Origin".to_string(),
            other => format!("Tag {:x}", other),
        };
        meta.insert(label, MetadataValue::String(value));
    }
}

pub struct Dm2Reader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    data_offset: u64,
}

impl Dm2Reader {
    pub fn new() -> Self {
        Dm2Reader {
            path: None,
            meta: None,
            data_offset: DM2_HEADER_SIZE,
        }
    }
}

impl Default for Dm2Reader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for Dm2Reader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("dm2"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // GatanDM2Reader.isThisType: first big-endian int equals DM2_MAGIC_BYTES.
        header.len() >= 4
            && i32::from_be_bytes([header[0], header[1], header[2], header[3]]) == DM2_MAGIC_BYTES
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        // Port of GatanDM2Reader.initFile (big-endian, ISO-8859-1 strings).
        let bytes = std::fs::read(path).map_err(BioFormatsError::Io)?;
        if bytes.len() < 24 {
            return Err(BioFormatsError::Format("DM2 file is too short".into()));
        }

        let magic = i32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        if magic != DM2_MAGIC_BYTES {
            return Err(BioFormatsError::Format("Invalid DM2 file".into()));
        }

        // readInt() magic (4) + skipBytes(8) -> offset 12: footerOffset int + 16.
        // Then sizeX short(16), sizeY short(18), bpp short(20), signed short(22).
        let width_i = i16::from_be_bytes([bytes[16], bytes[17]]);
        let height_i = i16::from_be_bytes([bytes[18], bytes[19]]);
        if width_i <= 0 || height_i <= 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "DM2 header has non-positive image dimensions".into(),
            ));
        }
        let width = width_i as u32;
        let height = height_i as u32;
        let bpp = i16::from_be_bytes([bytes[20], bytes[21]]) as i32;
        let signed = i16::from_be_bytes([bytes[22], bytes[23]]) == 1;

        let pixel_type = dm2_pixel_type_from_bytes(bpp, signed)?;
        let bps = pixel_type.bytes_per_sample();

        let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();

        // Tag/metadata scan, starting after the pixel plane plus the 35-byte gap
        // that GatanDM2Reader skips (skipBytes(planeSize + 35)).
        let plane_size = (width as usize)
            .checked_mul(height as usize)
            .and_then(|px| px.checked_mul(bps))
            .ok_or_else(|| BioFormatsError::Format("DM2 plane size overflows".into()))?;
        let required_len = (DM2_HEADER_SIZE as usize)
            .checked_add(plane_size)
            .ok_or_else(|| BioFormatsError::Format("DM2 file size overflows".into()))?;
        if bytes.len() < required_len {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "DM2 pixel payload is shorter than declared ({} < {required_len})",
                bytes.len()
            )));
        }
        let scan_start = DM2_HEADER_SIZE as usize + plane_size + 35;
        let (name, date, time) = parse_dm2_metadata(&bytes, scan_start, &mut meta_map);
        if let Some(n) = name {
            meta_map.insert("Name".into(), MetadataValue::String(n));
        }
        if let Some(d) = date {
            meta_map.insert("Acquisition Date".into(), MetadataValue::String(d));
        }
        if let Some(t) = time {
            meta_map.insert("Acquisition Time".into(), MetadataValue::String(t));
        }

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
            // GatanDM2Reader sets m.littleEndian = false.
            is_little_endian: false,
            resolution_count: 1,
            series_metadata: meta_map,
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        };

        self.meta = Some(meta);
        self.data_offset = DM2_HEADER_SIZE;
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.data_offset = DM2_HEADER_SIZE;
        Ok(())
    }

    fn series_count(&self) -> usize {
        usize::from(self.meta.is_some())
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.meta.is_none() {
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
        self.meta
            .as_ref()
            .unwrap_or(crate::common::reader::uninitialized_metadata())
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
        f.seek(std::io::SeekFrom::Start(file_offset))
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
        crop_full_plane("DM2", &full, meta, 1, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = ((meta.size_x - tw) / 2, (meta.size_y - th) / 2);
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}
