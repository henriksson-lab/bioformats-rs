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
}

impl SimfcsReader {
    pub fn new() -> Self {
        SimfcsReader {
            path: None,
            meta: None,
        }
    }
}

impl Default for SimfcsReader {
    fn default() -> Self {
        Self::new()
    }
}

fn simfcs_pixel_type(ext: &str) -> Option<PixelType> {
    match ext {
        "b64" => Some(PixelType::Uint8),
        "r64" => Some(PixelType::Float32),
        "i64" => Some(PixelType::Int32),
        _ => None,
    }
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
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .unwrap_or_default();
        let pixel_type = simfcs_pixel_type(&ext)
            .ok_or_else(|| BioFormatsError::Format(format!("Unknown SimFCS extension: {}", ext)))?;

        let bps = pixel_type.bytes_per_sample();
        let file_size = fs::metadata(path).map_err(BioFormatsError::Io)?.len() as usize;
        let frame_bytes = 256 * 256 * bps;
        if file_size == 0 || file_size % frame_bytes != 0 {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "SimFCS payload length {file_size} is not a whole number of 256x256 frames"
            )));
        }
        let image_count = (file_size / frame_bytes) as u32;
        let mut series_metadata = HashMap::new();
        series_metadata.insert("format".into(), MetadataValue::String("SimFCS".into()));
        series_metadata.insert("simfcs.extension".into(), MetadataValue::String(ext));
        series_metadata.insert("simfcs.width".into(), MetadataValue::Int(256));
        series_metadata.insert("simfcs.height".into(), MetadataValue::Int(256));
        series_metadata.insert(
            "simfcs.frame_bytes".into(),
            MetadataValue::Int(frame_bytes as i64),
        );
        series_metadata.insert(
            "simfcs.payload_bytes".into(),
            MetadataValue::Int(file_size as i64),
        );

        let meta = ImageMetadata {
            size_x: 256,
            size_y: 256,
            size_z: image_count,
            size_c: 1,
            size_t: 1,
            pixel_type,
            bits_per_pixel: (bps * 8) as u8,
            image_count,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: true,
            resolution_count: 1,
            series_metadata,
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        };

        self.path = Some(path.to_path_buf());
        self.meta = Some(meta);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
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
        let plane_bytes = 256 * 256 * bps;
        let offset = plane_index as u64 * plane_bytes as u64;

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
