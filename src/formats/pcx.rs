//! PCX format reader.
//!
//! PCX is a raster image format originally developed for PC Paintbrush.
//! Supports 8-bit grayscale and 24-bit RGB (3 planes × 8bpp).

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;

pub struct PcxReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    pixels: Option<Vec<u8>>,
}

impl PcxReader {
    pub fn new() -> Self {
        PcxReader {
            path: None,
            meta: None,
            pixels: None,
        }
    }
}

impl Default for PcxReader {
    fn default() -> Self {
        Self::new()
    }
}

/// Decode RLE-encoded PCX row data into `out` buffer.
fn decode_rle(src: &mut impl Read, out: &mut Vec<u8>, count: usize) -> std::io::Result<()> {
    let mut written = 0usize;
    let mut buf = [0u8; 1];
    while written < count {
        src.read_exact(&mut buf)?;
        let byte = buf[0];
        if byte >= 0xC0 {
            let run = (byte & 0x3F) as usize;
            src.read_exact(&mut buf)?;
            let val = buf[0];
            let take = run.min(count - written);
            for _ in 0..take {
                out.push(val);
            }
            written += take;
        } else {
            out.push(byte);
            written += 1;
        }
    }
    Ok(())
}

fn load_pcx(path: &Path) -> Result<(ImageMetadata, Vec<u8>)> {
    let f = File::open(path).map_err(BioFormatsError::Io)?;
    let mut r = BufReader::new(f);

    let mut header = [0u8; 128];
    r.read_exact(&mut header).map_err(BioFormatsError::Io)?;

    if header[0] != 0x0A {
        return Err(BioFormatsError::InvalidData(
            "PCX: invalid manufacturer byte".into(),
        ));
    }
    let _version = header[1];
    let encoding = header[2]; // 0=raw, 1=RLE
    let bits_per_pixel = header[3];
    let x_min = u16::from_le_bytes([header[4], header[5]]) as u32;
    let y_min = u16::from_le_bytes([header[6], header[7]]) as u32;
    let x_max = u16::from_le_bytes([header[8], header[9]]) as u32;
    let y_max = u16::from_le_bytes([header[10], header[11]]) as u32;
    let n_planes = header[65] as usize;
    let bytes_per_line = u16::from_le_bytes([header[66], header[67]]) as usize;

    if encoding > 1 {
        return Err(BioFormatsError::InvalidData(format!(
            "PCX: unsupported encoding {encoding}"
        )));
    }
    if bits_per_pixel != 8 {
        return Err(BioFormatsError::InvalidData(format!(
            "PCX: unsupported bits per pixel {bits_per_pixel}; only 8-bit PCX is supported"
        )));
    }
    if x_max < x_min || y_max < y_min {
        return Err(BioFormatsError::InvalidData(
            "PCX: inverted image bounds".into(),
        ));
    }

    let width = x_max - x_min + 1;
    let height = y_max - y_min + 1;
    let width_usize = width as usize;
    let height_usize = height as usize;

    if !matches!(n_planes, 1 | 3) {
        return Err(BioFormatsError::InvalidData(format!(
            "PCX: unsupported plane count {n_planes}; only 1-plane grayscale and 3-plane RGB are supported"
        )));
    }
    if bytes_per_line < width_usize {
        return Err(BioFormatsError::InvalidData(format!(
            "PCX: bytes_per_line {bytes_per_line} is smaller than image width {width}"
        )));
    }

    let is_rgb = n_planes == 3 && bits_per_pixel == 8;
    let channels: usize = if is_rgb { 3 } else { 1 };

    let row_data_len = n_planes.checked_mul(bytes_per_line).ok_or_else(|| {
        BioFormatsError::InvalidData("PCX: decoded row byte count overflows".into())
    })?;
    let plane_len = width_usize
        .checked_mul(height_usize)
        .and_then(|n| n.checked_mul(channels))
        .ok_or_else(|| BioFormatsError::InvalidData("PCX: image byte count overflows".into()))?;

    let mut pixels = Vec::with_capacity(plane_len);

    for _row in 0..height_usize {
        let mut row_data = Vec::with_capacity(row_data_len);
        if encoding == 1 {
            decode_rle(&mut r, &mut row_data, row_data_len).map_err(BioFormatsError::Io)?;
        } else {
            row_data.resize(row_data_len, 0);
            r.read_exact(&mut row_data).map_err(BioFormatsError::Io)?;
        }

        if is_rgb {
            // Planes are stored sequentially: [R bytes][G bytes][B bytes]
            // Convert to interleaved RGB
            let r_plane = &row_data[0..bytes_per_line];
            let g_plane = &row_data[bytes_per_line..2 * bytes_per_line];
            let b_plane = &row_data[2 * bytes_per_line..3 * bytes_per_line];
            for x in 0..width_usize {
                pixels.push(r_plane[x]);
                pixels.push(g_plane[x]);
                pixels.push(b_plane[x]);
            }
        } else {
            // Grayscale: just take first bytes_per_line bytes, trimmed to width
            pixels.extend_from_slice(&row_data[..width_usize]);
        }
    }

    let meta = ImageMetadata {
        size_x: width,
        size_y: height,
        size_z: 1,
        size_c: channels as u32,
        size_t: 1,
        pixel_type: PixelType::Uint8,
        bits_per_pixel: 8,
        image_count: 1,
        dimension_order: DimensionOrder::XYCZT,
        is_rgb,
        is_interleaved: is_rgb,
        is_indexed: false,
        is_little_endian: true,
        resolution_count: 1,
        series_metadata: HashMap::new(),
        lookup_table: None,
        modulo_z: None,
        modulo_c: None,
        modulo_t: None,
    };

    Ok((meta, pixels))
}

impl FormatReader for PcxReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("pcx"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        if header.len() < 2 {
            return false;
        }
        header[0] == 0x0A && matches!(header[1], 0 | 2 | 3 | 5)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let (meta, pixels) = load_pcx(path)?;
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
        let meta = self.meta.as_ref().unwrap();
        let channels = if meta.is_rgb { 3usize } else { 1usize };
        crop_full_plane("PCX", &full, meta, channels, x, y, w, h)
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
