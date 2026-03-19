//! CellH5 (.ch5) format reader.
//!
//! CellH5 is an HDF5-based format for cell biology HCS data, developed alongside
//! CellProfiler and used in the Sommer et al. cell tracking / segmentation pipeline.
//!
//! Common HDF5 layout:
//!   sample/0/position/{well}/image/channel/{ch}   — uint16 [n_frames, y, x] or [y, x]
//!   plate/{plate}/experiment/{well}/image/channel/{ch}
//!
//! Detection: extension `.ch5` only (HDF5 magic-byte detection disabled to avoid
//! conflicts with other HDF5-based readers).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use bioformats_common::error::{BioFormatsError, Result};
use bioformats_common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use bioformats_common::pixel_type::PixelType;
use bioformats_common::reader::FormatReader;

pub struct CellH5Reader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    /// HDF5 dataset paths to per-channel image data.
    channel_paths: Vec<String>,
}

impl CellH5Reader {
    pub fn new() -> Self {
        CellH5Reader {
            path: None,
            meta: None,
            channel_paths: Vec::new(),
        }
    }
}

impl Default for CellH5Reader {
    fn default() -> Self {
        Self::new()
    }
}

/// Walk known CellH5 layout patterns and collect leaf channel dataset paths.
fn find_image_datasets(file: &hdf5::File) -> Vec<String> {
    let mut paths = Vec::new();

    for root in &["sample", "plate"] {
        let root_g = match file.group(root) {
            Ok(g) => g,
            Err(_) => continue,
        };
        let plates = match root_g.member_names() {
            Ok(m) => m,
            Err(_) => continue,
        };
        for plate in &plates {
            for mid in &["position", "experiment"] {
                let mid_path = format!("{root}/{plate}/{mid}");
                let mid_g = match file.group(&mid_path) {
                    Ok(g) => g,
                    Err(_) => continue,
                };
                let wells = match mid_g.member_names() {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                for well in &wells {
                    let ch_path = format!("{mid_path}/{well}/image/channel");
                    let ch_g = match file.group(&ch_path) {
                        Ok(g) => g,
                        Err(_) => continue,
                    };
                    let chs = match ch_g.member_names() {
                        Ok(m) => m,
                        Err(_) => continue,
                    };
                    for ch in &chs {
                        paths.push(format!("{ch_path}/{ch}"));
                    }
                }
            }
        }
    }

    paths
}

fn parse_cellh5(path: &Path) -> Result<(ImageMetadata, Vec<String>)> {
    let file = hdf5::File::open(path)
        .map_err(|e| BioFormatsError::Format(format!("HDF5 open error: {e}")))?;

    let channel_paths = find_image_datasets(&file);

    let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
    meta_map.insert("format".into(), MetadataValue::String("CellH5".into()));

    if channel_paths.is_empty() {
        // Unknown layout — return a minimal placeholder
        log::warn!("CellH5: no image datasets found, returning placeholder metadata");
        let meta = ImageMetadata {
            size_x: 512,
            size_y: 512,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type: PixelType::Uint16,
            bits_per_pixel: 16,
            image_count: 1,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: true,
            resolution_count: 1,
            series_metadata: meta_map,
            lookup_table: None,
        };
        return Ok((meta, channel_paths));
    }

    // Inspect the first channel dataset to get dimensions
    let ds = file
        .dataset(&channel_paths[0])
        .map_err(|e| BioFormatsError::Format(format!("dataset {}: {e}", channel_paths[0])))?;

    let shape = ds.shape();
    let (size_x, size_y, size_z, size_t) = match shape.len() {
        3 => {
            // [n_frames, y, x]
            let nt = shape[0] as u32;
            let sy = shape[1] as u32;
            let sx = shape[2] as u32;
            (sx, sy, 1u32, nt)
        }
        2 => {
            // [y, x]
            let sy = shape[0] as u32;
            let sx = shape[1] as u32;
            (sx, sy, 1u32, 1u32)
        }
        _ => (512u32, 512u32, 1u32, 1u32),
    };

    // Determine pixel type from dataset dtype size
    let pixel_type = match ds.dtype().map(|d| d.size()).unwrap_or(2) {
        1 => PixelType::Uint8,
        4 => PixelType::Uint32,
        _ => PixelType::Uint16,
    };
    let bytes_per_sample: usize = match pixel_type {
        PixelType::Uint8 => 1,
        PixelType::Uint32 => 4,
        _ => 2,
    };

    let size_c = channel_paths.len() as u32;
    let image_count = size_z * size_c * size_t;

    let meta = ImageMetadata {
        size_x,
        size_y,
        size_z,
        size_c,
        size_t,
        pixel_type,
        bits_per_pixel: (bytes_per_sample * 8) as u8,
        image_count,
        dimension_order: DimensionOrder::XYZCT,
        is_rgb: false,
        is_interleaved: false,
        is_indexed: false,
        is_little_endian: true,
        resolution_count: 1,
        series_metadata: meta_map,
        lookup_table: None,
    };

    Ok((meta, channel_paths))
}

impl FormatReader for CellH5Reader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("ch5"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        // Disabled — rely on extension only to avoid conflicts with other HDF5 readers.
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let (meta, channel_paths) = parse_cellh5(path)?;
        self.meta = Some(meta);
        self.path = Some(path.to_path_buf());
        self.channel_paths = channel_paths;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.channel_paths.clear();
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
        self.meta.as_ref().expect("set_id not called")
    }

    fn resolution_count(&self) -> usize {
        1
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if level != 0 {
            Err(BioFormatsError::Format(format!(
                "CellH5: resolution {level} out of range"
            )))
        } else {
            Ok(())
        }
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }

        let plane_pixels = meta.size_x as usize * meta.size_y as usize;
        let bytes_per_sample = (meta.bits_per_pixel / 8) as usize;
        let plane_bytes = plane_pixels * bytes_per_sample;

        if self.channel_paths.is_empty() {
            return Ok(vec![0u8; plane_bytes]);
        }

        let sz = meta.size_z as usize;
        let sc = meta.size_c as usize;
        let z = (plane_index as usize) % sz;
        let c = (plane_index as usize / sz) % sc;
        let t = (plane_index as usize) / (sz * sc);

        let ds_path = self.channel_paths[c].clone();
        let path = self
            .path
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?
            .clone();
        let file = hdf5::File::open(&path)
            .map_err(|e| BioFormatsError::Format(format!("HDF5: {e}")))?;
        let ds = file
            .dataset(&ds_path)
            .map_err(|e| BioFormatsError::Format(format!("dataset {ds_path}: {e}")))?;

        let raw: Vec<u8> = match bytes_per_sample {
            1 => ds
                .read_raw::<u8>()
                .map_err(|e| BioFormatsError::Format(format!("HDF5 read: {e}")))?,
            2 => {
                let words: Vec<u16> = ds
                    .read_raw::<u16>()
                    .map_err(|e| BioFormatsError::Format(format!("HDF5 read: {e}")))?;
                words.iter().flat_map(|w| w.to_le_bytes()).collect()
            }
            4 => {
                let dwords: Vec<u32> = ds
                    .read_raw::<u32>()
                    .map_err(|e| BioFormatsError::Format(format!("HDF5 read: {e}")))?;
                dwords.iter().flat_map(|d| d.to_le_bytes()).collect()
            }
            _ => ds
                .read_raw::<u8>()
                .map_err(|e| BioFormatsError::Format(format!("HDF5 read: {e}")))?,
        };

        // raw layout: [t, z, y, x] → [t, y, x] when sz==1, or [y, x]
        // Offset for frame t, z-plane z:
        let frame_bytes = meta.size_z as usize * plane_bytes;
        let offset = t * frame_bytes + z * plane_bytes;

        if offset + plane_bytes <= raw.len() {
            Ok(raw[offset..offset + plane_bytes].to_vec())
        } else {
            Ok(vec![0u8; plane_bytes])
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
        let meta = self.meta.as_ref().unwrap();
        let bps = (meta.bits_per_pixel / 8) as usize;
        let row_bytes = meta.size_x as usize * bps;
        let out_row = w as usize * bps;
        let mut out = Vec::with_capacity(h as usize * out_row);
        for r in 0..h as usize {
            let src_start = (y as usize + r) * row_bytes + x as usize * bps;
            if src_start + out_row <= full.len() {
                out.extend_from_slice(&full[src_start..src_start + out_row]);
            } else {
                out.extend(std::iter::repeat(0u8).take(out_row));
            }
        }
        Ok(out)
    }

    fn open_thumb_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(0, tx, ty, tw, th)
    }
}
