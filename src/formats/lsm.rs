//! Zeiss LSM format reader (confocal laser scanning microscopy).
//!
//! LSM files are TIFF-based with a proprietary CZ_LSMInfo block (tag 34412).
//! The CZ_LSMInfo block provides the true Z/C/T dimensions.
//! Every other IFD is a thumbnail; only even-indexed IFDs contain full-res data.

use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::tiff::ifd::IfdValue;
use crate::tiff::parser::TiffParser;
use crate::tiff::TiffReader;

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
//   offset 28: DataType (int32) -> 1=uint8, 2=uint12, 5=float32
//   offset 32: ThumbnailX (int32)
//   offset 36: ThumbnailY (int32)
//   offset 40: VoxelSizeX (float64)
//   offset 48: VoxelSizeY (float64)
//   offset 56: VoxelSizeZ (float64)
// Known CZ_LSMInfo magic numbers. ZeissLSMReader.java does not gate on the
// magic value at all (it only records it as metadata), so we accept both
// documented variants and do not hard-fail on others.
const LSM_MAGIC: u32 = 0x0030_0494;
const LSM_MAGIC_ALT: u32 = 0x0040_0494;

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

fn checked_plane_count(size_z: u32, size_c: u32, size_t: u32) -> Result<u32> {
    size_z
        .checked_mul(size_c)
        .and_then(|v| v.checked_mul(size_t))
        .ok_or_else(|| BioFormatsError::Format("LSM: plane count overflow".into()))
}

fn lsm_uses_packed_channels(
    dim_z: u32,
    dim_c: u32,
    dim_t: u32,
    tiff_size_c: u32,
    full_res_ifd_count: u32,
) -> bool {
    dim_c > 1
        && tiff_size_c == dim_c
        && checked_plane_count(dim_z, 1, dim_t).ok() == Some(full_res_ifd_count)
}

fn resolve_lsm_plane_index(
    plane_index: u32,
    logical_count: u32,
    physical_count: u32,
) -> Result<u32> {
    if plane_index >= logical_count || plane_index >= physical_count {
        return Err(BioFormatsError::PlaneOutOfRange(plane_index));
    }
    Ok(plane_index)
}

fn read_i32_lsm(buf: &[u8], off: usize, le: bool) -> i32 {
    let b = [buf[off], buf[off + 1], buf[off + 2], buf[off + 3]];
    if le {
        i32::from_le_bytes(b)
    } else {
        i32::from_be_bytes(b)
    }
}
fn read_f64_lsm(buf: &[u8], off: usize, le: bool) -> f64 {
    let b: [u8; 8] = buf[off..off + 8].try_into().unwrap_or([0u8; 8]);
    if le {
        f64::from_le_bytes(b)
    } else {
        f64::from_be_bytes(b)
    }
}

fn parse_lsm_info(bytes: &[u8], le: bool) -> Result<LsmInfo> {
    if bytes.len() < 64 {
        return Err(BioFormatsError::Format(
            "LSM: CZ_LSMInfo block is shorter than 64 bytes".into(),
        ));
    }
    // ZeissLSMReader.java never rejects based on the magic number; it only
    // records it. We mirror that: accept the documented magics (0x00300494 and
    // 0x00400494) and, for any other value, only emit a debug-level note rather
    // than failing to parse the block.
    let magic = read_i32_lsm(bytes, 0, le) as u32;
    if magic != LSM_MAGIC && magic != LSM_MAGIC_ALT {
        // Not a hard error: continue parsing dimensions like Java does.
    }

    let dim_z = read_i32_lsm(bytes, 16, le);
    let dim_c = read_i32_lsm(bytes, 20, le);
    let dim_t = read_i32_lsm(bytes, 24, le);
    if dim_z <= 0 || dim_c <= 0 || dim_t <= 0 {
        return Err(BioFormatsError::Format(format!(
            "LSM: invalid non-positive dimensions Z={dim_z} C={dim_c} T={dim_t}"
        )));
    }

    Ok(LsmInfo {
        dim_z: dim_z as u32,
        dim_c: dim_c as u32,
        dim_t: dim_t as u32,
        data_type: read_i32_lsm(bytes, 28, le),
        voxel_x: if bytes.len() >= 48 {
            read_f64_lsm(bytes, 40, le)
        } else {
            0.0
        },
        voxel_y: if bytes.len() >= 56 {
            read_f64_lsm(bytes, 48, le)
        } else {
            0.0
        },
        voxel_z: if bytes.len() >= 64 {
            read_f64_lsm(bytes, 56, le)
        } else {
            0.0
        },
    })
}

fn lsm_pixel_type(data_type: i32, tiff_bps: u16) -> Result<PixelType> {
    // data_type follows ZeissLSMReader: 1=uint8, 2=12-bit stored as uint16,
    // 5=32-bit float.
    match data_type {
        1 => Ok(PixelType::Uint8),
        2 => Ok(PixelType::Uint16),
        5 => Ok(PixelType::Float32),
        other => Err(BioFormatsError::UnsupportedFormat(format!(
            "LSM: unsupported CZ_LSMInfo DataType {other} (TIFF bits/sample {tiff_bps})"
        ))),
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

    let info = parse_lsm_info(&lsm_bytes, le)?;
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

    fn collect_full_resolution_ifds(&self, best_series: usize) -> Vec<usize> {
        let series = self.inner.series_list();
        let Some(target) = series.get(best_series).map(|s| &s.metadata) else {
            return Vec::new();
        };

        series
            .iter()
            .filter(|s| {
                let meta = &s.metadata;
                meta.size_x == target.size_x
                    && meta.size_y == target.size_y
                    && meta.size_c == target.size_c
                    && meta.bits_per_pixel == target.bits_per_pixel
                    && meta.pixel_type == target.pixel_type
                    && meta.is_rgb == target.is_rgb
                    && meta.is_interleaved == target.is_interleaved
            })
            .flat_map(|s| s.ifd_indices.iter().copied())
            .collect()
    }

    fn configure_full_resolution_series(&mut self, best_series: usize) -> u32 {
        let full_res_ifds = self.collect_full_resolution_ifds(best_series);
        let full_res_ifd_count = full_res_ifds.len() as u32;
        if !full_res_ifds.is_empty() {
            if let Some(series) = self.inner.series_list_mut().get_mut(best_series) {
                series.ifd_indices = full_res_ifds;
                series.plane_ifd_indices.clear();
                series.metadata.image_count = full_res_ifd_count;
                series.metadata.size_z = full_res_ifd_count;
            }
        }
        full_res_ifd_count
    }
}

impl Default for LsmReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for LsmReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("lsm"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        // LSM files are TIFF; we rely on extension detection since the TIFF
        // reader also matches magic bytes.
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
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
        let full_res_ifd_count = self.configure_full_resolution_series(best_series);
        let tiff_meta = self.inner.metadata().clone();

        // Build corrected metadata using LSM dimensions
        let dim_z = lsm_info.dim_z;
        let dim_c = lsm_info.dim_c;
        let dim_t = lsm_info.dim_t;
        let packed_channels =
            lsm_uses_packed_channels(dim_z, dim_c, dim_t, tiff_meta.size_c, full_res_ifd_count);
        let image_count = if packed_channels {
            checked_plane_count(dim_z, 1, dim_t)?
        } else {
            checked_plane_count(dim_z, dim_c, dim_t)?
        };

        let pixel_type = lsm_pixel_type(lsm_info.data_type, tiff_meta.bits_per_pixel as u16)?;
        let is_rgb = packed_channels && tiff_meta.is_rgb;

        let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
        meta_map.insert(
            "voxel_size_x_um".into(),
            MetadataValue::Float(lsm_info.voxel_x * 1e6),
        );
        meta_map.insert(
            "voxel_size_y_um".into(),
            MetadataValue::Float(lsm_info.voxel_y * 1e6),
        );
        meta_map.insert(
            "voxel_size_z_um".into(),
            MetadataValue::Float(lsm_info.voxel_z * 1e6),
        );

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
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
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

    fn series_count(&self) -> usize {
        usize::from(self.meta.is_some())
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.meta.is_none() || s != 0 {
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
        let count = self.meta.as_ref().map(|m| m.image_count).unwrap_or(0);
        if plane_index >= count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let inner_count = self.inner.metadata().image_count;
        let inner_idx = resolve_lsm_plane_index(plane_index, count, inner_count)?;
        self.inner.open_bytes(inner_idx)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let count = self.meta.as_ref().map(|m| m.image_count).unwrap_or(0);
        if plane_index >= count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let inner_count = self.inner.metadata().image_count;
        let inner_idx = resolve_lsm_plane_index(plane_index, count, inner_count)?;
        self.inner.open_bytes_region(inner_idx, x, y, w, h)
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
        let get_f = |k: &str| -> Option<f64> {
            if let Some(MetadataValue::Float(v)) = meta.series_metadata.get(k) {
                Some(*v)
            } else {
                None
            }
        };
        // Already stored in µm
        img.physical_size_x = get_f("voxel_size_x_um");
        img.physical_size_y = get_f("voxel_size_y_um");
        img.physical_size_z = get_f("voxel_size_z_um");
        Some(ome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lsm_plane_mapping_rejects_logical_planes_without_physical_ifds() {
        assert_eq!(resolve_lsm_plane_index(0, 3, 2).unwrap(), 0);
        assert_eq!(resolve_lsm_plane_index(1, 3, 2).unwrap(), 1);
        assert!(matches!(
            resolve_lsm_plane_index(2, 3, 2),
            Err(BioFormatsError::PlaneOutOfRange(2))
        ));
    }

    #[test]
    fn lsm_plane_mapping_rejects_planes_past_logical_count() {
        assert!(matches!(
            resolve_lsm_plane_index(2, 2, 4),
            Err(BioFormatsError::PlaneOutOfRange(2))
        ));
    }

    #[test]
    fn lsm_channel_split_planes_are_not_treated_as_packed_rgb() {
        assert!(!lsm_uses_packed_channels(1, 3, 1, 1, 3));
        assert_eq!(checked_plane_count(1, 3, 1).unwrap(), 3);
    }

    #[test]
    fn lsm_packed_channels_use_one_physical_ifd_per_zt_plane() {
        assert!(lsm_uses_packed_channels(2, 3, 4, 3, 8));
        assert_eq!(checked_plane_count(2, 1, 4).unwrap(), 8);
    }
}
