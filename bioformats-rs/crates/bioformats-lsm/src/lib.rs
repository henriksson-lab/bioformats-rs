//! Zeiss LSM format reader (confocal laser scanning microscopy).
//!
//! LSM files are TIFF-based with a proprietary CZ_LSMInfo block (tag 34412).
//! The CZ_LSMInfo block provides the true Z/C/T dimensions.
//! Every other IFD is a thumbnail; only even-indexed IFDs contain full-res data.

use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use bioformats_common::error::{BioFormatsError, Result};
use bioformats_common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use bioformats_common::pixel_type::PixelType;
use bioformats_common::reader::FormatReader;
use bioformats_tiff::TiffReader;
use bioformats_tiff::ifd::IfdValue;
use bioformats_tiff::parser::TiffParser;

// ── Tag IDs ───────────────────────────────────────────────────────────────────
const CZ_LSM_INFO: u16 = 34412;

// ── CZ_LSMInfo block (partial) ────────────────────────────────────────────────
// Only the fields we actually need:
//   offset 0:  MagicNumber (int32) = 0x00300494
//   offset 4:  StructureSize (int32)
//   offset 8:  DimensionX (int32)
//   offset 12: DimensionY (int32)
//   offset 16: DimensionZ (int32)
//   offset 20: DimensionChannels (int32)
//   offset 24: DimensionTime (int32)
//   offset 28: DataType (int32) → 1=uint8, 2=uint12, 5=uint16
//   offset 32: ThumbnailX (int32)
//   offset 36: ThumbnailY (int32)
//   offset 40: VoxelSizeX (float64)
//   offset 48: VoxelSizeY (float64)
//   offset 56: VoxelSizeZ (float64)
const LSM_MAGIC: u32 = 0x0030_0494;

#[derive(Debug, Default)]
struct LsmInfo {
    dim_z: u32,
    dim_c: u32,
    dim_t: u32,
    data_type: i32,
    voxel_x: f64,
    voxel_y: f64,
    voxel_z: f64,
}

fn read_i32_lsm(buf: &[u8], off: usize, le: bool) -> i32 {
    let b = [buf[off], buf[off+1], buf[off+2], buf[off+3]];
    if le { i32::from_le_bytes(b) } else { i32::from_be_bytes(b) }
}
fn read_f64_lsm(buf: &[u8], off: usize, le: bool) -> f64 {
    let b: [u8; 8] = buf[off..off+8].try_into().unwrap_or([0u8; 8]);
    if le { f64::from_le_bytes(b) } else { f64::from_be_bytes(b) }
}

fn parse_lsm_info(bytes: &[u8], le: bool) -> Option<LsmInfo> {
    if bytes.len() < 64 { return None; }
    let magic = read_i32_lsm(bytes, 0, le) as u32;
    if magic != LSM_MAGIC { return None; }

    Some(LsmInfo {
        dim_z: read_i32_lsm(bytes, 16, le).max(1) as u32,
        dim_c: read_i32_lsm(bytes, 20, le).max(1) as u32,
        dim_t: read_i32_lsm(bytes, 24, le).max(1) as u32,
        data_type: read_i32_lsm(bytes, 28, le),
        voxel_x: if bytes.len() >= 48 { read_f64_lsm(bytes, 40, le) } else { 0.0 },
        voxel_y: if bytes.len() >= 56 { read_f64_lsm(bytes, 48, le) } else { 0.0 },
        voxel_z: if bytes.len() >= 64 { read_f64_lsm(bytes, 56, le) } else { 0.0 },
    })
}

fn lsm_pixel_type(data_type: i32, tiff_bps: u16) -> PixelType {
    // data_type: 1=uint8, 2=uint12, 5=uint16
    match data_type {
        1 => PixelType::Uint8,
        2 | 5 => PixelType::Uint16,
        _ => {
            // Fall back to TIFF BPS
            match tiff_bps {
                8 => PixelType::Uint8,
                16 => PixelType::Uint16,
                32 => PixelType::Float32,
                _ => PixelType::Uint8,
            }
        }
    }
}

// ── Minimal TIFF IFD reader for fetching CZ_LSMInfo bytes ────────────────────
fn read_lsm_info_from_file(path: &Path) -> Result<(LsmInfo, bool)> {
    let f = File::open(path).map_err(BioFormatsError::Io)?;
    let buf = BufReader::new(f);
    let mut parser = TiffParser::new(buf)?;
    let le = parser.little_endian;
    let (ifd, _) = parser.read_ifd(parser.first_ifd_offset)?;

    // Find CZ_LSMInfo tag
    let lsm_bytes = match ifd.get(CZ_LSM_INFO) {
        Some(IfdValue::Byte(b)) => b.clone(),
        Some(IfdValue::Undefined(b)) => b.clone(),
        _ => {
            return Err(BioFormatsError::Format(
                "LSM: CZ_LSMInfo tag (34412) not found in first IFD".into(),
            ))
        }
    };

    let info = parse_lsm_info(&lsm_bytes, le).ok_or_else(|| {
        BioFormatsError::Format("LSM: failed to parse CZ_LSMInfo block".into())
    })?;
    Ok((info, le))
}

// ── Reader ────────────────────────────────────────────────────────────────────

pub struct LsmReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    /// Inner TIFF reader handles pixel I/O; we select the correct series.
    inner: TiffReader,
}

impl LsmReader {
    pub fn new() -> Self {
        LsmReader {
            path: None,
            meta: None,
            inner: TiffReader::new(),
        }
    }
}

impl Default for LsmReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for LsmReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension().and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("lsm"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        // LSM files are TIFF; we rely on extension detection since the TIFF
        // reader also matches magic bytes.
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        // First, read the CZ_LSMInfo block to get true dimensions
        let (lsm_info, le) = read_lsm_info_from_file(path)?;

        // Open with inner TIFF reader to get pixel dimensions and read pixel data
        self.inner.set_id(path)?;

        // The TIFF reader may have multiple series (full-res + thumbnails).
        // Select the series with the largest images.
        let n_series = self.inner.series_count();
        let mut best_series = 0usize;
        let mut best_pixels = 0u64;
        for s in 0..n_series {
            let _ = self.inner.set_series(s);
            let m = self.inner.metadata();
            let px = m.size_x as u64 * m.size_y as u64;
            if px > best_pixels {
                best_pixels = px;
                best_series = s;
            }
        }
        let _ = self.inner.set_series(best_series);
        let tiff_meta = self.inner.metadata().clone();

        // Build corrected metadata using LSM dimensions
        let dim_z = lsm_info.dim_z.max(1);
        let dim_c = lsm_info.dim_c.max(1);
        let dim_t = lsm_info.dim_t.max(1);
        let image_count = dim_z * dim_c * dim_t;

        let pixel_type = lsm_pixel_type(lsm_info.data_type, tiff_meta.bits_per_pixel as u16);
        let is_rgb = dim_c == 3;

        let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
        meta_map.insert("voxel_size_x_um".into(), MetadataValue::Float(lsm_info.voxel_x * 1e6));
        meta_map.insert("voxel_size_y_um".into(), MetadataValue::Float(lsm_info.voxel_y * 1e6));
        meta_map.insert("voxel_size_z_um".into(), MetadataValue::Float(lsm_info.voxel_z * 1e6));

        let meta = ImageMetadata {
            size_x: tiff_meta.size_x,
            size_y: tiff_meta.size_y,
            size_z: dim_z,
            size_c: dim_c,
            size_t: dim_t,
            pixel_type,
            bits_per_pixel: tiff_meta.bits_per_pixel,
            image_count,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb,
            is_interleaved: tiff_meta.is_interleaved,
            is_indexed: false,
            is_little_endian: le,
            resolution_count: 1,
            series_metadata: meta_map,
            lookup_table: None,
        };

        self.meta = Some(meta);
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        let _ = self.inner.close();
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
        let count = self.meta.as_ref().map(|m| m.image_count).unwrap_or(0);
        if plane_index >= count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        // Delegate to inner TIFF reader. In LSM, every pair of IFDs is
        // (full-res, thumbnail); full-res planes are at even TIFF IFD indices.
        // The inner reader's current series already selects the correct IFDs,
        // but plane_index here may be out of range for the inner reader if
        // the TIFF series has fewer planes than LSM dimensions suggest.
        // Map plane_index → inner plane index with bounds check.
        let inner_count = self.inner.metadata().image_count;
        let inner_idx = if inner_count > 0 { plane_index.min(inner_count - 1) } else { 0 };
        self.inner.open_bytes(inner_idx)
    }

    fn open_bytes_region(&mut self, plane_index: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let count = self.meta.as_ref().map(|m| m.image_count).unwrap_or(0);
        if plane_index >= count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let inner_count = self.inner.metadata().image_count;
        let inner_idx = if inner_count > 0 { plane_index.min(inner_count - 1) } else { 0 };
        self.inner.open_bytes_region(inner_idx, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = ((meta.size_x - tw) / 2, (meta.size_y - th) / 2);
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}
