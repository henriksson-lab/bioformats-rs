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
use crate::common::region::crop_full_plane;

pub struct PsdReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    pixels: Option<Vec<u8>>,
}

impl PsdReader {
    pub fn new() -> Self {
        PsdReader {
            path: None,
            meta: None,
            pixels: None,
        }
    }
}

impl Default for PsdReader {
    fn default() -> Self {
        Self::new()
    }
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
fn decode_packbits(src: &[u8], expected_bytes: usize) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(expected_bytes);
    let mut i = 0;
    while i < src.len() && out.len() < expected_bytes {
        let n = src[i] as i8;
        i += 1;
        if n >= 0 {
            // Copy next n+1 bytes literally
            let count = (n as usize) + 1;
            let end = i.checked_add(count).ok_or_else(|| {
                BioFormatsError::InvalidData("PSD PackBits row count overflow".into())
            })?;
            if end > src.len() {
                return Err(BioFormatsError::InvalidData(
                    "PSD PackBits row is truncated".into(),
                ));
            }
            out.extend_from_slice(&src[i..end]);
            i += count;
        } else if n != -128 {
            // Repeat next byte (-n+1) times
            let count = ((-n) as usize) + 1;
            if i >= src.len() {
                return Err(BioFormatsError::InvalidData(
                    "PSD PackBits row is truncated".into(),
                ));
            }
            let val = src[i];
            i += 1;
            for _ in 0..count {
                out.push(val);
            }
        }
        // n == -128: no-op
    }
    if out.len() < expected_bytes {
        return Err(BioFormatsError::InvalidData(
            "PSD PackBits row is shorter than expected".into(),
        ));
    }
    out.truncate(expected_bytes);
    Ok(out)
}

fn pixel_type_from_depth(depth: u16) -> Result<PixelType> {
    match depth {
        8 => Ok(PixelType::Uint8),
        16 => Ok(PixelType::Uint16),
        32 => Ok(PixelType::Float32),
        _ => Err(BioFormatsError::UnsupportedFormat(format!(
            "PSD unsupported bit depth {depth}"
        ))),
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
    if !matches!(version, 1 | 2) {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "PSD unsupported version {version}"
        )));
    }
    let psb = version == 2;

    // Skip reserved 6 bytes
    let mut reserved = [0u8; 6];
    r.read_exact(&mut reserved).map_err(BioFormatsError::Io)?;

    let channels = read_u16_be(&mut r).map_err(BioFormatsError::Io)? as u32;
    let height = read_u32_be(&mut r).map_err(BioFormatsError::Io)?;
    let width = read_u32_be(&mut r).map_err(BioFormatsError::Io)?;
    let depth = read_u16_be(&mut r).map_err(BioFormatsError::Io)?;
    let color_mode = read_u16_be(&mut r).map_err(BioFormatsError::Io)?;
    if channels == 0 {
        return Err(BioFormatsError::UnsupportedFormat(
            "PSD channel count is non-positive".into(),
        ));
    }
    if matches!(color_mode, 3 | 4) && channels < 3 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "PSD RGB/CMYK channel count is too small ({channels})"
        )));
    }
    if width == 0 || height == 0 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "PSD dimensions are non-positive ({width}x{height})"
        )));
    }
    let pixel_type = pixel_type_from_depth(depth)?;

    // Color Mode Data section. For palette images (mode 2) this holds a
    // 768-byte (3 x 256) RGB lookup table, stored plane-by-plane.
    let cm_len = read_u32_be(&mut r).map_err(BioFormatsError::Io)? as u64;
    let mut lookup_table = None;
    if cm_len != 0 {
        if color_mode == 2 && cm_len >= 768 {
            let mut lut = [0u8; 768];
            r.read_exact(&mut lut).map_err(BioFormatsError::Io)?;
            let mut red = vec![0u16; 256];
            let mut green = vec![0u16; 256];
            let mut blue = vec![0u16; 256];
            for i in 0..256 {
                red[i] = lut[i] as u16;
                green[i] = lut[256 + i] as u16;
                blue[i] = lut[512 + i] as u16;
            }
            lookup_table = Some(crate::common::metadata::LookupTable { red, green, blue });
            // Seek past any remaining color-mode data.
            if cm_len > 768 {
                r.seek(SeekFrom::Current((cm_len - 768) as i64))
                    .map_err(BioFormatsError::Io)?;
            }
        } else {
            r.seek(SeekFrom::Current(cm_len as i64))
                .map_err(BioFormatsError::Io)?;
        }
    }

    // Skip Image Resources section
    let ir_len = read_u32_be(&mut r).map_err(BioFormatsError::Io)? as u64;
    r.seek(SeekFrom::Current(ir_len as i64))
        .map_err(BioFormatsError::Io)?;

    // Skip Layer and Mask Info section
    let lm_len: u64 = if psb {
        read_u64_be(&mut r).map_err(BioFormatsError::Io)?
    } else {
        read_u32_be(&mut r).map_err(BioFormatsError::Io)? as u64
    };
    r.seek(SeekFrom::Current(lm_len as i64))
        .map_err(BioFormatsError::Io)?;

    // Image Data section
    let compression = read_u16_be(&mut r).map_err(BioFormatsError::Io)?;

    let bytes_per_sample = pixel_type.bytes_per_sample();
    let row_bytes = (width as usize)
        .checked_mul(bytes_per_sample)
        .ok_or_else(|| BioFormatsError::Format("PSD row byte count overflows".into()))?;
    let plane_bytes = row_bytes
        .checked_mul(height as usize)
        .ok_or_else(|| BioFormatsError::Format("PSD plane byte count overflows".into()))?;
    let total_bytes = plane_bytes
        .checked_mul(channels as usize)
        .ok_or_else(|| BioFormatsError::Format("PSD pixel byte count overflows".into()))?;

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
            let decoded = decode_packbits(&compressed[offset..offset + rc], row_bytes)?;
            out.extend_from_slice(&decoded);
            offset += rc;
        }
        out
    } else if compression == 0 {
        // Raw
        let mut raw = vec![0u8; total_bytes];
        r.read_exact(&mut raw).map_err(BioFormatsError::Io)?;
        raw
    } else {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "PSD unsupported compression {compression}"
        )));
    };

    // Color-mode semantics per the Java PSDReader:
    //   RGB(3) / CMYK(4) -> rgb = true
    //   palette(2)       -> indexed
    // sizeC keeps the full channel count (RGB+alpha=4, CMYK=4, Lab=3, ...).
    let is_rgb = color_mode == 3 || color_mode == 4;
    let is_indexed = color_mode == 2 && lookup_table.is_some();
    let output_channels = channels as usize;

    // Convert from planar to interleaved, preserving every channel so that
    // alpha (RGBA), CMYK, and Lab data are not dropped.
    let pixels = if output_channels > 1 {
        let mut interleaved = Vec::with_capacity(
            width as usize * height as usize * output_channels * bytes_per_sample,
        );
        for i in 0..(width as usize * height as usize) {
            for c in 0..output_channels {
                let src_off = c * plane_bytes + i * bytes_per_sample;
                if src_off + bytes_per_sample <= pixel_data.len() {
                    interleaved.extend_from_slice(&pixel_data[src_off..src_off + bytes_per_sample]);
                } else {
                    interleaved.extend(std::iter::repeat(0u8).take(bytes_per_sample));
                }
            }
        }
        interleaved
    } else {
        pixel_data[..plane_bytes.min(pixel_data.len())].to_vec()
    };

    // Java: imageCount = sizeC / (isRGB ? 3 : 1).
    let image_count = (output_channels as u32 / if is_rgb { 3 } else { 1 }).max(1);

    let meta = ImageMetadata {
        size_x: width,
        size_y: height,
        size_z: 1,
        size_c: output_channels as u32,
        size_t: 1,
        pixel_type,
        bits_per_pixel: depth as u8,
        image_count,
        dimension_order: DimensionOrder::XYCZT,
        is_rgb,
        is_interleaved: output_channels > 1,
        is_indexed,
        is_little_endian: false, // PSD is big-endian
        resolution_count: 1,
        series_metadata: HashMap::new(),
        lookup_table,
        modulo_z: None,
        modulo_c: None,
        modulo_t: None,
    };

    Ok((meta, pixels))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packbits_rejects_truncated_literal_payload() {
        let err = decode_packbits(&[2, 10, 11], 3)
            .expect_err("short PackBits literal should be rejected");
        assert!(
            matches!(err, BioFormatsError::InvalidData(message) if message.contains("truncated"))
        );
    }

    #[test]
    fn packbits_rejects_short_decoded_row() {
        let err = decode_packbits(&[0, 10], 2).expect_err("short PackBits row should be rejected");
        assert!(
            matches!(err, BioFormatsError::InvalidData(message) if message.contains("shorter"))
        );
    }
}

impl FormatReader for PsdReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("psd") | Some("psb"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.starts_with(b"8BPS")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
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

    fn series_count(&self) -> usize {
        usize::from(self.meta.is_some())
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.meta.is_none() {
            return Err(BioFormatsError::NotInitialized);
        }
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
        if plane_index != 0 {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        self.pixels.clone().ok_or(BioFormatsError::NotInitialized)
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
        crop_full_plane("PSD", &full, meta, meta.size_c as usize, x, y, w, h)
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
