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
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

fn r_i32_le(b: &[u8], off: usize) -> i32 {
    i32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

fn r_u16_le(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}

fn fourcc(b: &[u8], off: usize) -> [u8; 4] {
    [b[off], b[off + 1], b[off + 2], b[off + 3]]
}

fn fourcc_to_string(cc: [u8; 4]) -> String {
    if cc == [0, 0, 0, 0] {
        return "BI_RGB".into();
    }
    String::from_utf8_lossy(&cc).trim_end().to_string()
}

fn chunk_end(payload: usize, size: u32, limit: usize) -> Result<usize> {
    payload
        .checked_add(size as usize)
        .filter(|&end| end <= limit)
        .ok_or_else(|| {
            BioFormatsError::Format("AVI chunk extends past containing RIFF list".into())
        })
}

fn padded_chunk_end(pos: usize, size: u32, limit: usize) -> Result<usize> {
    let payload = pos
        .checked_add(8)
        .ok_or_else(|| BioFormatsError::Format("AVI chunk offset overflow".into()))?;
    let end = chunk_end(payload, size, limit)?;
    end.checked_add((size as usize) & 1)
        .filter(|&padded| padded <= limit)
        .ok_or_else(|| {
            BioFormatsError::Format("AVI padded chunk extends past containing RIFF list".into())
        })
}

fn is_video_frame_chunk(cc: [u8; 4]) -> bool {
    cc[0].is_ascii_digit()
        && cc[1].is_ascii_digit()
        && cc[2] == b'd'
        && (cc[3] == b'b' || cc[3] == b'c')
}

fn is_raw_handler(handler: [u8; 4]) -> bool {
    handler == [0, 0, 0, 0]
        || handler == *b"DIB "
        || handler == *b"RGB "
        || handler == *b"RAW "
        || handler == *b"    "
}

#[derive(Default)]
struct AviParse {
    width: u32,
    height: u32,
    total_frames: u32,
    bit_count: u16,
    compression: [u8; 4],
    stream_handler: [u8; 4],
    is_rgb: bool,
    top_down: bool,
    movi_data_start: Option<usize>,
    movi_data_end: Option<usize>,
    frame_chunks: Vec<(u64, u32)>,
    idx1_frames: Vec<(u64, u32)>,
    odml_frames: Vec<(u64, u32)>,
}

impl AviParse {
    fn new() -> Self {
        Self {
            width: 320,
            height: 240,
            bit_count: 24,
            is_rgb: true,
            ..Self::default()
        }
    }
}

fn parse_avi(buf: &[u8]) -> Result<AviParse> {
    if buf.len() < 12 || &buf[0..4] != b"RIFF" || &buf[8..12] != b"AVI " {
        return Err(BioFormatsError::Format("Not an AVI RIFF file".into()));
    }
    let riff_end = (8usize)
        .checked_add(r_u32_le(buf, 4) as usize)
        .map(|end| end.min(buf.len()))
        .ok_or_else(|| BioFormatsError::Format("AVI RIFF size overflow".into()))?;
    let mut parsed = AviParse::new();
    parse_riff_chunks(buf, 12, riff_end, &mut parsed)?;
    Ok(parsed)
}

fn parse_riff_chunks(
    buf: &[u8],
    mut pos: usize,
    limit: usize,
    parsed: &mut AviParse,
) -> Result<()> {
    while pos + 8 <= limit {
        let cc = fourcc(buf, pos);
        let size = r_u32_le(buf, pos + 4);
        let payload = pos + 8;
        let data_end = chunk_end(payload, size, limit)?;

        match &cc {
            b"LIST" => {
                if payload + 4 <= data_end {
                    let list_type = fourcc(buf, payload);
                    let list_data_start = payload + 4;
                    if &list_type == b"movi" {
                        parsed.movi_data_start = Some(list_data_start);
                        parsed.movi_data_end = Some(data_end);
                    }
                    parse_riff_chunks(buf, list_data_start, data_end, parsed)?;
                }
            }
            b"RIFF" => {
                if payload + 4 <= data_end {
                    parse_riff_chunks(buf, payload + 4, data_end, parsed)?;
                }
            }
            b"avih" => parse_avih(buf, payload, data_end, parsed),
            b"strh" => parse_strh(buf, payload, data_end, parsed),
            b"strf" => parse_strf(buf, payload, data_end, parsed),
            b"idx1" => parse_idx1(buf, payload, data_end, parsed)?,
            _ if cc[0] == b'i' && cc[1] == b'x' => {
                parse_odml_standard_index(buf, payload, data_end, parsed)?
            }
            _ if is_video_frame_chunk(cc) && size > 0 => {
                parsed.frame_chunks.push((payload as u64, size));
            }
            _ => {}
        }

        pos = padded_chunk_end(pos, size, limit)?;
    }
    Ok(())
}

fn parse_avih(buf: &[u8], payload: usize, data_end: usize, parsed: &mut AviParse) {
    if data_end.saturating_sub(payload) >= 40 {
        parsed.total_frames = r_u32_le(buf, payload + 16);
        parsed.width = r_u32_le(buf, payload + 32).max(1);
        parsed.height = r_u32_le(buf, payload + 36).max(1);
    }
}

fn parse_strh(buf: &[u8], payload: usize, data_end: usize, parsed: &mut AviParse) {
    if data_end.saturating_sub(payload) >= 8 && &buf[payload..payload + 4] == b"vids" {
        parsed.stream_handler = fourcc(buf, payload + 4);
    }
}

fn parse_strf(buf: &[u8], payload: usize, data_end: usize, parsed: &mut AviParse) {
    if data_end.saturating_sub(payload) >= 20 {
        let dib_height = r_i32_le(buf, payload + 8);
        parsed.width = r_u32_le(buf, payload + 4).max(1);
        parsed.height = dib_height.unsigned_abs().max(1);
        parsed.top_down = dib_height < 0;
        parsed.bit_count = r_u16_le(buf, payload + 14);
        parsed.compression = fourcc(buf, payload + 16);
        parsed.is_rgb = parsed.bit_count == 24 || parsed.bit_count == 32;
    }
}

fn parse_idx1(buf: &[u8], payload: usize, data_end: usize, parsed: &mut AviParse) -> Result<()> {
    let mut pos = payload;
    while pos + 16 <= data_end {
        let chunk_id = fourcc(buf, pos);
        let offset = r_u32_le(buf, pos + 8) as u64;
        let size = r_u32_le(buf, pos + 12);
        if is_video_frame_chunk(chunk_id) && size > 0 {
            if let Some(frame) = resolve_indexed_frame(buf, parsed, offset, size, 0) {
                parsed.idx1_frames.push(frame);
            }
        }
        pos += 16;
    }
    Ok(())
}

fn parse_odml_standard_index(
    buf: &[u8],
    payload: usize,
    data_end: usize,
    parsed: &mut AviParse,
) -> Result<()> {
    if data_end.saturating_sub(payload) < 24 {
        return Ok(());
    }
    let longs_per_entry = r_u16_le(buf, payload) as usize;
    let index_type = buf[payload + 3];
    if longs_per_entry < 2 || index_type != 1 {
        return Ok(());
    }
    let entries = r_u32_le(buf, payload + 4) as usize;
    let chunk_id = fourcc(buf, payload + 8);
    if !is_video_frame_chunk(chunk_id) {
        return Ok(());
    }
    let base = u64::from_le_bytes([
        buf[payload + 12],
        buf[payload + 13],
        buf[payload + 14],
        buf[payload + 15],
        buf[payload + 16],
        buf[payload + 17],
        buf[payload + 18],
        buf[payload + 19],
    ]);
    let entry_size = longs_per_entry * 4;
    let mut pos = payload + 24;
    for _ in 0..entries {
        if pos + entry_size > data_end {
            break;
        }
        let offset = r_u32_le(buf, pos) as u64;
        let size = r_u32_le(buf, pos + 4) & 0x7fff_ffff;
        if size > 0 {
            if let Some(frame) = resolve_indexed_frame(buf, parsed, offset, size, base) {
                parsed.odml_frames.push(frame);
            }
        }
        pos += entry_size;
    }
    Ok(())
}

fn resolve_indexed_frame(
    buf: &[u8],
    parsed: &AviParse,
    offset: u64,
    size: u32,
    base: u64,
) -> Option<(u64, u32)> {
    let mut candidates = Vec::with_capacity(4);
    if let Some(movi_start) = parsed.movi_data_start {
        candidates.push(movi_start as u64 + offset);
    }
    candidates.push(base + offset);
    candidates.push(offset);
    if offset >= 8 {
        candidates.push(offset - 8);
    }

    for candidate in candidates {
        let chunk_start = candidate as usize;
        if chunk_start + 8 <= buf.len() && is_video_frame_chunk(fourcc(buf, chunk_start)) {
            let chunk_size = r_u32_le(buf, chunk_start + 4);
            if chunk_size > 0 {
                return Some((candidate + 8, chunk_size.min(size)));
            }
        }
        let data_start = chunk_start;
        if data_start >= 8 && data_start <= buf.len() {
            let header = data_start - 8;
            if is_video_frame_chunk(fourcc(buf, header)) {
                let chunk_size = r_u32_le(buf, header + 4);
                if chunk_size > 0 {
                    return Some((data_start as u64, chunk_size.min(size)));
                }
            }
        }
    }
    None
}

pub struct AviReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    frame_offsets: Vec<(u64, u32)>, // (offset, size) per frame
    bytes_per_pixel: usize,
    top_down: bool,
}

impl AviReader {
    pub fn new() -> Self {
        AviReader {
            path: None,
            meta: None,
            frame_offsets: Vec::new(),
            bytes_per_pixel: 3,
            top_down: false,
        }
    }
}

impl Default for AviReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for AviReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("avi"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.len() >= 12 && &header[0..4] == b"RIFF" && &header[8..12] == b"AVI "
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf).map_err(BioFormatsError::Io)?;
        let parsed = parse_avi(&buf)?;

        if parsed.compression != [0, 0, 0, 0] {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "AVI compressed video stream {} is not supported; only uncompressed BI_RGB/DIB frames are supported",
                fourcc_to_string(parsed.compression)
            )));
        }
        if !is_raw_handler(parsed.stream_handler) {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "AVI compressed video stream {} is not supported; only uncompressed BI_RGB/DIB frames are supported",
                fourcc_to_string(parsed.stream_handler)
            )));
        }

        let width = parsed.width.max(1);
        let height = parsed.height.max(1);
        let bit_count = parsed.bit_count;
        if !matches!(bit_count, 8 | 24 | 32) {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "AVI uncompressed BI_RGB bit depth {bit_count} is not supported; only 8-bit grayscale and 24/32-bit RGB are supported"
            )));
        }
        let is_rgb = parsed.is_rgb;
        let frame_offsets = if !parsed.idx1_frames.is_empty() {
            parsed.idx1_frames
        } else if !parsed.odml_frames.is_empty() {
            parsed.odml_frames
        } else {
            parsed.frame_chunks
        };
        if frame_offsets.is_empty() {
            return Err(BioFormatsError::Format(
                "AVI: no uncompressed video frame chunks found".into(),
            ));
        }
        let n_frames = frame_offsets.len() as u32;
        let bpp = match bit_count {
            24 => 3u32,
            32 => 4u32,
            8 => 1u32,
            _ => {
                if is_rgb {
                    3u32
                } else {
                    1u32
                }
            }
        };
        let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
        meta_map.insert("format".into(), MetadataValue::String("AVI".into()));

        self.meta = Some(ImageMetadata {
            size_x: width,
            size_y: height,
            size_z: n_frames,
            size_c: bpp,
            size_t: 1,
            pixel_type: PixelType::Uint8,
            bits_per_pixel: 8,
            image_count: n_frames,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb,
            is_interleaved: is_rgb,
            is_indexed: false,
            is_little_endian: true,
            resolution_count: 1,
            series_metadata: meta_map,
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        });
        self.frame_offsets = frame_offsets;
        self.bytes_per_pixel = bpp as usize;
        self.top_down = parsed.top_down;
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.frame_offsets.clear();
        self.top_down = false;
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

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let channels = meta.size_c as usize;
        let plane_bytes = meta.size_x as usize * meta.size_y as usize * channels;
        let row_bytes = meta.size_x as usize * channels;
        let stored_row = (row_bytes + 3) & !3;
        let expected_stored = stored_row * meta.size_y as usize;
        let (offset, stored_size) = self
            .frame_offsets
            .get(plane_index as usize)
            .copied()
            .unwrap_or((plane_index as u64 * plane_bytes as u64, plane_bytes as u32));
        let read_size = (stored_size as usize).min(expected_stored.max(plane_bytes));
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(offset))
            .map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; read_size];
        f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;

        let mut out = vec![0u8; plane_bytes];
        for y in 0..meta.size_y as usize {
            let src_y = if self.top_down {
                y
            } else {
                meta.size_y as usize - 1 - y
            };
            let src = src_y * stored_row;
            let dst = y * row_bytes;
            if src >= buf.len() {
                continue;
            }
            let available = row_bytes.min(buf.len() - src);
            out[dst..dst + available].copy_from_slice(&buf[src..src + available]);
            if meta.is_rgb && channels >= 3 {
                for px in out[dst..dst + row_bytes].chunks_mut(channels) {
                    px.swap(0, 2);
                }
            }
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
        let meta = self.meta.as_ref().unwrap();
        let spp = meta.size_c as usize;
        let row = meta.size_x as usize * spp;
        let out_row = w as usize * spp;
        let mut out = Vec::with_capacity(h as usize * out_row);
        for r in 0..h as usize {
            let src = &full[(y as usize + r) * row..];
            out.extend_from_slice(&src[x as usize * spp..x as usize * spp + out_row]);
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
        AviWriter {
            path: None,
            meta: None,
            planes: Vec::new(),
            fps: 10,
        }
    }

    /// Set frames per second (default: 10).
    pub fn with_fps(mut self, fps: u32) -> Self {
        self.fps = fps;
        self
    }
}

impl Default for AviWriter {
    fn default() -> Self {
        Self::new()
    }
}

fn write_fourcc(w: &mut impl Write, cc: &[u8; 4]) -> std::io::Result<()> {
    w.write_all(cc)
}
fn write_u32_le(w: &mut impl Write, v: u32) -> std::io::Result<()> {
    w.write_all(&v.to_le_bytes())
}
fn write_u16_le(w: &mut impl Write, v: u16) -> std::io::Result<()> {
    w.write_all(&v.to_le_bytes())
}

impl crate::common::writer::FormatWriter for AviWriter {
    fn is_this_type(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
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
                if is_rgb {
                    let mut row = vec![0u8; row_bytes];
                    if offset < plane.len() {
                        row[..end - offset].copy_from_slice(&plane[offset..end]);
                    }
                    for px in row.chunks_mut(3) {
                        px.swap(0, 2);
                    }
                    w.write_all(&row).map_err(BioFormatsError::Io)?;
                } else if offset < plane.len() {
                    w.write_all(&plane[offset..end])
                        .map_err(BioFormatsError::Io)?;
                    if end - offset < row_bytes {
                        w.write_all(&vec![0u8; row_bytes - (end - offset)])
                            .map_err(BioFormatsError::Io)?;
                    }
                } else {
                    w.write_all(&vec![0u8; row_bytes])
                        .map_err(BioFormatsError::Io)?;
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

    fn can_do_stacks(&self) -> bool {
        true
    }
}
