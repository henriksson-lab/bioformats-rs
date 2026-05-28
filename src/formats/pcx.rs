//! PCX format reader.
//!
//! PCX is a raster image format originally developed for PC Paintbrush, also
//! used by Zeiss' LSM Image Browser. Ported from the upstream Java PCXReader:
//! channels are stored planar (channel-separated, not interleaved) and a
//! version-5 256-entry palette/LUT is read for single-plane images.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, LookupTable};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;

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

fn load_pcx(path: &Path) -> Result<(ImageMetadata, Vec<u8>)> {
    let mut f = File::open(path).map_err(BioFormatsError::Io)?;
    let file_len = f.metadata().map_err(BioFormatsError::Io)?.len();

    // Header (little-endian). Matches the Java initFile layout exactly.
    let mut header = [0u8; 128];
    f.read_exact(&mut header).map_err(BioFormatsError::Io)?;

    if header[0] != 0x0A {
        return Err(BioFormatsError::InvalidData(
            "PCX: invalid manufacturer byte".into(),
        ));
    }

    // Java: seek(1); version=read(); skip(1); bitsPerPixel=read();
    let version = header[1] as i32;
    let _bits_per_pixel = header[3];
    let read_i16 =
        |off: usize| -> i32 { i16::from_le_bytes([header[off], header[off + 1]]) as i32 };
    let x_min = read_i16(4);
    let y_min = read_i16(6);
    let x_max = read_i16(8);
    let y_max = read_i16(10);

    // Java uses xMax - xMin (no +1).
    let size_x = (x_max - x_min).max(0) as u32;
    let size_y = (y_max - y_min).max(0) as u32;

    // After reading the 4 shorts the file pointer is at offset 12.
    // Java then skips (version == 5 ? 53 : 51) bytes, putting nColorPlanes at
    // offset 65 (v5) or 63. We read directly from the header buffer.
    let n_color_planes = if version == 5 {
        header[65] as usize
    } else {
        header[63] as usize
    };
    let (bpl_off, pal_off) = if version == 5 { (66, 68) } else { (64, 66) };
    let bytes_per_line = u16::from_le_bytes([header[bpl_off], header[bpl_off + 1]]) as usize;
    let palette_type = u16::from_le_bytes([header[pal_off], header[pal_off + 1]]);

    // offset = filePointer + 58, where filePointer is just past paletteType.
    let pixel_offset = (pal_off as u64 + 2) + 58;

    if n_color_planes == 0 || bytes_per_line == 0 || size_x == 0 || size_y == 0 {
        return Err(BioFormatsError::InvalidData(
            "PCX: invalid dimensions / plane count".into(),
        ));
    }

    // Read the version-5 256-entry palette (located at end of file) for
    // single-plane images, marking the image as indexed.
    let mut lookup_table = None;
    let mut is_indexed = false;
    if version == 5 && n_color_planes == 1 && file_len >= 768 {
        f.seek(SeekFrom::Start(file_len - 768))
            .map_err(BioFormatsError::Io)?;
        let mut pal = [0u8; 768];
        f.read_exact(&mut pal).map_err(BioFormatsError::Io)?;
        let mut red = vec![0u16; 256];
        let mut green = vec![0u16; 256];
        let mut blue = vec![0u16; 256];
        // Stored R,G,B per entry.
        for i in 0..256 {
            red[i] = pal[i * 3] as u16;
            green[i] = pal[i * 3 + 1] as u16;
            blue[i] = pal[i * 3 + 2] as u16;
        }
        lookup_table = Some(LookupTable { red, green, blue });
        is_indexed = true;
    }

    // Decode the RLE-compressed pixel stream into a planar buffer of size
    // bytesPerLine * sizeY * nColorPlanes.
    f.seek(SeekFrom::Start(pixel_offset))
        .map_err(BioFormatsError::Io)?;
    let total = bytes_per_line
        .checked_mul(size_y as usize)
        .and_then(|n| n.checked_mul(n_color_planes))
        .ok_or_else(|| BioFormatsError::InvalidData("PCX: image byte count overflows".into()))?;

    let mut b = vec![0u8; total];
    let mut reader = std::io::BufReader::new(f);
    let mut pt = 0usize;
    let mut byte = [0u8; 1];
    while pt < total {
        reader.read_exact(&mut byte).map_err(BioFormatsError::Io)?;
        let val = byte[0] as i32;
        if ((val & 0xc0) >> 6) == 3 {
            let len = (val & 0x3f) as usize;
            reader.read_exact(&mut byte).map_err(BioFormatsError::Io)?;
            let runval = byte[0];
            for _ in 0..len {
                if pt >= total {
                    break;
                }
                b[pt] = runval;
                pt += 1;
                // A run never crosses a scan-line boundary.
                if pt % bytes_per_line == 0 {
                    break;
                }
            }
        } else {
            b[pt] = val as u8;
            pt += 1;
        }
    }

    // Build the channel-separated (planar) output: c*w*h + row*w.
    let width_usize = size_x as usize;
    let height_usize = size_y as usize;
    let plane_len = width_usize * height_usize * n_color_planes;
    let mut pixels = vec![0u8; plane_len];
    for row in 0..height_usize {
        let mut row_offset = row * n_color_planes * bytes_per_line;
        for c in 0..n_color_planes {
            let src = row_offset;
            let dst = c * width_usize * height_usize + row * width_usize;
            pixels[dst..dst + width_usize].copy_from_slice(&b[src..src + width_usize]);
            row_offset += bytes_per_line;
        }
    }

    let is_rgb = n_color_planes > 1;
    let mut series_metadata = HashMap::new();
    series_metadata.insert(
        "Palette type".to_string(),
        crate::common::metadata::MetadataValue::Int(palette_type as i64),
    );

    let meta = ImageMetadata {
        size_x,
        size_y,
        size_z: 1,
        size_c: n_color_planes as u32,
        size_t: 1,
        pixel_type: PixelType::Uint8,
        bits_per_pixel: 8,
        image_count: 1,
        dimension_order: DimensionOrder::XYCZT,
        is_rgb,
        is_interleaved: false,
        is_indexed,
        is_little_endian: true,
        resolution_count: 1,
        series_metadata,
        lookup_table,
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
        let meta = self.meta.as_ref().unwrap();
        // PCX data is planar (channel-separated): c*W*H + row*W. Crop each
        // channel plane independently and keep the planar layout.
        let sw = meta.size_x as usize;
        let sh = meta.size_y as usize;
        let channels = meta.size_c as usize;
        let (x, y, w, h) = (x as usize, y as usize, w as usize, h as usize);
        if x.checked_add(w).is_none_or(|end| end > sw)
            || y.checked_add(h).is_none_or(|end| end > sh)
        {
            return Err(BioFormatsError::InvalidData(
                "PCX: requested region out of bounds".into(),
            ));
        }
        let mut out = vec![0u8; channels * w * h];
        for c in 0..channels {
            for row in 0..h {
                let src = c * sw * sh + (y + row) * sw + x;
                let dst = c * w * h + row * w;
                out[dst..dst + w].copy_from_slice(&full[src..src + w]);
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
}
