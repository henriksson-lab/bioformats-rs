//! IMAGIC electron microscopy format reader (.hed + .img).
//!
//! IMAGIC-5 stores images as a pair of files:
//!   .hed — header file (one 1024-byte record per image, each as 256 int32 values)
//!   .img — pixel data file (images stored sequentially)
//!
//! Header record layout (matching the upstream Java ImagicReader):
//!   skip 16, then month/day/year/hour/minute/seconds (6×i32 = 24 bytes), skip 8
//!   off 48: sizeY (i32)
//!   off 52: sizeX (i32)
//!   off 56: 4-char ASCII type string ("REAL"=float32, "INTG"=uint16, "PACK"=uint8)

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;

const HDR_RECORD_BYTES: usize = 1024;

fn r_i32_le(b: &[u8], off: usize) -> i32 {
    i32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

fn imagic_pixel_type(type_str: &str) -> Result<(PixelType, u8)> {
    match type_str {
        "REAL" => Ok((PixelType::Float32, 32)),
        "INTG" => Ok((PixelType::Uint16, 16)),
        "PACK" => Ok((PixelType::Uint8, 8)),
        "COMP" => Err(BioFormatsError::UnsupportedFormat(
            "Unsupported pixel type 'COMP'".into(),
        )),
        "RECO" => Err(BioFormatsError::UnsupportedFormat(
            "Unsupported pixel type 'RECO'".into(),
        )),
        // Default to float32 if the type string is unrecognized.
        _ => Ok((PixelType::Float32, 32)),
    }
}

pub struct ImagicReader {
    hed_path: Option<PathBuf>,
    img_path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    bytes_per_sample: usize,
}

impl ImagicReader {
    pub fn new() -> Self {
        ImagicReader {
            hed_path: None,
            img_path: None,
            meta: None,
            bytes_per_sample: 4,
        }
    }
}

impl Default for ImagicReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for ImagicReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("hed") | Some("img"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // The IMAGIC header has no fixed magic; upstream relies on the .hed
        // suffix plus the presence of a matching .img file. As a byte-level
        // heuristic, validate that the type string at offset 56 is one of the
        // known IMAGIC pixel format tags.
        if header.len() < 60 {
            return false;
        }
        let type_str = std::str::from_utf8(&header[56..60]).unwrap_or("");
        matches!(type_str, "REAL" | "INTG" | "PACK" | "COMP" | "RECO")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        // Determine .hed and .img paths
        let stem = path.file_stem().unwrap_or_default();
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let hed_path = if path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("hed"))
            .unwrap_or(false)
        {
            path.to_path_buf()
        } else {
            parent.join(format!("{}.hed", stem.to_string_lossy()))
        };
        let img_path = parent.join(format!("{}.img", stem.to_string_lossy()));

        // Read first .hed record
        let mut f = File::open(&hed_path).map_err(BioFormatsError::Io)?;
        let file_len = f.metadata().map_err(BioFormatsError::Io)?.len();
        let num_images = (file_len / HDR_RECORD_BYTES as u64).max(1);

        let mut rec = vec![0u8; HDR_RECORD_BYTES];
        f.read_exact(&mut rec).map_err(BioFormatsError::Io)?;

        // Java layout: skip 16, read 6 ints (date/time, 24 bytes), skip 8,
        // then sizeY @48, sizeX @52, 4-char type string @56.
        let size_y = r_i32_le(&rec, 48).max(1) as u32;
        let size_x = r_i32_le(&rec, 52).max(1) as u32;
        let type_str = std::str::from_utf8(&rec[56..60])
            .unwrap_or("")
            .trim_end_matches(char::from(0))
            .to_string();

        let (pixel_type, bpp) = imagic_pixel_type(&type_str)?;

        let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
        meta_map.insert("format".into(), MetadataValue::String("IMAGIC-5 EM".into()));
        meta_map.insert("type".into(), MetadataValue::String(type_str));

        self.meta = Some(ImageMetadata {
            size_x,
            size_y,
            size_z: num_images as u32,
            size_c: 1,
            size_t: 1,
            pixel_type,
            bits_per_pixel: bpp,
            image_count: num_images as u32,
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
        });
        self.bytes_per_sample = pixel_type.bytes_per_sample();
        self.hed_path = Some(hed_path);
        self.img_path = Some(img_path);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.hed_path = None;
        self.img_path = None;
        self.meta = None;
        Ok(())
    }
    fn series_count(&self) -> usize {
        1
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
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
        let plane_bytes = (meta.size_x * meta.size_y) as usize * self.bytes_per_sample;
        let offset = plane_index as u64 * plane_bytes as u64;
        let img_path = self
            .img_path
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?;
        let mut f = File::open(img_path).map_err(BioFormatsError::Io)?;
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
        crop_full_plane("IMAGIC", &full, meta, 1, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = ((meta.size_x - tw) / 2, (meta.size_y - th) / 2);
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}
