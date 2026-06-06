//! Adobe Photoshop PSD/PSB format reader.
//!
//! Supports PSD (version 1) and PSB Large Document (version 2) files.
//! Returns the merged composite image data.

use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata};
use crate::common::ome_metadata::OmeMetadata;
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::validate_region;

pub struct PsdReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    /// Decoded composite pixel data, stored **planar** (channel-separated):
    /// all of channel 0's rows, then channel 1's, etc. — matching the on-disk
    /// PSD layout and Java Bio-Formats' channel-separated `openBytes` output.
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

/// A minimal big-endian cursor over an in-memory buffer that mirrors the
/// `RandomAccessInputStream` operations used by the Java `PSDReader`. Reads
/// past end-of-buffer clamp the pointer rather than erroring, matching how the
/// Java offset-finding heuristic tolerates short reads.
struct Cur<'a> {
    d: &'a [u8],
    p: usize,
}

impl<'a> Cur<'a> {
    fn new(d: &'a [u8]) -> Self {
        Cur { d, p: 0 }
    }
    fn fp(&self) -> usize {
        self.p
    }
    fn len(&self) -> usize {
        self.d.len()
    }
    fn seek(&mut self, p: usize) {
        self.p = p.min(self.d.len());
    }
    fn skip(&mut self, n: usize) {
        self.p = self.p.saturating_add(n).min(self.d.len());
    }
    fn read_u8(&mut self) -> u8 {
        let v = self.d.get(self.p).copied().unwrap_or(0);
        if self.p < self.d.len() {
            self.p += 1;
        }
        v
    }
    fn read_u16(&mut self) -> u16 {
        let v = if self.p + 2 <= self.d.len() {
            u16::from_be_bytes([self.d[self.p], self.d[self.p + 1]])
        } else {
            0
        };
        self.skip(2);
        v
    }
    fn read_i16(&mut self) -> i16 {
        self.read_u16() as i16
    }
    fn read_u32(&mut self) -> u32 {
        let v = if self.p + 4 <= self.d.len() {
            u32::from_be_bytes([
                self.d[self.p],
                self.d[self.p + 1],
                self.d[self.p + 2],
                self.d[self.p + 3],
            ])
        } else {
            0
        };
        self.skip(4);
        v
    }
    fn read_i32(&mut self) -> i32 {
        self.read_u32() as i32
    }
    fn read_bytes(&mut self, n: usize) -> &'a [u8] {
        let end = (self.p + n).min(self.d.len());
        let s = &self.d[self.p..end];
        self.p = end;
        s
    }
}

fn load_psd(path: &Path) -> Result<(ImageMetadata, Vec<u8>)> {
    let mut f = File::open(path).map_err(BioFormatsError::Io)?;
    let mut data = Vec::new();
    f.read_to_end(&mut data).map_err(BioFormatsError::Io)?;
    let mut r = Cur::new(&data);

    // Check magic
    if r.read_bytes(4) != b"8BPS" {
        return Err(BioFormatsError::Format("Not a PSD file".into()));
    }

    let version = r.read_u16();
    if !matches!(version, 1 | 2) {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "PSD unsupported version {version}"
        )));
    }
    let psb = version == 2;

    // Skip reserved 6 bytes
    r.skip(6);

    let channels = r.read_u16() as u32;
    let height = r.read_u32();
    let width = r.read_u32();
    let depth = r.read_u16();
    let color_mode = r.read_u16();
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

    let _ = psb; // Java's PSDReader uses 4-byte lengths regardless of version.

    // Color Mode Data section. For palette images (mode 2) this holds a
    // 768-byte (3 x 256) RGB lookup table, stored plane-by-plane.
    let mode_data_len = r.read_i32() as i64;
    let fp = r.fp();
    let mut lookup_table = None;
    if mode_data_len != 0 {
        if color_mode == 2 {
            let lut = r.read_bytes(768);
            if lut.len() == 768 {
                let mut red = vec![0u16; 256];
                let mut green = vec![0u16; 256];
                let mut blue = vec![0u16; 256];
                for i in 0..256 {
                    red[i] = lut[i] as u16;
                    green[i] = lut[256 + i] as u16;
                    blue[i] = lut[512 + i] as u16;
                }
                lookup_table = Some(crate::common::metadata::LookupTable { red, green, blue });
            }
        }
        r.seek((fp as i64 + mode_data_len).max(0) as usize);
    }

    // Image Resources section: Java skips the 4-byte length, then walks "8BIM"
    // resource blocks one at a time.
    r.skip(4);
    while r.read_bytes(4) == b"8BIM" {
        let _tag = r.read_i16();
        let mut read = 1;
        while r.read_u8() != 0 {
            read += 1;
        }
        if read % 2 == 1 {
            r.skip(1);
        }
        let mut size = r.read_i32();
        if size % 2 == 1 {
            size += 1;
        }
        r.skip(size.max(0) as usize);
    }
    r.seek(r.fp().saturating_sub(4));

    // Layer and Mask Info section. Java derives the image-data offset through a
    // sequence of heuristics; we mirror them byte-for-byte so the resulting
    // (sometimes slightly misaligned) offset matches the Java reference output.
    let block_len = r.read_i32();
    // Start of the layer+mask block (just past the 4-byte length). The simple
    // fallback offset for the image-data section is `block_start + block_len`.
    let block_start = r.fp();
    let offset;
    if block_len == 0 {
        offset = r.fp();
    } else {
        let layer_len = r.read_i32();
        let layer_count = r.read_i16();
        if layer_count < 0 {
            // Vector/large-document layer data: Java rejects this, but we still
            // expose the flattened composite image. Skip the whole layer+mask
            // block and read the image-data section that follows it.
            r.seek(block_start.saturating_add(block_len.max(0) as usize));
            offset = r.fp();
            return finish_psd(
                &data, &mut r, offset, channels, height, width, depth, color_mode,
                pixel_type, lookup_table,
            );
        }
        if layer_len == 0 && layer_count == 0 {
            r.skip(2);
            let check = r.read_i16();
            r.seek(r.fp().saturating_sub(if check == 0 { 4 } else { 2 }));
        }

        let lc = layer_count as usize;
        let mut lw = vec![0i32; lc];
        let mut lh = vec![0i32; lc];
        let mut lcc = vec![0i32; lc];
        for i in 0..lc {
            let top = r.read_i32();
            let left = r.read_i32();
            let bottom = r.read_i32();
            let right = r.read_i32();
            lw[i] = right - left;
            lh[i] = bottom - top;
            lcc[i] = r.read_i16() as i32;
            r.skip((lcc[i] * 6 + 12).max(0) as usize);
            let mut len = r.read_i32();
            if len % 2 == 1 {
                len += 1;
            }
            r.skip(len.max(0) as usize);
        }
        // Skip over each layer's per-channel pixel data.
        for i in 0..lc {
            if lh[i] < 0 {
                continue;
            }
            for _cc in 0..lcc[i] {
                let compressed = r.read_i16() == 1;
                if !compressed {
                    r.skip((lw[i] as i64 * lh[i] as i64).max(0) as usize);
                } else {
                    let mut lens = vec![0usize; lh[i] as usize];
                    for y in 0..lh[i] as usize {
                        lens[y] = r.read_u16() as usize;
                    }
                    for y in 0..lh[i] as usize {
                        r.skip(lens[y]);
                    }
                }
            }
        }
        let start = r.fp();
        while r.read_u8() != b'8' && r.fp() < r.len() {}
        r.skip(7);
        if r.fp() - start > 1024 {
            r.seek(start);
        }
        let mut len = r.read_i32();
        if len % 4 != 0 {
            len += 4 - (len % 4);
        }
        if (len as i64) > (r.len() as i64 - r.fp() as i64) || (len & 0xff_0000) >> 16 == 1 {
            r.seek(start);
            len = 0;
        }
        r.skip(len.max(0) as usize);

        let mut s = r.read_bytes(4).to_vec();
        while s == b"8BIM" {
            r.skip(4);
            let mut len = r.read_i32();
            if len % 4 != 0 {
                len += 4 - (len % 4);
            }
            r.skip(len.max(0) as usize);
            s = r.read_bytes(4).to_vec();
        }
        offset = r.fp().saturating_sub(4);
    }

    finish_psd(
        &data, &mut r, offset, channels, height, width, depth, color_mode, pixel_type,
        lookup_table,
    )
}

/// Decode the PSD image-data section starting at `offset` and assemble metadata.
/// `offset` points at the compression word (Java's pre-read position).
#[allow(clippy::too_many_arguments)]
fn finish_psd(
    data: &[u8],
    r: &mut Cur,
    mut offset: usize,
    channels: u32,
    height: u32,
    width: u32,
    depth: u16,
    color_mode: u16,
    pixel_type: PixelType,
    lookup_table: Option<crate::common::metadata::LookupTable>,
) -> Result<(ImageMetadata, Vec<u8>)> {
    // Image Data section. Java reads the compression word at `offset`, then sets
    // `offset = filePointer` (just past the word).
    r.seek(offset);
    let compression = r.read_u16();
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

    let compressed = compression == 1;
    // Java stores per-(channel,row) RLE byte counts read immediately after the
    // compression word; `offset` then points at the first compressed byte.
    let mut row_counts: Vec<usize> = Vec::new();
    if compressed {
        for _ in 0..(channels as usize * height as usize) {
            row_counts.push(r.read_u16() as usize);
        }
    }
    offset = r.fp();

    let pixel_data: Vec<u8> = if compressed {
        // RLE: decode each row from `offset` using its byte count. Java decodes
        // exactly `lens[c][row]` bytes per row into a `sizeX*bpp` output row.
        let mut out = Vec::with_capacity(total_bytes);
        let mut pos = offset;
        for &rc in &row_counts {
            let end = (pos + rc).min(data.len());
            let src = &data[pos.min(data.len())..end];
            let decoded = decode_packbits(src, row_bytes).unwrap_or_else(|_| {
                let mut v = src.to_vec();
                v.resize(row_bytes, 0);
                v
            });
            out.extend_from_slice(&decoded);
            pos += rc;
        }
        out
    } else if compression == 0 {
        // Raw planar data starting at `offset`. Like Java's readPlane, require
        // the full plane to be present rather than zero-padding a truncated one.
        let end = offset.checked_add(total_bytes).unwrap_or(usize::MAX);
        if end > data.len() {
            return Err(BioFormatsError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "failed to fill whole buffer",
            )));
        }
        data[offset..end].to_vec()
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

    // Keep the composite data **planar** (channel-separated): channel 0's plane,
    // then channel 1's, etc. Java's PSDReader is interleaved=false and emits
    // channels separately, so storing planar lets the region crop mirror Java's
    // byte layout exactly. Normalize to the full expected size (pad/truncate).
    let mut pixels = pixel_data;
    pixels.resize(total_bytes, 0u8);

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
        is_interleaved: false,
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
        if plane_index != 0 {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let full = self.pixels.as_ref().ok_or(BioFormatsError::NotInitialized)?;

        validate_region("PSD", meta.size_x, meta.size_y, x, y, w, h)?;

        let bps = meta.pixel_type.bytes_per_sample();
        let channels = meta.size_c as usize;
        let row_bytes = (meta.size_x as usize)
            .checked_mul(bps)
            .ok_or_else(|| BioFormatsError::Format("PSD row size overflows".into()))?;
        let plane_bytes = row_bytes
            .checked_mul(meta.size_y as usize)
            .ok_or_else(|| BioFormatsError::Format("PSD plane size overflows".into()))?;
        let out_row = (w as usize)
            .checked_mul(bps)
            .ok_or_else(|| BioFormatsError::Format("PSD output row size overflows".into()))?;

        // Channel-separated (planar) output, matching Java's openBytes layout:
        // for each channel, copy its cropped region rows, then the next channel.
        let mut out = Vec::with_capacity(channels * (h as usize) * out_row);
        let start_x = (x as usize) * bps;
        for c in 0..channels {
            let chan_base = c * plane_bytes;
            for row in 0..h as usize {
                let src = chan_base + (y as usize + row) * row_bytes + start_x;
                out.extend_from_slice(&full[src..src + out_row]);
            }
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

    fn ome_metadata(&self) -> Option<OmeMetadata> {
        let meta = self.meta.as_ref()?;
        let mut ome = OmeMetadata::from_image_metadata(meta);
        // Java sets the image name to the source file's basename.
        if let (Some(path), Some(image)) = (self.path.as_ref(), ome.images.first_mut()) {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                image.name = Some(name.to_string());
            }
        }
        Some(ome)
    }
}
