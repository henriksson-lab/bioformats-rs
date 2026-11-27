//! SimFCS FLIM binary format reader.
//!
//! SimFCS stores raw binary FLIM data with no file header.
//! The file extension indicates the data type.
//!
//! NON-UPSTREAM EXTENSION: Bio-Formats has no SimFCS reader; this is kept as a
//! documented extra (reads raw 256x256 .r64/.ref/.b64 frames).

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;

// ── SimFCS Reader ─────────────────────────────────────────────────────────────

pub struct SimfcsReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    data_offset: u64,
    decompressed: Option<Vec<u8>>,
}

impl SimfcsReader {
    pub fn new() -> Self {
        SimfcsReader {
            path: None,
            meta: None,
            data_offset: 0,
            decompressed: None,
        }
    }
}

impl Default for SimfcsReader {
    fn default() -> Self {
        Self::new()
    }
}

fn simfcs_base_pixel_type(ext: &str) -> Option<PixelType> {
    match ext {
        "b64" => Some(PixelType::Uint8),
        "r64" => Some(PixelType::Float32),
        "i64" => Some(PixelType::Int32),
        _ => None,
    }
}

fn is_zlib_header(bytes: &[u8]) -> bool {
    bytes.len() >= 2
        && (bytes[0] & 0x0f) == 8
        && (bytes[0] >> 4) <= 7
        && (((u16::from(bytes[0])) << 8) | u16::from(bytes[1])) % 31 == 0
}

fn resolve_simfcs_layout(
    ext: &str,
    default_pt: PixelType,
    total_len: usize,
    first4: Option<[u8; 4]>,
) -> Option<(u32, u32, PixelType, u64, usize)> {
    if let Some(hdr) = first4 {
        let dim = i32::from_le_bytes(hdr);
        if (1..=16384).contains(&dim) && total_len >= 4 {
            let d = dim as usize;
            let payload = total_len - 4;
            if ext == "b64" {
                if let Some(frame_i16) = d.checked_mul(d).and_then(|v| v.checked_mul(2)) {
                    if payload > 0 && payload % frame_i16 == 0 {
                        return Some((dim as u32, dim as u32, PixelType::Int16, 4, payload));
                    }
                }
                if let Some(frame_u8) = d.checked_mul(d) {
                    if payload > 0 && payload % frame_u8 == 0 {
                        return Some((dim as u32, dim as u32, PixelType::Uint8, 4, payload));
                    }
                }
            } else {
                let bps = default_pt.bytes_per_sample();
                if let Some(frame_bytes) = d.checked_mul(d).and_then(|v| v.checked_mul(bps)) {
                    if payload > 0 && payload % frame_bytes == 0 {
                        return Some((dim as u32, dim as u32, default_pt, 4, payload));
                    }
                }
            }
        }
    }
    let frame_bytes = 256 * 256 * default_pt.bytes_per_sample();
    if total_len > 0 && total_len % frame_bytes == 0 {
        return Some((256, 256, default_pt, 0, total_len));
    }
    None
}

impl FormatReader for SimfcsReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("b64") | Some("r64") | Some("i64"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .unwrap_or_default();
        let default_pt = simfcs_base_pixel_type(&ext)
            .ok_or_else(|| BioFormatsError::Format(format!("Unknown SimFCS extension: {}", ext)))?;

        let mut f = fs::File::open(path).map_err(BioFormatsError::Io)?;
        let file_size = f.metadata().map_err(BioFormatsError::Io)?.len() as usize;
        let mut prefix = [0u8; 4];
        let prefix_len = f.read(&mut prefix).map_err(BioFormatsError::Io)?;

        let (decompressed, total_len, first4) =
            if matches!(ext.as_str(), "r64" | "i64") && prefix_len >= 2 && is_zlib_header(&prefix[..prefix_len]) {
                f.seek(SeekFrom::Start(0)).map_err(BioFormatsError::Io)?;
                let mut dec = flate2::read::ZlibDecoder::new(f);
                let mut decoded = Vec::new();
                if dec.read_to_end(&mut decoded).is_ok() && !decoded.is_empty() {
                    let f4 = decoded.get(..4).map(|b| [b[0], b[1], b[2], b[3]]);
                    let len = decoded.len();
                    (Some(decoded), len, f4)
                } else {
                    let f4 = if prefix_len == 4 { Some(prefix) } else { None };
                    (None, file_size, f4)
                }
            } else {
                let f4 = if prefix_len == 4 { Some(prefix) } else { None };
                (None, file_size, f4)
            };

        let (width, height, pixel_type, data_offset, payload_len) =
            resolve_simfcs_layout(&ext, default_pt, total_len, first4).ok_or_else(|| {
                BioFormatsError::UnsupportedFormat(format!(
                    "SimFCS payload length {total_len} is not a whole number of 256x256 frames"
                ))
            })?;

        let bps = pixel_type.bytes_per_sample();
        let frame_bytes = width as usize * height as usize * bps;
        let image_count = (payload_len / frame_bytes) as u32;
        let mut series_metadata = HashMap::new();
        series_metadata.insert("format".into(), MetadataValue::String("SimFCS".into()));
        series_metadata.insert("simfcs.extension".into(), MetadataValue::String(ext));
        series_metadata.insert("simfcs.width".into(), MetadataValue::Int(i64::from(width)));
        series_metadata.insert("simfcs.height".into(), MetadataValue::Int(i64::from(height)));
        series_metadata.insert(
            "simfcs.frame_bytes".into(),
            MetadataValue::Int(frame_bytes as i64),
        );
        series_metadata.insert(
            "simfcs.payload_bytes".into(),
            MetadataValue::Int(payload_len as i64),
        );
        if decompressed.is_some() {
            series_metadata.insert("simfcs.compressed".into(), MetadataValue::Bool(true));
        }

        let meta = ImageMetadata {
            size_x: width,
            size_y: height,
            size_z: image_count,
            size_c: 1,
            size_t: 1,
            pixel_type,
            bits_per_pixel: (bps * 8) as u16,
            image_count,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: true,
            resolution_count: 1,
            thumbnail: false,
            series_metadata,
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        };

        self.path = Some(path.to_path_buf());
        self.meta = Some(meta);
        self.data_offset = data_offset;
        self.decompressed = decompressed;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.data_offset = 0;
        self.decompressed = None;
        Ok(())
    }

    fn series_count(&self) -> usize {
        usize::from(self.meta.is_some())
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.meta.is_none() {
            Err(BioFormatsError::NotInitialized)
        } else if s == 0 {
            Ok(())
        } else {
            Err(BioFormatsError::SeriesOutOfRange(s))
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
        let offset = self.data_offset + plane_index as u64 * plane_bytes as u64;

        if let Some(decoded) = self.decompressed.as_ref() {
            let start = usize::try_from(offset).map_err(|_| {
                BioFormatsError::InvalidData("SimFCS plane offset overflows".into())
            })?;
            let end = start.checked_add(plane_bytes).ok_or_else(|| {
                BioFormatsError::InvalidData("SimFCS plane range overflows".into())
            })?;
            let slice = decoded.get(start..end).ok_or_else(|| {
                BioFormatsError::InvalidData("SimFCS decoded payload is shorter than declared".into())
            })?;
            return Ok(slice.to_vec());
        }

        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = fs::File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(offset))
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
        crop_full_plane("SimFCS", &full, meta, 1, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        let meta = self.meta.as_ref()?;
        let mut ome = crate::common::ome_metadata::OmeMetadata::from_image_metadata(meta);
        let _ = ome.add_original_metadata_annotations(meta, 0);
        Some(ome)
    }
}
