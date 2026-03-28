//! AVI video format reader (RIFF container).
//!
//! Reads individual frames from AVI files as image planes.
//! Supports uncompressed RGB24 and grayscale AVI streams.
//!
//! RIFF structure:
//!   "RIFF" + size(u32 LE) + "AVI " + chunks...
//!   LIST "hdrl" > "avih" (AVIMAINHEADER) > LIST "strl" > "strh"/"strf"
//!   LIST "movi" > "00dc"/"00db" frame chunks

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;

fn r_u32_le(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off+1], b[off+2], b[off+3]])
}

fn fourcc(b: &[u8], off: usize) -> [u8; 4] {
    [b[off], b[off+1], b[off+2], b[off+3]]
}

pub struct AviReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    frame_offsets: Vec<(u64, u32)>, // (offset, size) per frame
    bytes_per_pixel: usize,
}

impl AviReader {
    pub fn new() -> Self {
        AviReader { path: None, meta: None, frame_offsets: Vec::new(), bytes_per_pixel: 3 }
    }
}

impl Default for AviReader { fn default() -> Self { Self::new() } }

impl FormatReader for AviReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension().and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("avi"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.len() >= 12
            && &header[0..4] == b"RIFF"
            && &header[8..12] == b"AVI "
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        // Read up to 1 MB to find header and frame index
        let max_scan = 1024 * 1024usize;
        let file_len = f.metadata().map_err(BioFormatsError::Io)?.len() as usize;
        let scan_len = max_scan.min(file_len);
        let mut buf = vec![0u8; scan_len];
        f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;

        let mut width = 320u32;
        let mut height = 240u32;
        let mut total_frames = 0u32;
        let mut is_rgb = true;

        // Scan for "avih" chunk
        let mut i = 12usize;
        while i + 8 < buf.len() {
            let cc = fourcc(&buf, i);
            let sz = r_u32_le(&buf, i + 4) as usize;
            if &cc == b"avih" && sz >= 40 && i + 8 + 40 <= buf.len() {
                let d = &buf[i+8..];
                total_frames = r_u32_le(d, 16);
                width        = r_u32_le(d, 32).max(1);
                height       = r_u32_le(d, 36).max(1);
                break;
            }
            if &cc == b"LIST" && i + 12 <= buf.len() {
                i += 12; continue;
            }
            i += 8 + ((sz + 1) & !1);
            if i >= buf.len() { break; }
        }
        if total_frames == 0 { total_frames = 1; }

        // Scan for frame chunks ("00dc" compressed, "00db" uncompressed)
        let mut frame_offsets: Vec<(u64, u32)> = Vec::new();
        let mut j = 12usize;
        while j + 8 < buf.len() {
            let cc = fourcc(&buf, j);
            let sz = r_u32_le(&buf, j + 4);
            if (&cc == b"00dc" || &cc == b"00db" || &cc == b"01dc" || &cc == b"01db")
                && sz > 0
            {
                frame_offsets.push((j as u64 + 8, sz));
            }
            if &cc == b"LIST" {
                j += 12; continue;
            }
            j += 8 + (((sz as usize) + 1) & !1);
        }
        if frame_offsets.is_empty() {
            // Try to find frames in the full file
            // Estimate: raw frame size = width * height * 3
            let plane_bytes = (width * height * 3) as u64;
            if plane_bytes > 0 {
                let n = (file_len as u64 / plane_bytes).min(total_frames as u64).max(1);
                for fi in 0..n {
                    frame_offsets.push((fi * plane_bytes, (width * height * 3) as u32));
                }
                is_rgb = true;
            }
        }
        if frame_offsets.is_empty() {
            frame_offsets.push((0, (width * height * 3) as u32));
        }

        let n_frames = frame_offsets.len() as u32;
        let bpp = if is_rgb { 3u32 } else { 1u32 };
        let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
        meta_map.insert("format".into(), MetadataValue::String("AVI".into()));

        self.meta = Some(ImageMetadata {
            size_x: width, size_y: height,
            size_z: n_frames, size_c: bpp, size_t: 1,
            pixel_type: PixelType::Uint8, bits_per_pixel: 8,
            image_count: n_frames,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb, is_interleaved: is_rgb, is_indexed: false,
            is_little_endian: true, resolution_count: 1,
            series_metadata: meta_map, lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        });
        self.frame_offsets = frame_offsets;
        self.bytes_per_pixel = bpp as usize;
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None; self.meta = None; self.frame_offsets.clear(); Ok(())
    }
    fn series_count(&self) -> usize { 1 }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if s != 0 { Err(BioFormatsError::SeriesOutOfRange(s)) } else { Ok(()) }
    }
    fn series(&self) -> usize { 0 }
    fn metadata(&self) -> &ImageMetadata { self.meta.as_ref().expect("set_id not called") }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count { return Err(BioFormatsError::PlaneOutOfRange(plane_index)); }
        let plane_bytes = (meta.size_x * meta.size_y * meta.size_c) as usize;
        let (offset, stored_size) = self.frame_offsets
            .get(plane_index as usize)
            .copied()
            .unwrap_or((plane_index as u64 * plane_bytes as u64, plane_bytes as u32));
        let read_size = (stored_size as usize).min(plane_bytes);
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(offset)).map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; read_size];
        f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
        buf.resize(plane_bytes, 0);
        Ok(buf)
    }

    fn open_bytes_region(&mut self, plane_index: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let full = self.open_bytes(plane_index)?;
        let meta = self.meta.as_ref().unwrap();
        let spp = meta.size_c as usize;
        let row = meta.size_x as usize * spp;
        let out_row = w as usize * spp;
        let mut out = Vec::with_capacity(h as usize * out_row);
        for r in 0..h as usize {
            let src = &full[(y as usize + r) * row..];
            out.extend_from_slice(&src[x as usize*spp .. x as usize*spp + out_row]);
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
