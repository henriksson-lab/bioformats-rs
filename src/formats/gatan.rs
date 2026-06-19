//! Gatan DM3 / DM4 format reader (electron microscopy).
//!
//! Supports DM3 (version 3) and DM4 (version 4) Digital Micrograph files.
//! Reads the tag tree to find the primary image data (ImageList entry 1).

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::io::read_bytes_at;
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

/// Whether a DM DataType code denotes signed integer/float pixels.
///
/// The DM `DataType` codes encode signedness for the normal path: 1=Int16,
/// 9=Int8, 7=Int32 and 2/12 (float/double) are signed; 6=UInt8, 10=UInt16,
/// 11=UInt32 are unsigned. Java instead reads sign from the `LowLimit` tag
/// (`signed = LowLimit < 0`, GatanReader.java:719-720); gatan.rs does not
/// capture that tag; for reconciliation we also fall back to the DataType's own
/// signedness. For the RGB-ish DataType 23 (Java's `getNumBytes(23)==0`)
/// signedness is unknown, so we treat it as unsigned (matching the Uint8
/// default).
fn dm_data_type_is_signed(dm_type: i32) -> bool {
    matches!(dm_type, 1 | 2 | 7 | 9 | 12)
}

/// Map (bytes-per-pixel, signed) to a PixelType, mirroring
/// `FormatTools.pixelTypeFromBytes(bytes, signed, /*fp=*/false)`
/// (GatanReader.java:258). Used to reconcile the DataType-derived
/// bytes-per-pixel with the bpp implied by the stored payload length.
fn dm_pixel_type_from_bytes(bytes: usize, signed: bool) -> Result<PixelType> {
    let pt = match (bytes, signed) {
        (1, true) => PixelType::Int8,
        (1, false) => PixelType::Uint8,
        (2, true) => PixelType::Int16,
        (2, false) => PixelType::Uint16,
        (4, true) => PixelType::Int32,
        (4, false) => PixelType::Uint32,
        // 8 bytes: FormatTools maps to DOUBLE when fp, else to the widest
        // available integer. With fp=false and our PixelType set lacking a
        // 64-bit integer, fall back to Float64 to preserve the stride.
        (8, _) => PixelType::Float64,
        _ => {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "Gatan DM: cannot derive pixel type from {bytes} bytes/pixel (signed={signed})"
            )))
        }
    };
    Ok(pt)
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
    Bytes { offset: u64, len: u64 }, // raw image data location
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

    fn as_f64(&self) -> Option<f64> {
        match self {
            DmValue::Float(v) => Some(*v),
            DmValue::Int(v) => Some(*v as f64),
            DmValue::Uint(v) => Some(*v as f64),
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

// ── Annotation/ROI shape types ────────────────────────────────────────────────
// GatanReader shape-type codes (the "AnnotationType" leaf value), from
// GatanReader.java:82-86.
const SHAPE_LINE: i32 = 2;
const SHAPE_RECTANGLE: i32 = 5;
const SHAPE_ELLIPSE: i32 = 6;
const SHAPE_TEXT: i32 = 13;

/// Port of GatanReader's inner `ROIShape` class (GatanReader.java:873-882).
///
/// Annotation geometry collected from the DM tag tree under an
/// "AnnotationGroupList"/"DocumentObjectList" group. The `text`, `font_size`
/// and `stroke_color` fields mirror the Java members; they are captured for
/// fidelity even though the repo's `OmeShape` model carries only geometry.
#[derive(Debug, Clone, Default)]
struct RoiShape {
    /// Annotation type code (LINE / RECTANGLE / ELLIPSE / TEXT).
    type_code: i32,
    x1: f64,
    y1: f64,
    x2: f64,
    y2: f64,
    text: Option<String>,
    /// Font size in points ("FontSize" under a "TextFormat" group).
    font_size: Option<f64>,
    /// Stroke colour as (r, g, b) bytes ("ForegroundColor").
    stroke_color: Option<(u8, u8, u8)>,
}

// ── Binary reader helpers ─────────────────────────────────────────────────────
struct DmReader<R: Read + Seek> {
    r: R,
    dm4: bool,
    le: bool, // declared file byte order (m.littleEndian in Java)
    // Java: when adjust_endianness is true, 4/8-byte structural scalars and the
    // Dimensions ints are read with the opposite byte order (in.order(!le)).
    adjust_endianness: bool,
    // Physical pixel sizes (and matching units), collected in tree order from
    // "Scale"/"Units" leaves whose parent group is "Dimension" — mirroring
    // GatanReader.parseTags. Used to derive OME PhysicalSize{X,Y,Z}.
    pixel_sizes: Vec<f64>,
    units: Vec<String>,
    // Scalar data fields captured from named leaf tags, mirroring the
    // `addGlobalMeta`/special-case dispatch in GatanReader.parseTags. Each
    // corresponds to a Java member variable; populated where Java populates it.
    /// `signed` — derived from the "LowLimit" tag (`LowLimit < 0`).
    signed: bool,
    /// `gamma` — "Gamma" tag.
    gamma: Option<f64>,
    /// `mag` — "Indicated Magnification" tag (objective nominal magnification).
    mag: Option<f64>,
    /// `voltage` — "Voltage" tag (detector settings voltage, volts).
    voltage: Option<f64>,
    /// `info` — "Microscope Info" tag (used to derive acquisition mode).
    info: Option<String>,
    /// `posX`/`posY`/`posZ` — "xPos*"/"yPos*"/"Specimen position*" tags
    /// (plane stage positions, reference-frame units).
    pos_x: Option<f64>,
    pos_y: Option<f64>,
    pos_z: Option<f64>,
    /// `sampleTime` — "Sample Time" tag (plane exposure time, seconds).
    sample_time: Option<f64>,
    /// `timestamp` — "Acquisition Start Time (epoch)" tag.
    timestamp: Option<i64>,
    /// `foundMontage` — set when a "Montage" group is encountered.
    found_montage: bool,
    /// `stageX`/`stageY`/`stageZ` — per-tile "Stage X/Y/Z" leaves collected
    /// under a "Stage Position" group while inside a montage.
    stage_x: Vec<f64>,
    stage_y: Vec<f64>,
    stage_z: Vec<f64>,
    /// `shapes` — annotation ROIs collected from "AnnotationGroupList" /
    /// "DocumentObjectList" groups (GatanReader.java:113, 651-688).
    shapes: Vec<RoiShape>,
}

impl<R: Read + Seek> DmReader<R> {
    // Header fields are big-endian regardless of data endianness
    fn read_u8(&mut self) -> std::io::Result<u8> {
        let mut b = [0u8];
        self.r.read_exact(&mut b)?;
        Ok(b[0])
    }
    #[allow(dead_code)]
    fn read_be_u16(&mut self) -> std::io::Result<u16> {
        let mut b = [0u8; 2];
        self.r.read_exact(&mut b)?;
        Ok(u16::from_be_bytes(b))
    }
    #[allow(dead_code)]
    fn read_be_u32(&mut self) -> std::io::Result<u32> {
        let mut b = [0u8; 4];
        self.r.read_exact(&mut b)?;
        Ok(u32::from_be_bytes(b))
    }
    #[allow(dead_code)]
    fn read_be_u64(&mut self) -> std::io::Result<u64> {
        let mut b = [0u8; 8];
        self.r.read_exact(&mut b)?;
        Ok(u64::from_be_bytes(b))
    }
    // Structural integers (tag length/type, n_info, data_type, array/struct
    // counts) are read in the file's declared byte order. Java sets
    // in.order(isLittleEndian()) before parseTags and reads these with
    // readShort/readInt/readLong. Standard DM3/DM4 are big-endian (le=false),
    // so these match read_be_*; for LE-declared files they read little-endian.
    fn read_struct_u16(&mut self) -> std::io::Result<u16> {
        let mut b = [0u8; 2];
        self.r.read_exact(&mut b)?;
        Ok(if self.le {
            u16::from_le_bytes(b)
        } else {
            u16::from_be_bytes(b)
        })
    }
    fn read_struct_u32(&mut self) -> std::io::Result<u32> {
        let mut b = [0u8; 4];
        self.r.read_exact(&mut b)?;
        Ok(if self.le {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        })
    }
    fn read_struct_u64(&mut self) -> std::io::Result<u64> {
        let mut b = [0u8; 8];
        self.r.read_exact(&mut b)?;
        Ok(if self.le {
            u64::from_le_bytes(b)
        } else {
            u64::from_be_bytes(b)
        })
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
        let n_info = self.read_struct_u32()?;
        self.skip_dm4_padding()?;
        let data_type = self.read_struct_u32()?;

        match n_info {
            0 => Ok(DmValue::Int(0)),
            1 => self.read_scalar(data_type),
            2 => {
                let len = self.read_struct_u32()? as usize;
                let mut bytes = vec![0u8; len];
                self.r.read_exact(&mut bytes)?;
                Ok(DmValue::Str(String::from_utf8_lossy(&bytes).to_string()))
            }
            3 if data_type == DM_TYPE_ARRAY => {
                self.skip_dm4_padding()?;
                let elem_type = self.read_struct_u32()?;
                let elem_count = if self.dm4 {
                    self.read_struct_u64()?
                } else {
                    self.read_struct_u32()? as u64
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
                    let offset = self.r.stream_position()?;
                    self.skip_bytes(total_bytes)?;
                    Ok(DmValue::Bytes {
                        offset,
                        len: total_bytes,
                    })
                } else {
                    self.skip_bytes(total_bytes)?;
                    Ok(DmValue::Int(0))
                }
            }
            _ if data_type == DM_TYPE_STRUCT => {
                self.skip_bytes(4)?;
                self.skip_dm4_padding()?;
                self.skip_dm4_padding()?;
                let n_fields = self.read_struct_u32()? as usize;
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
                    field_types.push(self.read_struct_u32()?);
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
                let nested_type = self.read_struct_u32()?;
                if nested_type == DM_TYPE_STRUCT {
                    self.skip_bytes(4)?;
                    self.skip_dm4_padding()?;
                    self.skip_dm4_padding()?;
                    let n_fields = self.read_struct_u32()? as usize;
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
                        field_types.push(self.read_struct_u32()?);
                    }
                    self.skip_dm4_padding()?;
                    let len = self.read_struct_u32()? as usize;
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

    /// Parse a TagGroup (branch node). `parent` is the name of the enclosing
    /// group (for Scale/Units physical-size detection), like Java's `parent`.
    fn parse_tag_group(&mut self, depth: usize, parent: &str) -> std::io::Result<DmValue> {
        if depth > 20 {
            return Ok(DmValue::Group(vec![]));
        }
        // Java GatanReader.parseTags: a group whose parent is "Montage" marks the
        // file as a montage (foundMontage = true).
        if parent == "Montage" {
            self.found_montage = true;
        }
        let _is_sorted = self.read_u8()?;
        let _is_open = self.read_u8()?;
        self.skip_dm4_padding()?;
        if depth > 0 {
            self.skip_dm4_padding()?;
            self.skip_dm4_padding()?;
        }
        let n_tags = self.read_struct_u32()? as u64;

        // Java: if numTags > in.length() the declared byte order is wrong, so
        // flip m.littleEndian and disable adjust_endianness.
        if depth == 0 {
            let len = self.stream_len()?;
            if n_tags > len {
                self.le = !self.le;
                self.adjust_endianness = false;
            }
        }

        let stream_len = self.stream_len()?;
        let mut entries = Vec::new();
        // Java uses a `for (int i; i<numTags; i++)` loop where type-23 tags do
        // `i--` to re-read the index; mirror that with an explicit counter.
        let mut i: u64 = 0;
        while i < n_tags {
            // Java: if (in.getFilePointer() + 3 >= in.length()) break;
            if self.r.stream_position()? + 3 >= stream_len {
                break;
            }

            let tag_type = self.read_u8()?;

            // Java GatanReader.java:637-640: type 23 tags consume only 5 bytes
            // and re-read the same index (i--), instead of carrying a name.
            if tag_type == 23 {
                self.skip_bytes(5)?;
                continue;
            }

            let name_len = self.read_struct_u16()? as usize;
            let mut name_bytes = vec![0u8; name_len];
            self.r.read_exact(&mut name_bytes)?;
            let name = String::from_utf8_lossy(&name_bytes).to_string();

            let val = match tag_type {
                // Java passes the group's label as the new parent, but keeps the
                // current parent when the label is empty (GatanReader.java:633).
                20 => {
                    let child_parent = if name.is_empty() {
                        parent
                    } else {
                        name.as_str()
                    };
                    self.parse_tag_group(depth + 1, child_parent)?
                }
                21 => self.parse_tag_data(&name)?, // leaf
                _ => DmValue::Int(0),
            };

            // Capture annotation ROI shapes, mirroring the first if/else-if
            // chain in GatanReader.parseTags (GatanReader.java:651-688). This is
            // an independent dispatch from the named-field one below — Java runs
            // both for the same leaf — so it is called separately here.
            self.capture_roi_shape(parent, &name, &val);

            // Capture named scalar/string leaf data fields, mirroring the
            // special-case label dispatch in GatanReader.parseTags
            // (GatanReader.java:719-749, 689-704). Java keys off the leaf's
            // stringified `value`; we read the typed DmValue directly.
            self.capture_named_field(parent, &name, &val);

            // Physical pixel sizes: GatanReader collects "Scale" leaves whose
            // parent group is "Dimension" (validPhysicalSize), then "Units"
            // leaves, keeping units no longer than the size list. The OME value
            // is the raw scale, so no unit conversion is applied here.
            let valid_physical_size = parent == "Dimension"
                || ((self.pixel_sizes.len() == 4 || self.units.len() == 4) && parent == "2");
            if valid_physical_size {
                if name == "Scale" {
                    if let Some(v) = val.as_f64() {
                        self.pixel_sizes.push(v);
                    }
                } else if name == "Units" {
                    if self.pixel_sizes.len() == self.units.len() + 1 {
                        if let DmValue::Str(s) = &val {
                            self.units.push(s.clone());
                        }
                    }
                }
            }

            entries.push((name, val));
            i += 1;
        }
        Ok(DmValue::Group(entries))
    }

    /// Stringify a leaf value the way GatanReader does before `addGlobalMeta`
    /// (NUL characters stripped). Used for the string-valued tags ("Microscope
    /// Info") and as a uniform source for numeric parsing.
    fn leaf_string(val: &DmValue) -> Option<String> {
        match val {
            DmValue::Str(s) => Some(s.replace('\0', "")),
            DmValue::Int(v) => Some(v.to_string()),
            DmValue::Uint(v) => Some(v.to_string()),
            DmValue::Float(v) => Some(v.to_string()),
            _ => None,
        }
    }

    /// Extract the comma-separated component values of a struct leaf, the way
    /// GatanReader builds the joined `value` string for struct tags and then
    /// `value.split(",")`s it (GatanReader.java:584-591, 666-670, 676-679).
    ///
    /// "Rectangle" and "ForegroundColor" are DM structs, so in our parser they
    /// arrive as `DmValue::Group`; a comma-joined string fallback mirrors Java's
    /// purely string-based handling.
    fn struct_values(val: &DmValue) -> Vec<f64> {
        match val {
            DmValue::Group(fields) => fields.iter().filter_map(|(_, v)| v.as_f64()).collect(),
            DmValue::Str(s) => s
                .split(',')
                .filter_map(|p| p.trim().parse::<f64>().ok())
                .collect(),
            _ => val.as_f64().into_iter().collect(),
        }
    }

    /// Mirror of the annotation-ROI dispatch in GatanReader.parseTags
    /// (GatanReader.java:651-688). Shapes are accumulated into `self.shapes`:
    /// an "AnnotationType" leaf under an "AnnotationGroupList"/"DocumentObjectList"
    /// group starts a new shape; subsequent "Rectangle"/"Text"/"ForegroundColor"
    /// leaves mutate the most recent shape; a "FontSize" leaf under a
    /// "TextFormat" group sets its font size.
    fn capture_roi_shape(&mut self, parent: &str, label: &str, val: &DmValue) {
        if parent == "AnnotationGroupList" || parent == "DocumentObjectList" {
            // A new ROI begins at the AnnotationType leaf; otherwise we edit the
            // most recently started shape (Java: `shape = shapes.get(last)`).
            if label == "AnnotationType" {
                if let Some(v) = val.as_f64() {
                    self.shapes.push(RoiShape {
                        type_code: v as i32,
                        ..Default::default()
                    });
                }
                return;
            }
            let shape = match self.shapes.last_mut() {
                Some(s) => s,
                None => return,
            };
            match label {
                "Rectangle" => {
                    // value.split(",") → y1, x1, y2, x2 (GatanReader.java:666-670).
                    let pts = Self::struct_values(val);
                    if pts.len() >= 4 {
                        shape.y1 = pts[0];
                        shape.x1 = pts[1];
                        shape.y2 = pts[2];
                        shape.x2 = pts[3];
                    }
                }
                "Text" => {
                    shape.text = Self::leaf_string(val);
                }
                "ForegroundColor" => {
                    // value.split(",") → red, green, blue (& 0xff each).
                    let colors = Self::struct_values(val);
                    if colors.len() >= 3 {
                        shape.stroke_color = Some((
                            (colors[0] as i32 & 0xff) as u8,
                            (colors[1] as i32 & 0xff) as u8,
                            (colors[2] as i32 & 0xff) as u8,
                        ));
                    }
                }
                _ => {}
            }
        } else if parent == "TextFormat" && label == "FontSize" {
            // FontSize applies to the most recently started shape
            // (GatanReader.java:683-688).
            if let (Some(shape), Some(v)) = (self.shapes.last_mut(), val.as_f64()) {
                shape.font_size = Some(v);
            }
        }
    }

    /// Mirror of the named-label dispatch in GatanReader.parseTags
    /// (GatanReader.java:719-749 and the montage Stage Position block at
    /// 689-704). One Rust branch per Java `else if` branch; field names match
    /// the Java member variables.
    fn capture_named_field(&mut self, parent: &str, label: &str, val: &DmValue) {
        // Montage stage positions: "Stage X/Y/Z" leaves under a "Stage Position"
        // group, only while a montage has been detected.
        if self.found_montage && parent == "Stage Position" {
            if let Some(v) = val.as_f64() {
                match label {
                    "Stage X" => self.stage_x.push(v),
                    "Stage Y" => self.stage_y.push(v),
                    "Stage Z" => self.stage_z.push(v),
                    _ => {}
                }
            }
            return;
        }

        match label {
            "LowLimit" => {
                if let Some(v) = val.as_f64() {
                    self.signed = v < 0.0;
                }
            }
            "Acquisition Start Time (epoch)" => {
                if let Some(v) = val.as_f64() {
                    self.timestamp = Some(v as i64);
                }
            }
            "Voltage" => {
                if let Some(v) = val.as_f64() {
                    self.voltage = Some(v);
                }
            }
            "Microscope Info" => {
                self.info = Self::leaf_string(val);
            }
            "Indicated Magnification" => {
                if let Some(v) = val.as_f64() {
                    self.mag = Some(v);
                }
            }
            "Gamma" => {
                if let Some(v) = val.as_f64() {
                    self.gamma = Some(v);
                }
            }
            "Sample Time" => {
                if let Some(v) = val.as_f64() {
                    self.sample_time = Some(v);
                }
            }
            _ => {
                // Java uses startsWith for the position tags.
                if label.starts_with("xPos") {
                    if let Some(v) = val.as_f64() {
                        self.pos_x = Some(v);
                    }
                } else if label.starts_with("yPos") {
                    if let Some(v) = val.as_f64() {
                        self.pos_y = Some(v);
                    }
                } else if label.starts_with("Specimen position") {
                    if let Some(v) = val.as_f64() {
                        self.pos_z = Some(v);
                    }
                }
            }
        }
    }
}

// ── Parsed image info ─────────────────────────────────────────────────────────
struct DmImage {
    width: u32,
    height: u32,
    depth: u32, // Z planes
    dm_data_type: i32,
    pixel_data_offset: u64,
    pixel_data_len: u64,
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
    let (pixel_data_offset, pixel_data_len) = match data_tag {
        DmValue::Bytes { offset, len } => (*offset, *len),
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
        pixel_data_offset,
        pixel_data_len,
        name,
    }))
}

/// Select the physical pixel sizes (X, Y, Z) from the collected `Scale` values,
/// porting GatanReader.initFile's index heuristic (GatanReader.java:282-330).
///
/// The reported OME value is the raw scale (the Java unit conversion only
/// changes the unit, not `Length.value()`), so this returns the chosen scalars.
fn select_physical_sizes(
    pixel_sizes: &[f64],
    size_y: u32,
) -> (Option<f64>, Option<f64>, Option<f64>) {
    const EPSILON: f64 = 1e-10; // loci.common.Constants.EPSILON
    let n = pixel_sizes.len();
    let mut index: usize = 0;
    if n > 4 {
        index = n - 3;
    } else if n == 4 && (pixel_sizes[0] - 1.0).abs() < EPSILON {
        index = n - 2;
    }
    if index + 2 < n && (pixel_sizes[index + 1] - pixel_sizes[index + 2]).abs() < EPSILON {
        if (pixel_sizes[index] - pixel_sizes[index + 1]).abs() > EPSILON && size_y > 1 {
            index += 1;
        }
    }

    let mut psx = None;
    let mut psy = None;
    let mut psz = None;
    if index + 1 < n {
        psx = Some(pixel_sizes[index]);
        psy = Some(pixel_sizes[index + 1]);
        if index + 2 < n {
            psz = Some(pixel_sizes[index + 2]);
        }
    }
    (psx, psy, psz)
}

/// Derive the channel acquisition mode from the "Microscope Info" string,
/// porting GatanReader.initFile (GatanReader.java:344-361). The info string is
/// split on '(' and the token starting with "Mode" yields the mode word; "TEM"
/// is remapped to "Other". Returns `None` when no Mode token is present.
fn acquisition_mode_from_info(info: Option<&str>) -> Option<String> {
    let info = info.unwrap_or("");
    for token in info.split('(') {
        let token = token.trim();
        if token.starts_with("Mode") {
            // strip leading "Mode" word
            let mut mode = match token.find(' ') {
                Some(sp) => token[sp..].trim().to_string(),
                None => token.to_string(),
            };
            if let Some(sp) = mode.find(' ') {
                mode = mode[..sp].trim().to_string();
            }
            if mode == "TEM" {
                mode = "Other".to_string();
            }
            return Some(mode);
        }
    }
    None
}

/// Convert the parsed annotation `RoiShape`s into OME ROIs, porting the ROI
/// emission switch in GatanReader.initFile (GatanReader.java:397-460).
///
/// Each shape becomes one `OmeROI` containing a single shape. The mapping is:
/// LINE→Line, RECTANGLE→Rectangle (width/height = x2-x1, y2-y1), ELLIPSE→
/// Ellipse (centre = x1+rx, y1+ry; radii = (x2-x1)/2, (y2-y1)/2), TEXT→Point
/// (a label at x1,y1 — the repo `OmeShape` model has no Label/text variant).
/// Unknown type codes are skipped, exactly as Java logs and skips them.
fn roi_shapes_to_ome_rois(shapes: &[RoiShape]) -> Vec<crate::common::ome_metadata::OmeROI> {
    use crate::common::ome_metadata::{create_lsid, OmeROI, OmeShape};
    let mut rois = Vec::new();
    // Java increments `nextROI` only for emitted shapes, and uses it both as the
    // ROI index and as the ROI/Shape LSID index.
    let mut next_roi: usize = 0;
    for shape in shapes {
        let ome_shape = match shape.type_code {
            SHAPE_LINE => OmeShape::Line {
                x1: shape.x1,
                y1: shape.y1,
                x2: shape.x2,
                y2: shape.y2,
                the_z: None,
                the_t: None,
                the_c: None,
            },
            SHAPE_TEXT => OmeShape::Point {
                x: shape.x1,
                y: shape.y1,
                the_z: None,
                the_t: None,
                the_c: None,
            },
            SHAPE_ELLIPSE => {
                let radius_x = (shape.x2 - shape.x1) / 2.0;
                let radius_y = (shape.y2 - shape.y1) / 2.0;
                OmeShape::Ellipse {
                    x: shape.x1 + radius_x,
                    y: shape.y1 + radius_y,
                    radius_x,
                    radius_y,
                    the_z: None,
                    the_t: None,
                    the_c: None,
                }
            }
            SHAPE_RECTANGLE => OmeShape::Rectangle {
                x: shape.x1,
                y: shape.y1,
                width: shape.x2 - shape.x1,
                height: shape.y2 - shape.y1,
                the_z: None,
                the_t: None,
                the_c: None,
            },
            // Java logs "Unknown ROI type" and emits nothing.
            _ => continue,
        };
        rois.push(OmeROI {
            id: Some(create_lsid("ROI", &[next_roi])),
            name: shape.text.clone(),
            shapes: vec![ome_shape],
        });
        next_roi += 1;
    }
    rois
}

// ── Reader ────────────────────────────────────────────────────────────────────

pub struct GatanReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    pixel_data_offset: u64,
    pixel_data_len: u64,
    dm_data_type: i32,
    /// OME PhysicalSize{X,Y,Z} derived from the "Scale" tags (micrometres value).
    physical_size_x: Option<f64>,
    physical_size_y: Option<f64>,
    physical_size_z: Option<f64>,
    // ── Data fields mirroring GatanReader's Java member variables ──
    /// DM file `version` (3 or 4), from the header.
    version: i32,
    /// `mag` — objective nominal magnification ("Indicated Magnification").
    mag: Option<f64>,
    /// `voltage` — detector settings voltage in volts ("Voltage").
    voltage: Option<f64>,
    /// `gamma` — display gamma ("Gamma").
    gamma: Option<f64>,
    /// `timestamp` — acquisition start time, epoch seconds.
    timestamp: Option<i64>,
    /// `signed` — whether pixels are signed (from "LowLimit").
    signed: bool,
    /// `sampleTime` — plane exposure time in seconds ("Sample Time").
    sample_time: Option<f64>,
    /// `posX`/`posY`/`posZ` — single-image plane stage positions.
    pos_x: Option<f64>,
    pos_y: Option<f64>,
    pos_z: Option<f64>,
    /// `foundMontage` — whether a "Montage" group was present.
    found_montage: bool,
    /// `stageX`/`stageY`/`stageZ` — per-tile montage stage positions.
    stage_x: Vec<f64>,
    stage_y: Vec<f64>,
    stage_z: Vec<f64>,
    /// Acquisition mode derived from `info` ("Microscope Info"), per Java.
    acquisition_mode: Option<String>,
    /// `shapes` — annotation ROIs parsed from the DM tag tree
    /// (GatanReader.java:113), emitted as OME ROIs in `ome_metadata`.
    shapes: Vec<RoiShape>,
}

impl GatanReader {
    pub fn new() -> Self {
        GatanReader {
            path: None,
            meta: None,
            pixel_data_offset: 0,
            pixel_data_len: 0,
            dm_data_type: 23,
            physical_size_x: None,
            physical_size_y: None,
            physical_size_z: None,
            version: 0,
            mag: None,
            voltage: None,
            gamma: None,
            timestamp: None,
            signed: false,
            sample_time: None,
            pos_x: None,
            pos_y: None,
            pos_z: None,
            found_montage: false,
            stage_x: Vec::new(),
            stage_y: Vec::new(),
            stage_z: Vec::new(),
            acquisition_mode: None,
            shapes: Vec::new(),
        }
    }
}

impl GatanReader {
    /// DM file format version (3 or 4), from the header.
    pub fn version(&self) -> i32 {
        self.version
    }
    /// Objective nominal magnification ("Indicated Magnification" tag).
    pub fn magnification(&self) -> Option<f64> {
        self.mag
    }
    /// Detector voltage in volts ("Voltage" tag).
    pub fn voltage(&self) -> Option<f64> {
        self.voltage
    }
    /// Display gamma ("Gamma" tag).
    pub fn gamma(&self) -> Option<f64> {
        self.gamma
    }
    /// Acquisition start time, epoch seconds ("Acquisition Start Time (epoch)").
    pub fn timestamp(&self) -> Option<i64> {
        self.timestamp
    }
    /// Whether the pixel data is signed (from the "LowLimit" tag).
    pub fn is_signed(&self) -> bool {
        self.signed
    }
    /// Plane exposure / sample time in seconds ("Sample Time" tag).
    pub fn sample_time(&self) -> Option<f64> {
        self.sample_time
    }
    /// Single-image stage position (reference-frame units).
    pub fn position(&self) -> (Option<f64>, Option<f64>, Option<f64>) {
        (self.pos_x, self.pos_y, self.pos_z)
    }
    /// Whether a "Montage" tag group was present.
    pub fn found_montage(&self) -> bool {
        self.found_montage
    }
    /// Per-tile montage stage positions (X, Y, Z lists).
    pub fn stage_positions(&self) -> (&[f64], &[f64], &[f64]) {
        (&self.stage_x, &self.stage_y, &self.stage_z)
    }
    /// Channel acquisition mode derived from "Microscope Info".
    pub fn acquisition_mode(&self) -> Option<&str> {
        self.acquisition_mode.as_deref()
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
        matches!(ext.as_deref(), Some("dm3") | Some("dm4"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        if header.len() < 4 {
            return false;
        }
        // Java GatanReader.isThisType reads only the first big-endian int.
        let v = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
        matches!(v, 3 | 4)
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
            pixel_sizes: Vec::new(),
            units: Vec::new(),
            signed: false,
            gamma: None,
            mag: None,
            voltage: None,
            info: None,
            pos_x: None,
            pos_y: None,
            pos_z: None,
            sample_time: None,
            timestamp: None,
            found_montage: false,
            stage_x: Vec::new(),
            stage_y: Vec::new(),
            stage_z: Vec::new(),
            shapes: Vec::new(),
        };

        // Seek past the file header to the root tag group
        let _root_offset = if dm4 { 24u64 } else { 16u64 }; // version(4) + size(4/8) + byteorder(4)
                                                            // Actually:
                                                            // DM3: version(4) + filesize(4) + byteorder(4) = 12 bytes → root at 12
                                                            // DM4: version(4) + filesize(8) + byteorder(4) = 16 bytes → root at 16
        let root_off = if dm4 { 16u64 } else { 12u64 };
        dm.r.seek(SeekFrom::Start(root_off))
            .map_err(BioFormatsError::Io)?;

        let root = dm.parse_tag_group(0, "").map_err(BioFormatsError::Io)?;
        let pixel_sizes = std::mem::take(&mut dm.pixel_sizes);
        let units = std::mem::take(&mut dm.units);
        // Captured data fields (mirroring GatanReader member variables).
        let captured_signed = dm.signed;
        let gamma = dm.gamma;
        let mag = dm.mag;
        let voltage = dm.voltage;
        let info = dm.info.take();
        let pos_x = dm.pos_x;
        let pos_y = dm.pos_y;
        let pos_z = dm.pos_z;
        let sample_time = dm.sample_time;
        let timestamp = dm.timestamp;
        let found_montage = dm.found_montage;
        let stage_x = std::mem::take(&mut dm.stage_x);
        let stage_y = std::mem::take(&mut dm.stage_y);
        let stage_z = std::mem::take(&mut dm.stage_z);
        let shapes = std::mem::take(&mut dm.shapes);

        let img = find_image_data(&root)?;

        let (mut pixel_type, mut bytes_per_pixel) = dm_pixel_type_and_bytes(img.dm_data_type)?;
        let image_count = img.depth;

        // Reconcile the DataType-derived bytes-per-pixel with the bpp implied by
        // the stored payload. Java: bytes = numPixelBytes / (sizeX*sizeY*imageCount)
        // and, if that disagrees with FormatTools.getBytesPerPixel(pixelType),
        // overrides via FormatTools.pixelTypeFromBytes(bytes, signed, false)
        // (GatanReader.java:256-259). This rescues files whose payload implies a
        // different bpp than the DataType (e.g. RGB DataType 23, where Java's
        // getNumBytes(23)==0).
        let pixel_count = (img.width as usize)
            .checked_mul(img.height as usize)
            .and_then(|px| px.checked_mul(image_count as usize))
            .ok_or_else(|| BioFormatsError::Format("Gatan DM pixel count overflows".into()))?;
        if pixel_count == 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "Gatan DM image has zero pixels".into(),
            ));
        }
        let derived_bytes = img.pixel_data_len as usize / pixel_count;
        if derived_bytes != bytes_per_pixel {
            // Java sources `signed` from the LowLimit tag (GatanReader.java:258).
            // We now capture that tag; fall back to the DataType's own
            // signedness when LowLimit was absent (e.g. RGB DataType 23).
            let signed = captured_signed || dm_data_type_is_signed(img.dm_data_type);
            pixel_type = dm_pixel_type_from_bytes(derived_bytes, signed)?;
            bytes_per_pixel = derived_bytes;
        }

        // Ensure the payload is large enough for the (possibly reconciled) stride.
        let expected_len = pixel_count
            .checked_mul(bytes_per_pixel)
            .ok_or_else(|| BioFormatsError::Format("Gatan DM pixel payload overflows".into()))?;
        if img.pixel_data_len < expected_len as u64 {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "Gatan DM pixel payload is shorter than declared ({} < {expected_len})",
                img.pixel_data_len
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
        // Surface the captured scalar data fields into series metadata, mirroring
        // GatanReader's addGlobalMeta of these named tags. Keys use the Java tag
        // label so downstream consumers see the same names.
        if let Some(v) = gamma {
            meta_map.insert("Gamma".into(), MetadataValue::Float(v));
        }
        if let Some(v) = mag {
            meta_map.insert("Indicated Magnification".into(), MetadataValue::Float(v));
        }
        if let Some(v) = voltage {
            meta_map.insert("Voltage".into(), MetadataValue::Float(v));
        }
        if let Some(v) = sample_time {
            meta_map.insert("Sample Time".into(), MetadataValue::Float(v));
        }
        if let Some(v) = timestamp {
            meta_map.insert(
                "Acquisition Start Time (epoch)".into(),
                MetadataValue::Int(v),
            );
        }
        if let Some(s) = &info {
            meta_map.insert("Microscope Info".into(), MetadataValue::String(s.clone()));
        }
        if let Some(v) = pos_x {
            meta_map.insert("xPos".into(), MetadataValue::Float(v));
        }
        if let Some(v) = pos_y {
            meta_map.insert("yPos".into(), MetadataValue::Float(v));
        }
        if let Some(v) = pos_z {
            meta_map.insert("Specimen position".into(), MetadataValue::Float(v));
        }

        let meta = ImageMetadata {
            size_x: img.width,
            size_y: img.height,
            size_z: image_count,
            size_c: 1,
            size_t: 1,
            pixel_type,
            bits_per_pixel: (bytes_per_pixel * 8) as u8,
            image_count,
            // GatanReader hard-codes dimensionOrder = "XYZTC" (GatanReader.java:253).
            dimension_order: DimensionOrder::XYZTC,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            // Java forces m.littleEndian = true before populating pixels
            // (GatanReader.initFile line 242); pixel data is always little-endian.
            is_little_endian: true,
            resolution_count: 1,
            thumbnail: false,
            series_metadata: meta_map,
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        };

        let (psx, psy, psz) = select_physical_sizes(&pixel_sizes, img.height);
        let _ = units; // units only affect the OME unit, not the reported value

        self.meta = Some(meta);
        self.pixel_data_offset = img.pixel_data_offset;
        self.pixel_data_len = img.pixel_data_len;
        self.dm_data_type = img.dm_data_type;
        self.path = Some(path.to_path_buf());
        self.physical_size_x = psx;
        self.physical_size_y = psy;
        self.physical_size_z = psz;
        self.version = version as i32;
        self.mag = mag;
        self.voltage = voltage;
        self.gamma = gamma;
        self.timestamp = timestamp;
        self.signed = captured_signed;
        self.sample_time = sample_time;
        self.pos_x = pos_x;
        self.pos_y = pos_y;
        self.pos_z = pos_z;
        self.found_montage = found_montage;
        self.stage_x = stage_x;
        self.stage_y = stage_y;
        self.stage_z = stage_z;
        self.acquisition_mode = acquisition_mode_from_info(info.as_deref());
        self.shapes = shapes;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.pixel_data_offset = 0;
        self.pixel_data_len = 0;
        self.physical_size_x = None;
        self.physical_size_y = None;
        self.physical_size_z = None;
        self.version = 0;
        self.mag = None;
        self.voltage = None;
        self.gamma = None;
        self.timestamp = None;
        self.signed = false;
        self.sample_time = None;
        self.pos_x = None;
        self.pos_y = None;
        self.pos_z = None;
        self.found_montage = false;
        self.stage_x.clear();
        self.stage_y.clear();
        self.stage_z.clear();
        self.acquisition_mode = None;
        self.shapes.clear();
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
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let bps = meta.pixel_type.bytes_per_sample();
        let plane_bytes = (meta.size_x * meta.size_y) as usize * bps;
        let start = plane_index as usize * plane_bytes;
        let end = start + plane_bytes;
        if end as u64 > self.pixel_data_len {
            return Err(BioFormatsError::InvalidData(
                "DM plane out of range in data".into(),
            ));
        }
        let mut file = File::open(path).map_err(BioFormatsError::Io)?;
        read_bytes_at(
            &mut file,
            self.pixel_data_offset + start as u64,
            plane_bytes,
        )
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
        let x2 = x
            .checked_add(w)
            .ok_or_else(|| BioFormatsError::Format("Gatan region width overflows".into()))?;
        let y2 = y
            .checked_add(h)
            .ok_or_else(|| BioFormatsError::Format("Gatan region height overflows".into()))?;
        if x2 > meta.size_x || y2 > meta.size_y {
            return Err(BioFormatsError::Format(
                "Gatan region is outside image bounds".into(),
            ));
        }
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let bps = meta.pixel_type.bytes_per_sample();
        let src_row_bytes = meta.size_x as usize * bps;
        let dst_row_bytes = w as usize * bps;
        let plane_bytes = src_row_bytes
            .checked_mul(meta.size_y as usize)
            .ok_or_else(|| BioFormatsError::Format("Gatan plane byte count overflows".into()))?;
        let plane_start = (plane_index as usize)
            .checked_mul(plane_bytes)
            .ok_or_else(|| BioFormatsError::Format("Gatan plane offset overflows".into()))?;
        if plane_start as u64 + plane_bytes as u64 > self.pixel_data_len {
            return Err(BioFormatsError::InvalidData(
                "DM plane out of range in data".into(),
            ));
        }
        let mut file = File::open(path).map_err(BioFormatsError::Io)?;
        let mut out = vec![0u8; dst_row_bytes * h as usize];
        for row in 0..h as usize {
            let src = plane_start + (y as usize + row) * src_row_bytes + x as usize * bps;
            let dst = row * dst_row_bytes;
            if src as u64 + dst_row_bytes as u64 > self.pixel_data_len {
                return Err(BioFormatsError::InvalidData(
                    "DM region out of range in data".into(),
                ));
            }
            let row_data = read_bytes_at(
                &mut file,
                self.pixel_data_offset + src as u64,
                dst_row_bytes,
            )?;
            out[dst..dst + dst_row_bytes].copy_from_slice(&row_data);
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
        use crate::common::ome_metadata::OmeMetadata;
        use crate::common::ome_metadata::{
            create_lsid, OmeChannel, OmeDetector, OmeInstrument, OmeObjective, OmePlane,
        };
        let meta = self.meta.as_ref()?;
        let mut ome = OmeMetadata::from_image_metadata(meta);

        // Instrument with one objective (nominal magnification = `mag`) and one
        // detector, mirroring GatanReader.initFile (GatanReader.java:332-343).
        let instrument = OmeInstrument {
            id: Some(create_lsid("Instrument", &[0])),
            objectives: vec![OmeObjective {
                id: Some(create_lsid("Objective", &[0, 0])),
                correction: Some("Unknown".to_string()),
                immersion: Some("Unknown".to_string()),
                nominal_magnification: self.mag,
                ..Default::default()
            }],
            detectors: vec![OmeDetector {
                id: Some(create_lsid("Detector", &[0, 0])),
                ..Default::default()
            }],
            ..Default::default()
        };
        ome.instruments = vec![instrument];

        let img = ome.images.get_mut(0)?;

        // Image name: GatanReader only sets an explicit name ("Tile #N") for
        // multi-series montages; for a single image it falls back to the file's
        // base name (with extension), e.g. "clem_fig3b.dm3".
        if let Some(path) = &self.path {
            img.name = path
                .file_name()
                .and_then(|n| n.to_str())
                .map(str::to_string);
        }

        img.physical_size_x = self.physical_size_x;
        img.physical_size_y = self.physical_size_y;
        img.physical_size_z = self.physical_size_z;

        // Link instrument + objective, and attach detector settings voltage and
        // the derived acquisition mode to the single channel (Java sets these per
        // series; we have one series, GatanReader.java:369-378).
        img.instrument_ref = Some(0);
        img.objective_ref = Some(0);
        if img.channels.is_empty() {
            img.channels.push(OmeChannel {
                samples_per_pixel: 1,
                ..Default::default()
            });
        }
        if let Some(ch) = img.channels.get_mut(0) {
            ch.detector_ref = Some(create_lsid("Detector", &[0, 0]));
            ch.detector_settings_voltage = self.voltage;
            if let Some(mode) = &self.acquisition_mode {
                ch.acquisition_mode = Some(mode.clone());
            }
        }

        // Plane positions and exposure time. Java sets montage stage positions
        // per series when found (GatanReader.java:380-389); with one series we
        // use the first montage tile if present, else the single posX/Y/Z. Every
        // plane gets the "Sample Time" exposure (GatanReader.java:391-393).
        let (px, py, pz) = if self.found_montage && !self.stage_x.is_empty() {
            (
                self.stage_x.first().copied(),
                self.stage_y.first().copied(),
                self.stage_z.first().copied(),
            )
        } else {
            (self.pos_x, self.pos_y, self.pos_z)
        };
        if px.is_some() || py.is_some() || pz.is_some() || self.sample_time.is_some() {
            let c_size = meta.size_c.max(1);
            let z_size = meta.size_z.max(1);
            img.planes = (0..meta.image_count)
                .map(|p| OmePlane {
                    the_z: (p / c_size) % z_size,
                    the_c: p % c_size,
                    the_t: p / (c_size * z_size),
                    delta_t: None,
                    exposure_time: self.sample_time,
                    position_x: px,
                    position_y: py,
                    position_z: pz,
                })
                .collect();
        }

        // Emit annotation ROIs (GatanReader.java:397-460). Java guards this on
        // metadata level != NO_OVERLAYS; we emit them whenever any were parsed.
        if !self.shapes.is_empty() {
            ome.rois = roi_shapes_to_ome_rois(&self.shapes);
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

pub struct GatanDm2Reader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    data_offset: u64,
}

impl GatanDm2Reader {
    pub fn new() -> Self {
        GatanDm2Reader {
            path: None,
            meta: None,
            data_offset: DM2_HEADER_SIZE,
        }
    }
}

impl Default for GatanDm2Reader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for GatanDm2Reader {
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
            thumbnail: false,
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

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        use crate::common::ome_metadata::OmeMetadata;

        let meta = self.meta.as_ref()?;
        let mut ome = OmeMetadata::from_image_metadata(meta);
        let image = &mut ome.images[0];

        let get_physical = |key: &str| -> Option<f64> {
            match meta.series_metadata.get(key) {
                Some(MetadataValue::Float(v)) => Some(*v),
                Some(MetadataValue::Int(v)) => Some(*v as f64),
                Some(MetadataValue::String(s)) => s.parse::<f64>().ok(),
                _ => None,
            }
        };

        // GatanDM2Reader.parseExtraTags stores tags 31/32 as pixelSizeX/Y and
        // writes them to OME in micrometers. It only warns on non-um units, so
        // preserve the Java value here regardless of the unit tag.
        image.physical_size_x = get_physical("Physical width");
        image.physical_size_y = get_physical("Physical height");

        if let Some(MetadataValue::String(name)) = meta.series_metadata.get("Name") {
            image.name = Some(name.clone());
        }

        Some(ome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::reader::FormatReader;
    use std::path::PathBuf;

    fn testdata(rel: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("testdata")
            .join(rel)
    }

    // Pure-logic port of GatanReader's "Microscope Info" → acquisition mode
    // derivation (GatanReader.java:344-361).
    #[test]
    fn acquisition_mode_parsing() {
        assert_eq!(acquisition_mode_from_info(None), None);
        assert_eq!(acquisition_mode_from_info(Some("")), None);
        // "(Mode <value> ...)" → first word after "Mode" (info is split on '(',
        // matching Java info.split("\\("), so the '(' is consumed).
        assert_eq!(
            acquisition_mode_from_info(Some("Microscope (Mode STEM stuff")).as_deref(),
            Some("STEM")
        );
        // A "Mode TEM <more>" token reduces to exactly "TEM", which Java remaps
        // to "Other" (GatanReader.java:359).
        assert_eq!(
            acquisition_mode_from_info(Some("(Mode TEM more")).as_deref(),
            Some("Other")
        );
    }

    #[test]
    fn dm2_ome_metadata_uses_java_physical_size_extra_tags() {
        let mut series_metadata = HashMap::new();
        series_metadata.insert(
            "Physical width".into(),
            MetadataValue::String("1.25".into()),
        );
        series_metadata.insert(
            "Physical height".into(),
            MetadataValue::String("2.5".into()),
        );
        series_metadata.insert(
            "Physical size units".into(),
            MetadataValue::String("nm".into()),
        );
        series_metadata.insert("Name".into(), MetadataValue::String("dm2 image".into()));

        let reader = GatanDm2Reader {
            path: None,
            data_offset: DM2_HEADER_SIZE,
            meta: Some(ImageMetadata {
                size_x: 1,
                size_y: 1,
                size_z: 1,
                size_c: 1,
                size_t: 1,
                pixel_type: PixelType::Uint8,
                bits_per_pixel: 8,
                image_count: 1,
                dimension_order: DimensionOrder::XYZCT,
                is_rgb: false,
                is_interleaved: false,
                is_indexed: false,
                is_little_endian: false,
                resolution_count: 1,
                thumbnail: false,
                series_metadata,
                lookup_table: None,
                modulo_z: None,
                modulo_c: None,
                modulo_t: None,
            }),
        };

        let ome = reader.ome_metadata().expect("ome");
        assert_eq!(ome.images[0].physical_size_x, Some(1.25));
        assert_eq!(ome.images[0].physical_size_y, Some(2.5));
        assert_eq!(ome.images[0].name.as_deref(), Some("dm2 image"));
    }

    // Pure-logic port of GatanReader's ROI emission switch
    // (GatanReader.java:397-460): verifies the type-code → OmeShape mapping and
    // the coordinate math for each annotation type, plus that unknown codes are
    // skipped and `nextROI` only advances for emitted shapes.
    #[test]
    fn roi_shape_emission() {
        use crate::common::ome_metadata::OmeShape;
        let shapes = vec![
            RoiShape {
                type_code: SHAPE_LINE,
                x1: 1.0,
                y1: 2.0,
                x2: 3.0,
                y2: 4.0,
                text: Some("L".into()),
                ..Default::default()
            },
            RoiShape {
                type_code: SHAPE_RECTANGLE,
                // Java reads y1,x1,y2,x2 from the "Rectangle" struct.
                x1: 10.0,
                y1: 20.0,
                x2: 30.0,
                y2: 50.0,
                ..Default::default()
            },
            RoiShape {
                type_code: SHAPE_ELLIPSE,
                x1: 0.0,
                y1: 0.0,
                x2: 8.0,
                y2: 4.0,
                ..Default::default()
            },
            RoiShape {
                type_code: SHAPE_TEXT,
                x1: 5.0,
                y1: 6.0,
                text: Some("hello".into()),
                ..Default::default()
            },
            // Unknown code → skipped, must not advance the ROI index.
            RoiShape {
                type_code: 99,
                ..Default::default()
            },
        ];

        let rois = roi_shapes_to_ome_rois(&shapes);
        assert_eq!(rois.len(), 4, "unknown ROI type is skipped");

        assert_eq!(rois[0].id.as_deref(), Some("ROI:0"));
        assert!(matches!(
            rois[0].shapes[0],
            OmeShape::Line { x1, y1, x2, y2, .. }
                if x1 == 1.0 && y1 == 2.0 && x2 == 3.0 && y2 == 4.0
        ));

        assert_eq!(rois[1].id.as_deref(), Some("ROI:1"));
        assert!(matches!(
            rois[1].shapes[0],
            OmeShape::Rectangle { x, y, width, height, .. }
                if x == 10.0 && y == 20.0 && width == 20.0 && height == 30.0
        ));

        // Ellipse: centre = x1+rx, y1+ry; radii = (x2-x1)/2, (y2-y1)/2.
        assert!(matches!(
            rois[2].shapes[0],
            OmeShape::Ellipse { x, y, radius_x, radius_y, .. }
                if x == 4.0 && y == 2.0 && radius_x == 4.0 && radius_y == 2.0
        ));

        assert_eq!(rois[3].id.as_deref(), Some("ROI:3"));
        assert_eq!(rois[3].name.as_deref(), Some("hello"));
        assert!(matches!(
            rois[3].shapes[0],
            OmeShape::Point { x, y, .. } if x == 5.0 && y == 6.0
        ));
    }

    // Real DM3 file: confirms the newly-captured scalar data fields (version,
    // mag, voltage, gamma, signed) are parsed, and that mag/voltage are surfaced
    // into OME (objective magnification, detector settings voltage).
    #[test]
    fn real_dm3_captured_fields() {
        let path = testdata("dm3/clem_fig3b.dm3");
        if !path.exists() {
            eprintln!("SKIP real_dm3_captured_fields: {} absent", path.display());
            return;
        }
        let mut r = GatanReader::new();
        r.set_id(&path).expect("set_id dm3");

        assert_eq!(r.version(), 3);
        assert_eq!(r.magnification(), Some(3000.0));
        assert_eq!(r.voltage(), Some(100000.0));
        assert_eq!(r.gamma(), Some(0.5));
        assert!(!r.is_signed());
        assert!(!r.found_montage());

        // Series metadata surfaces the captured tags under their Java labels.
        let meta = r.metadata();
        assert!(matches!(
            meta.series_metadata.get("Indicated Magnification"),
            Some(MetadataValue::Float(v)) if (*v - 3000.0).abs() < 1e-9
        ));
        assert!(matches!(
            meta.series_metadata.get("Voltage"),
            Some(MetadataValue::Float(v)) if (*v - 100000.0).abs() < 1e-9
        ));
        assert!(matches!(
            meta.series_metadata.get("Gamma"),
            Some(MetadataValue::Float(v)) if (*v - 0.5).abs() < 1e-9
        ));

        // OME: objective nominal magnification = mag; detector voltage = voltage.
        let ome = r.ome_metadata().expect("ome");
        let obj = &ome.instruments[0].objectives[0];
        assert_eq!(obj.nominal_magnification, Some(3000.0));
        let ch = &ome.images[0].channels[0];
        assert_eq!(ch.detector_settings_voltage, Some(100000.0));
    }

    // Real DM4 montage file: confirms montage detection, per-tile stage
    // positions, signedness from LowLimit, and OME plane positions.
    #[test]
    fn real_dm4_montage_fields() {
        let path = testdata("gatan/SmallMontage0000.dm4");
        if !path.exists() {
            eprintln!("SKIP real_dm4_montage_fields: {} absent", path.display());
            return;
        }
        let mut r = GatanReader::new();
        r.set_id(&path).expect("set_id dm4");

        assert_eq!(r.version(), 4);
        assert_eq!(r.magnification(), Some(1900.0));
        assert_eq!(r.voltage(), Some(120000.0));
        assert!(r.is_signed(), "DM4 file has negative LowLimit → signed");
        assert!(r.found_montage());

        let (sx, sy, sz) = r.stage_positions();
        assert_eq!(sx.len(), 1);
        assert_eq!(sy.len(), 1);
        assert_eq!(sz.len(), 1);

        // OME: the montage tile's stage position is used as the plane position.
        let ome = r.ome_metadata().expect("ome");
        let plane = ome.images[0]
            .planes
            .first()
            .expect("at least one plane with positions");
        assert_eq!(plane.position_x, Some(sx[0]));
        assert_eq!(plane.position_y, Some(sy[0]));
        assert_eq!(plane.position_z, Some(sz[0]));
    }
}
