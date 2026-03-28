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
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
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

// ═══════════════════════════════════════════════════════════════════════════════
// AVI Writer — uncompressed RGB24 or grayscale
// ═══════════════════════════════════════════════════════════════════════════════

/// AVI writer for exporting image stacks as uncompressed AVI video.
///
/// Supports 8-bit grayscale and 24-bit RGB. Each plane becomes one frame.
pub struct AviWriter {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    planes: Vec<Vec<u8>>,
    fps: u32,
}

impl AviWriter {
    pub fn new() -> Self {
        AviWriter { path: None, meta: None, planes: Vec::new(), fps: 10 }
    }

    /// Set frames per second (default: 10).
    pub fn with_fps(mut self, fps: u32) -> Self {
        self.fps = fps;
        self
    }
}

impl Default for AviWriter {
    fn default() -> Self { Self::new() }
}

fn write_fourcc(w: &mut impl Write, cc: &[u8; 4]) -> std::io::Result<()> { w.write_all(cc) }
fn write_u32_le(w: &mut impl Write, v: u32) -> std::io::Result<()> { w.write_all(&v.to_le_bytes()) }
fn write_u16_le(w: &mut impl Write, v: u16) -> std::io::Result<()> { w.write_all(&v.to_le_bytes()) }

impl crate::common::writer::FormatWriter for AviWriter {
    fn is_this_type(&self, path: &Path) -> bool {
        let ext = path.extension().and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("avi"))
    }

    fn set_metadata(&mut self, meta: &ImageMetadata) -> Result<()> {
        self.meta = Some(meta.clone());
        self.planes.clear();
        Ok(())
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn save_bytes(&mut self, _plane_index: u32, data: &[u8]) -> Result<()> {
        self.planes.push(data.to_vec());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        let meta = self.meta.take().ok_or(BioFormatsError::NotInitialized)?;
        let path = self.path.take().ok_or(BioFormatsError::NotInitialized)?;

        let width = meta.size_x;
        let height = meta.size_y;
        let is_rgb = meta.is_rgb && meta.size_c >= 3;
        let bpp: u16 = if is_rgb { 24 } else { 8 };
        let _frame_bytes = width as usize * height as usize * (bpp as usize / 8);
        let row_bytes = width as usize * (bpp as usize / 8);
        // AVI rows must be 4-byte aligned
        let padded_row = (row_bytes + 3) & !3;
        let padded_frame = padded_row * height as usize;
        let n_frames = self.planes.len() as u32;
        let fps = self.fps;
        let usec_per_frame = if fps > 0 { 1_000_000 / fps } else { 100_000 };

        let f = File::create(&path).map_err(BioFormatsError::Io)?;
        let mut w = BufWriter::new(f);

        // Compute sizes
        let movi_data_size = n_frames * (8 + padded_frame as u32); // chunk header + data per frame
        let movi_list_size = 4 + movi_data_size; // "movi" + chunks
        let strf_size: u32 = 40 + if !is_rgb { 256 * 4 } else { 0 }; // BITMAPINFOHEADER + palette
        let strl_size: u32 = 4 + (8 + 56) + (8 + strf_size); // "strl" + strh + strf
        let hdrl_size: u32 = 4 + (8 + 56) + (8 + strl_size); // "hdrl" + avih + strl_list
        let riff_size: u32 = 4 + (8 + hdrl_size) + (8 + movi_list_size);

        // RIFF header
        write_fourcc(&mut w, b"RIFF").map_err(BioFormatsError::Io)?;
        write_u32_le(&mut w, riff_size).map_err(BioFormatsError::Io)?;
        write_fourcc(&mut w, b"AVI ").map_err(BioFormatsError::Io)?;

        // LIST hdrl
        write_fourcc(&mut w, b"LIST").map_err(BioFormatsError::Io)?;
        write_u32_le(&mut w, hdrl_size).map_err(BioFormatsError::Io)?;
        write_fourcc(&mut w, b"hdrl").map_err(BioFormatsError::Io)?;

        // avih (AVIMAINHEADER, 56 bytes)
        write_fourcc(&mut w, b"avih").map_err(BioFormatsError::Io)?;
        write_u32_le(&mut w, 56).map_err(BioFormatsError::Io)?;
        write_u32_le(&mut w, usec_per_frame).map_err(BioFormatsError::Io)?; // dwMicroSecPerFrame
        write_u32_le(&mut w, 0).map_err(BioFormatsError::Io)?; // dwMaxBytesPerSec
        write_u32_le(&mut w, 0).map_err(BioFormatsError::Io)?; // dwPaddingGranularity
        write_u32_le(&mut w, 0x10).map_err(BioFormatsError::Io)?; // dwFlags (AVIF_HASINDEX)
        write_u32_le(&mut w, n_frames).map_err(BioFormatsError::Io)?; // dwTotalFrames
        write_u32_le(&mut w, 0).map_err(BioFormatsError::Io)?; // dwInitialFrames
        write_u32_le(&mut w, 1).map_err(BioFormatsError::Io)?; // dwStreams
        write_u32_le(&mut w, padded_frame as u32).map_err(BioFormatsError::Io)?; // dwSuggestedBufferSize
        write_u32_le(&mut w, width).map_err(BioFormatsError::Io)?; // dwWidth
        write_u32_le(&mut w, height).map_err(BioFormatsError::Io)?; // dwHeight
        w.write_all(&[0u8; 16]).map_err(BioFormatsError::Io)?; // dwReserved[4]

        // LIST strl
        write_fourcc(&mut w, b"LIST").map_err(BioFormatsError::Io)?;
        write_u32_le(&mut w, strl_size).map_err(BioFormatsError::Io)?;
        write_fourcc(&mut w, b"strl").map_err(BioFormatsError::Io)?;

        // strh (AVISTREAMHEADER, 56 bytes)
        write_fourcc(&mut w, b"strh").map_err(BioFormatsError::Io)?;
        write_u32_le(&mut w, 56).map_err(BioFormatsError::Io)?;
        write_fourcc(&mut w, b"vids").map_err(BioFormatsError::Io)?; // fccType
        write_fourcc(&mut w, b"DIB ").map_err(BioFormatsError::Io)?; // fccHandler (uncompressed)
        write_u32_le(&mut w, 0).map_err(BioFormatsError::Io)?; // dwFlags
        write_u16_le(&mut w, 0).map_err(BioFormatsError::Io)?; // wPriority
        write_u16_le(&mut w, 0).map_err(BioFormatsError::Io)?; // wLanguage
        write_u32_le(&mut w, 0).map_err(BioFormatsError::Io)?; // dwInitialFrames
        write_u32_le(&mut w, 1).map_err(BioFormatsError::Io)?; // dwScale
        write_u32_le(&mut w, fps).map_err(BioFormatsError::Io)?; // dwRate
        write_u32_le(&mut w, 0).map_err(BioFormatsError::Io)?; // dwStart
        write_u32_le(&mut w, n_frames).map_err(BioFormatsError::Io)?; // dwLength
        write_u32_le(&mut w, padded_frame as u32).map_err(BioFormatsError::Io)?; // dwSuggestedBufferSize
        write_u32_le(&mut w, 0).map_err(BioFormatsError::Io)?; // dwQuality
        write_u32_le(&mut w, 0).map_err(BioFormatsError::Io)?; // dwSampleSize
        w.write_all(&[0u8; 8]).map_err(BioFormatsError::Io)?; // rcFrame

        // strf (BITMAPINFOHEADER, 40 bytes + optional palette)
        write_fourcc(&mut w, b"strf").map_err(BioFormatsError::Io)?;
        write_u32_le(&mut w, strf_size).map_err(BioFormatsError::Io)?;
        write_u32_le(&mut w, 40).map_err(BioFormatsError::Io)?; // biSize
        write_u32_le(&mut w, width).map_err(BioFormatsError::Io)?; // biWidth
        write_u32_le(&mut w, height).map_err(BioFormatsError::Io)?; // biHeight (positive = bottom-up)
        write_u16_le(&mut w, 1).map_err(BioFormatsError::Io)?; // biPlanes
        write_u16_le(&mut w, bpp).map_err(BioFormatsError::Io)?; // biBitCount
        write_u32_le(&mut w, 0).map_err(BioFormatsError::Io)?; // biCompression = BI_RGB
        write_u32_le(&mut w, padded_frame as u32).map_err(BioFormatsError::Io)?; // biSizeImage
        write_u32_le(&mut w, 0).map_err(BioFormatsError::Io)?; // biXPelsPerMeter
        write_u32_le(&mut w, 0).map_err(BioFormatsError::Io)?; // biYPelsPerMeter
        write_u32_le(&mut w, if is_rgb { 0 } else { 256 }).map_err(BioFormatsError::Io)?; // biClrUsed
        write_u32_le(&mut w, 0).map_err(BioFormatsError::Io)?; // biClrImportant
        // Grayscale palette
        if !is_rgb {
            for i in 0u16..256 {
                let b = i as u8;
                w.write_all(&[b, b, b, 0]).map_err(BioFormatsError::Io)?; // BGRA
            }
        }

        // LIST movi
        write_fourcc(&mut w, b"LIST").map_err(BioFormatsError::Io)?;
        write_u32_le(&mut w, movi_list_size).map_err(BioFormatsError::Io)?;
        write_fourcc(&mut w, b"movi").map_err(BioFormatsError::Io)?;

        let pad_bytes = padded_row - row_bytes;
        let pad = vec![0u8; pad_bytes];

        for plane in &self.planes {
            write_fourcc(&mut w, b"00db").map_err(BioFormatsError::Io)?; // uncompressed frame
            write_u32_le(&mut w, padded_frame as u32).map_err(BioFormatsError::Io)?;
            // AVI stores rows bottom-up
            for y in (0..height as usize).rev() {
                let offset = y * row_bytes;
                let end = (offset + row_bytes).min(plane.len());
                if offset < plane.len() {
                    w.write_all(&plane[offset..end]).map_err(BioFormatsError::Io)?;
                    if end - offset < row_bytes {
                        w.write_all(&vec![0u8; row_bytes - (end - offset)]).map_err(BioFormatsError::Io)?;
                    }
                } else {
                    w.write_all(&vec![0u8; row_bytes]).map_err(BioFormatsError::Io)?;
                }
                if pad_bytes > 0 {
                    w.write_all(&pad).map_err(BioFormatsError::Io)?;
                }
            }
        }

        w.flush().map_err(BioFormatsError::Io)?;
        self.planes.clear();
        Ok(())
    }

    fn can_do_stacks(&self) -> bool { true }
}
