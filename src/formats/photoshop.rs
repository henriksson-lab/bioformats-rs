//! Adobe Photoshop PSD/PSB format reader.
//!
//! Supports PSD (version 1) and PSB Large Document (version 2) files.
//! Returns the merged composite image data.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;

pub struct PsdReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    pixels: Option<Vec<u8>>,
}

impl PsdReader {
    pub fn new() -> Self {
        PsdReader { path: None, meta: None, pixels: None }
    }
}

impl Default for PsdReader {
    fn default() -> Self { Self::new() }
}

fn read_u16_be(r: &mut impl Read) -> std::io::Result<u16> {
    let mut b = [0u8; 2];
    r.read_exact(&mut b)?;
    Ok(u16::from_be_bytes(b))
}

fn read_u32_be(r: &mut impl Read) -> std::io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_be_bytes(b))
}

fn read_u64_be(r: &mut impl Read) -> std::io::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_be_bytes(b))
}

/// Decode PackBits RLE-encoded data.
fn decode_packbits(src: &[u8], expected_bytes: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(expected_bytes);
    let mut i = 0;
    while i < src.len() && out.len() < expected_bytes {
        let n = src[i] as i8;
        i += 1;
        if n >= 0 {
            // Copy next n+1 bytes literally
            let count = (n as usize) + 1;
            let end = (i + count).min(src.len());
            out.extend_from_slice(&src[i..end]);
            i += count;
        } else if n != -128 {
            // Repeat next byte (-n+1) times
            let count = ((-n) as usize) + 1;
            if i < src.len() {
                let val = src[i];
                i += 1;
                for _ in 0..count {
                    out.push(val);
                }
            }
        }
        // n == -128: no-op
    }
    out
}

fn pixel_type_from_depth(depth: u16) -> PixelType {
    match depth {
        8 => PixelType::Uint8,
        16 => PixelType::Uint16,
        32 => PixelType::Float32,
        _ => PixelType::Uint8,
    }
}

fn load_psd(path: &Path) -> Result<(ImageMetadata, Vec<u8>)> {
    let f = File::open(path).map_err(BioFormatsError::Io)?;
    let mut r = BufReader::new(f);

    // Check magic
    let mut magic = [0u8; 4];
    r.read_exact(&mut magic).map_err(BioFormatsError::Io)?;
    if &magic != b"8BPS" {
        return Err(BioFormatsError::Format("Not a PSD file".into()));
    }

    let version = read_u16_be(&mut r).map_err(BioFormatsError::Io)?;
    let psb = version == 2;

    // Skip reserved 6 bytes
    let mut reserved = [0u8; 6];
    r.read_exact(&mut reserved).map_err(BioFormatsError::Io)?;

    let channels = read_u16_be(&mut r).map_err(BioFormatsError::Io)? as u32;
    let height = read_u32_be(&mut r).map_err(BioFormatsError::Io)?;
    let width = read_u32_be(&mut r).map_err(BioFormatsError::Io)?;
    let depth = read_u16_be(&mut r).map_err(BioFormatsError::Io)?;
    let color_mode = read_u16_be(&mut r).map_err(BioFormatsError::Io)?;

    // Skip Color Mode Data section
    let cm_len = read_u32_be(&mut r).map_err(BioFormatsError::Io)? as u64;
    r.seek(SeekFrom::Current(cm_len as i64)).map_err(BioFormatsError::Io)?;

    // Skip Image Resources section
    let ir_len = read_u32_be(&mut r).map_err(BioFormatsError::Io)? as u64;
    r.seek(SeekFrom::Current(ir_len as i64)).map_err(BioFormatsError::Io)?;

    // Skip Layer and Mask Info section
    let lm_len: u64 = if psb {
        read_u64_be(&mut r).map_err(BioFormatsError::Io)?
    } else {
        read_u32_be(&mut r).map_err(BioFormatsError::Io)? as u64
    };
    r.seek(SeekFrom::Current(lm_len as i64)).map_err(BioFormatsError::Io)?;

    // Image Data section
    let compression = read_u16_be(&mut r).map_err(BioFormatsError::Io)?;

    let bytes_per_sample = (depth as usize + 7) / 8;
    let row_bytes = width as usize * bytes_per_sample;
    let plane_bytes = row_bytes * height as usize;
    let total_bytes = plane_bytes * channels as usize;

    let pixel_data: Vec<u8> = if compression == 1 {
        // RLE: byte count table followed by compressed data
        let count_entries = (height * channels) as usize;
        let mut row_counts = Vec::with_capacity(count_entries);
        for _ in 0..count_entries {
            if psb {
                let c = {
                    let mut b = [0u8; 4];
                    r.read_exact(&mut b).map_err(BioFormatsError::Io)?;
                    u32::from_be_bytes(b) as usize
                };
                row_counts.push(c);
            } else {
                row_counts.push(read_u16_be(&mut r).map_err(BioFormatsError::Io)? as usize);
            }
        }
        let total_compressed: usize = row_counts.iter().sum();
        let mut compressed = vec![0u8; total_compressed];
        r.read_exact(&mut compressed).map_err(BioFormatsError::Io)?;

        // Decode each row
        let mut out = Vec::with_capacity(total_bytes);
        let mut offset = 0;
        for &rc in &row_counts {
            let decoded = decode_packbits(&compressed[offset..offset + rc], row_bytes);
            let decoded_len = decoded.len();
            out.extend_from_slice(&decoded);
            // Pad if short
            if decoded_len < row_bytes {
                out.resize(out.len() + (row_bytes - decoded_len), 0);
            }
            offset += rc;
        }
        out
    } else {
        // Raw
        let mut raw = vec![0u8; total_bytes];
        r.read_exact(&mut raw).map_err(BioFormatsError::Io)?;
        raw
    };

    // Convert from planar to interleaved
    let is_rgb = color_mode == 3;
    let output_channels = if is_rgb { 3usize } else { channels as usize };
    let pixels = if is_rgb && channels >= 3 {
        let mut interleaved = Vec::with_capacity(width as usize * height as usize * 3 * bytes_per_sample);
        for i in 0..(width as usize * height as usize) {
            for c in 0..3usize {
                let src_off = c * plane_bytes + i * bytes_per_sample;
                interleaved.extend_from_slice(&pixel_data[src_off..src_off + bytes_per_sample]);
            }
        }
        interleaved
    } else {
        // Grayscale or other: return first channel
        pixel_data[..plane_bytes].to_vec()
    };

    let pixel_type = pixel_type_from_depth(depth);

    let meta = ImageMetadata {
        size_x: width,
        size_y: height,
        size_z: 1,
        size_c: output_channels as u32,
        size_t: 1,
        pixel_type,
        bits_per_pixel: depth as u8,
        image_count: 1,
        dimension_order: DimensionOrder::XYCZT,
        is_rgb,
        is_interleaved: is_rgb,
        is_indexed: false,
        is_little_endian: false, // PSD is big-endian
        resolution_count: 1,
        series_metadata: HashMap::new(),
        lookup_table: None,
        modulo_z: None,
        modulo_c: None,
        modulo_t: None,
    };

    Ok((meta, pixels))
}

impl FormatReader for PsdReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension().and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("psd") | Some("psb"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.starts_with(b"8BPS")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let (meta, pixels) = load_psd(path)?;
        self.path = Some(path.to_path_buf());
        self.meta = Some(meta);
        self.pixels = Some(pixels);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.pixels = None;
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
        if plane_index != 0 { return Err(BioFormatsError::PlaneOutOfRange(plane_index)); }
        self.pixels.clone().ok_or(BioFormatsError::NotInitialized)
    }

    fn open_bytes_region(&mut self, plane_index: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let full = self.open_bytes(plane_index)?;
        let meta = self.meta.as_ref().unwrap();
        let bps = meta.pixel_type.bytes_per_sample();
        let channels = meta.size_c as usize;
        let row_bytes = meta.size_x as usize * channels * bps;
        let out_row = w as usize * channels * bps;
        let mut out = Vec::with_capacity(h as usize * out_row);
        for row in 0..h as usize {
            let src = &full[(y as usize + row) * row_bytes..];
            let s = x as usize * channels * bps;
            out.extend_from_slice(&src[s..s + out_row]);
        }
        Ok(out)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}
