//! Hamamatsu DCIMG time-lapse format reader.
//!
//! DCIMG is a proprietary Hamamatsu format for sCMOS camera time-lapse data.
//! The file starts with the 8-byte magic "DCIMG\0\0\0".
//!
//! The on-disk header is versioned. Version 0x01000000 is parsed from the
//! offsets used by Bio-Formats' DCIMGReader; older synthetic fixtures are kept
//! on the previous simplified layout for compatibility.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;

fn r_u32_le(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

fn r_u64_le(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes([
        b[off],
        b[off + 1],
        b[off + 2],
        b[off + 3],
        b[off + 4],
        b[off + 5],
        b[off + 6],
        b[off + 7],
    ])
}

pub struct DcimgReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    data_offset: u64,
    bytes_per_row: usize,
    bytes_per_image: u64,
    frame_footer_size: u64,
}

impl DcimgReader {
    pub fn new() -> Self {
        DcimgReader {
            path: None,
            meta: None,
            data_offset: 0,
            bytes_per_row: 0,
            bytes_per_image: 0,
            frame_footer_size: 0,
        }
    }
}

impl Default for DcimgReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for DcimgReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("dcimg"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.len() >= 5 && &header[..5] == b"DCIMG"
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        let mut hdr = vec![0u8; 512];
        let n = f.read(&mut hdr).map_err(BioFormatsError::Io)?;
        hdr.truncate(n);
        if hdr.len() < 64 {
            return Err(BioFormatsError::Format(
                "DCIMG header is shorter than 64 bytes".into(),
            ));
        }

        let version = r_u32_le(&hdr, 8);
        let (
            header_size,
            n_frames,
            width,
            height,
            pixel_type_code,
            bytes_per_row,
            data_offset,
            bytes_per_image,
            frame_footer_size,
        ) = if version >= 0x0100_0000 {
            let header_size = r_u32_le(&hdr, 40) as u64;
            let header_start = header_size as usize;
            if hdr.len() < header_start + 124 {
                return Err(BioFormatsError::Format(
                    "DCIMG version 1 header is truncated".into(),
                ));
            }

            let n_frames = r_u32_le(&hdr, header_start + 60).max(1);
            let pixel_type_code = r_u32_le(&hdr, header_start + 64);
            let width = r_u32_le(&hdr, header_start + 72).max(1);
            let height = r_u32_le(&hdr, header_start + 76).max(1);
            let bytes_per_row = r_u32_le(&hdr, header_start + 80) as usize;
            let bytes_per_image = r_u32_le(&hdr, header_start + 84) as u64;
            let data_offset = header_size + r_u64_le(&hdr, header_start + 96);
            let frame_footer_size = r_u32_le(&hdr, header_start + 120) as u64;
            (
                header_size,
                n_frames,
                width,
                height,
                pixel_type_code,
                bytes_per_row,
                data_offset,
                bytes_per_image,
                frame_footer_size,
            )
        } else {
            let header_size = r_u32_le(&hdr, 16) as u64;
            let n_frames = r_u32_le(&hdr, 20).max(1);
            let width = r_u32_le(&hdr, 32).max(1);
            let height = r_u32_le(&hdr, 36).max(1);
            let bit_depth = r_u32_le(&hdr, 40);
            let bytes_per_row = r_u32_le(&hdr, 48) as usize;
            (
                header_size,
                n_frames,
                width,
                height,
                bit_depth,
                bytes_per_row,
                if header_size > 64 { header_size } else { 64 },
                0,
                0,
            )
        };

        let (pixel_type, bpp): (PixelType, u8) = if version >= 0x0100_0000 {
            match pixel_type_code {
                1 => (PixelType::Uint8, 8),
                2 => (PixelType::Uint16, 16),
                other => {
                    return Err(BioFormatsError::Format(format!(
                        "unsupported DCIMG pixel type {other}"
                    )));
                }
            }
        } else {
            match pixel_type_code {
                8 => (PixelType::Uint8, 8),
                32 => (PixelType::Float32, 32),
                _ => (PixelType::Uint16, 16), // default: 16-bit
            }
        };

        let bps = pixel_type.bytes_per_sample();
        let bpr = if bytes_per_row > 0 {
            bytes_per_row
        } else {
            width as usize * bps
        };

        let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
        meta_map.insert(
            "format".into(),
            MetadataValue::String("Hamamatsu DCIMG".into()),
        );
        meta_map.insert("version".into(), MetadataValue::Int(version as i64));
        meta_map.insert("bit_depth".into(), MetadataValue::Int(bpp as i64));
        meta_map.insert("header_size".into(), MetadataValue::Int(header_size as i64));

        self.meta = Some(ImageMetadata {
            size_x: width,
            size_y: height,
            size_z: n_frames,
            size_c: 1,
            size_t: 1,
            pixel_type,
            bits_per_pixel: bpp,
            image_count: n_frames,
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

        self.data_offset = data_offset;
        self.bytes_per_row = bpr;
        self.bytes_per_image = bytes_per_image;
        self.frame_footer_size = frame_footer_size;
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
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
        let bps = meta.pixel_type.bytes_per_sample();
        let row_bytes = if self.bytes_per_row > 0 {
            self.bytes_per_row
        } else {
            meta.size_x as usize * bps
        };
        let plane_bytes = row_bytes * meta.size_y as usize;
        let frame_stride = if self.bytes_per_image > 0 {
            self.bytes_per_image + self.frame_footer_size
        } else {
            plane_bytes as u64
        };
        let offset = self.data_offset + plane_index as u64 * frame_stride;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(offset))
            .map_err(BioFormatsError::Io)?;
        let mut raw = vec![0u8; plane_bytes];
        f.read_exact(&mut raw).map_err(BioFormatsError::Io)?;

        // Four-corner footer correction.
        //
        // DCIMG overwrites the first four pixels of every frame with internal
        // bookkeeping data; the original values of those four pixels are
        // preserved in the per-frame footer. When a footer is present we read it
        // and restore the first four pixels of the frame in row-major order.
        //
        // The footer is laid out as: <reserved> followed by the four original
        // corner pixels stored as native-width samples at the tail of the
        // footer. We take the last `4 * bps` bytes of the footer as those
        // samples (little-endian), matching the DCIMG on-disk ordering.
        if self.frame_footer_size as usize >= 4 * bps {
            let footer_off = offset + self.bytes_per_image;
            if f.seek(SeekFrom::Start(footer_off)).is_ok() {
                let mut footer = vec![0u8; self.frame_footer_size as usize];
                if f.read_exact(&mut footer).is_ok() {
                    let corner = &footer[footer.len() - 4 * bps..];
                    let n = (4 * bps).min(raw.len());
                    raw[..n].copy_from_slice(&corner[..n]);
                }
            }
        }

        let out_row = meta.size_x as usize * bps;
        if row_bytes == out_row {
            return Ok(raw);
        }
        let mut out = Vec::with_capacity(meta.size_y as usize * out_row);
        for r in 0..meta.size_y as usize {
            out.extend_from_slice(&raw[r * row_bytes..r * row_bytes + out_row]);
        }
        Ok(out)
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
        crop_full_plane("DCIMG", &full, meta, 1, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = ((meta.size_x - tw) / 2, (meta.size_y - th) / 2);
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}
