use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, LookupTable};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;

pub struct BmpReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    pixels: Option<Vec<u8>>,
}

// BMP compression types.
const BMP_RAW: u32 = 0;
const BMP_RLE_8: u32 = 1;
const BMP_RLE_4: u32 = 2;
const BMP_BITFIELDS: u32 = 3;

impl BmpReader {
    pub fn new() -> Self {
        BmpReader {
            path: None,
            meta: None,
            pixels: None,
        }
    }
}

impl Default for BmpReader {
    fn default() -> Self {
        Self::new()
    }
}

fn rd_i32(b: &[u8], off: usize) -> i32 {
    i32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}
fn rd_u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}
fn rd_i16(b: &[u8], off: usize) -> i16 {
    i16::from_le_bytes([b[off], b[off + 1]])
}

/// Given a component bit mask, return (shift, max_value). The shift is the
/// number of trailing zero bits; max_value is the mask scaled down by the
/// shift (i.e. the number of distinct values minus one). Returns None for a
/// zero mask.
fn mask_shift_scale(mask: u32) -> Option<(u32, u32)> {
    if mask == 0 {
        return None;
    }
    let shift = mask.trailing_zeros();
    let max = mask >> shift;
    Some((shift, max))
}

/// Extract an 8-bit component value from a packed pixel using the given mask.
/// Mirrors the shift+scale approach: isolate the component bits, then scale the
/// component's value range up to 0..255.
fn extract_component(pixel: u32, mask: u32) -> u8 {
    match mask_shift_scale(mask) {
        Some((shift, max)) if max > 0 => {
            let v = (pixel & mask) >> shift;
            ((v * 255 + max / 2) / max) as u8
        }
        _ => 0,
    }
}

/// Full BMP parser modeled on the upstream Java BMPReader. Returns the
/// metadata plus a single interleaved plane (BGR already swapped to RGB for
/// multichannel images).
fn load_bmp(path: &Path) -> Result<(ImageMetadata, Vec<u8>)> {
    let mut f = File::open(path).map_err(BioFormatsError::Io)?;
    let mut data = Vec::new();
    f.read_to_end(&mut data).map_err(BioFormatsError::Io)?;
    if data.len() < 54 || &data[0..2] != b"BM" {
        return Err(BioFormatsError::InvalidData("BMP: bad magic".into()));
    }

    // First header (14 bytes): magic(2), fileSize(4), reserved(4), offset(4).
    let global_i32 = rd_i32(&data, 10); // offset to pixel data
    if global_i32 < 0 {
        return Err(BioFormatsError::InvalidData(
            "BMP: negative pixel data offset".into(),
        ));
    }
    let global = global_i32 as usize;
    if global > data.len() {
        return Err(BioFormatsError::InvalidData(
            "BMP: pixel data offset exceeds file length".into(),
        ));
    }

    // Second header (40-byte BITMAPINFOHEADER): headerSize(4) then dims.
    let mut size_x = rd_i32(&data, 18);
    let mut size_y = rd_i32(&data, 22);
    let mut invert_y = false;
    if size_x < 1 {
        size_x = size_x.abs();
    }
    if size_y < 1 {
        size_y = size_y.abs();
        invert_y = true;
    }
    let size_x = size_x as u32;
    let size_y = size_y as u32;

    let _color_planes = rd_i16(&data, 26);
    let bpp_total = rd_i16(&data, 28) as i32; // bits per pixel (all channels)
    let mut bpp = bpp_total;
    let compression = rd_i32(&data, 30) as u32;
    let pixel_size_x = rd_i32(&data, 38);
    let pixel_size_y = rd_i32(&data, 42);
    let mut n_colors = rd_i32(&data, 46);

    if n_colors == 0 && bpp != 32 && bpp != 24 {
        n_colors = if bpp < 8 { 1 << bpp } else { 256 };
    }

    // BITFIELDS (compression 3): per-channel bit masks follow the 40-byte
    // BITMAPINFOHEADER (file offset 54): red(4), green(4), blue(4) and, for
    // 32-bit V4+ headers, an optional alpha(4). When no masks are present we
    // fall back to the standard 5-6-5 (16-bit) / 8-8-8-8 (32-bit) defaults.
    let mut bitfields: Option<(u32, u32, u32, u32)> = None;
    if compression == BMP_BITFIELDS {
        let mut red = if data.len() >= 58 {
            rd_u32(&data, 54)
        } else {
            0
        };
        let mut green = if data.len() >= 62 {
            rd_u32(&data, 58)
        } else {
            0
        };
        let mut blue = if data.len() >= 66 {
            rd_u32(&data, 62)
        } else {
            0
        };
        // Alpha mask: present in BITMAPV4HEADER+. Only trust it for 32-bit and
        // when it sits before the pixel data offset.
        let mut alpha = if bpp_total == 32 && data.len() >= 70 && global >= 70 {
            rd_u32(&data, 66)
        } else {
            0
        };
        if red == 0 && green == 0 && blue == 0 {
            // No explicit masks: use the standard 5-6-5 default for 16-bit.
            if bpp_total == 16 {
                red = 0xF800; // 5 bits
                green = 0x07E0; // 6 bits
                blue = 0x001F; // 5 bits
                alpha = 0;
            } else {
                red = 0x00FF0000;
                green = 0x0000FF00;
                blue = 0x000000FF;
                alpha = 0xFF000000;
            }
        }
        bitfields = Some((red, green, blue, alpha));
    }

    // Palette begins after the 14+40 = 54-byte header.
    let mut palette_pos = 54usize;
    let mut palette: Option<[[u8; 256]; 3]> = None;
    if n_colors != 0 && bpp == 8 {
        // palette[j][i]; j from len-1 down (so stored B,G,R -> we read into
        // index 2,1,0), then skip 1 reserved byte per entry.
        let mut pal = [[0u8; 256]; 3];
        for i in 0..n_colors as usize {
            for j in (0..3usize).rev() {
                if palette_pos < data.len() {
                    pal[j][i] = data[palette_pos];
                    palette_pos += 1;
                }
            }
            palette_pos += 1; // reserved
        }
        palette = Some(pal);
    } else if n_colors != 0 {
        palette_pos += n_colors as usize * 4;
    }
    let _ = palette_pos;

    // sizeC / pixelType derivation (matches Java).
    let mut size_c: u32 = if bpp != 24 { 1 } else { 3 };
    if bpp == 32 {
        size_c = 4;
    }
    if bpp > 8 {
        bpp /= size_c as i32;
    }
    let mut pixel_type = match bpp {
        16 => PixelType::Uint16,
        32 => PixelType::Uint32,
        _ => PixelType::Uint8,
    };

    // BITFIELDS images are decoded into 8-bit RGB(A) channels (each component
    // is shifted and scaled to a full byte), regardless of the packed pixel
    // width. A 16-bit packed pixel yields RGB (3 channels); a 32-bit packed
    // pixel yields RGBA (4 channels) when an alpha mask is present, else RGB.
    if let Some((_r, _g, _b, alpha)) = bitfields {
        size_c = if bpp_total == 32 && alpha != 0 { 4 } else { 3 };
        pixel_type = PixelType::Uint8;
        bpp = 8;
    }

    let is_indexed = palette.is_some();
    if is_indexed {
        size_c = 1;
    }
    let is_rgb = size_c > 1;

    // -- decode pixel data --
    let effective_c = if is_indexed { 1 } else { size_c as usize };
    let bytes_per_sample = pixel_type.bytes_per_sample();
    let bpp_u = bpp as usize; // bits per sample
    let w = size_x as usize;
    let h = size_y as usize;

    // Output: interleaved, effective_c samples per pixel, row-major top-to-bottom.
    let out_len = w * h * effective_c * bytes_per_sample;
    let mut buf = vec![0u8; out_len];

    if compression == BMP_RAW {
        // Row length in bytes for the source data (per Java: sizeX * (indexed?1:sizeC) * bpp / 8).
        let row_bits = w * effective_c * bpp_u;
        let row_bytes = row_bits.div_ceil(8);
        // Rows are padded to a 4-byte boundary.
        let padded_row = (row_bytes + 3) & !3;
        let mut pos = global;
        // BMP stores rows bottom-up unless invert_y.
        for src_row in 0..h {
            // The output row this source row maps to.
            let out_row = if invert_y { src_row } else { h - 1 - src_row };
            let row_start = pos;
            let row_end = row_start.checked_add(row_bytes).ok_or_else(|| {
                BioFormatsError::InvalidData("BMP: pixel row offset overflow".into())
            })?;
            if row_end > data.len() {
                return Err(BioFormatsError::InvalidData(
                    "BMP: pixel data is shorter than expected".into(),
                ));
            }
            // Read samples for this row.
            for i in 0..(w * effective_c) {
                let sample_byte0 = row_start + i * (bpp_u / 8).max(1);
                if bpp_u <= 8 {
                    // sub-byte or byte samples
                    let bit_off = i * bpp_u;
                    let byte_i = row_start + bit_off / 8;
                    let val = if bpp_u == 8 {
                        data[byte_i]
                    } else {
                        let shift = 8 - bpp_u - (bit_off % 8);
                        (data[byte_i] >> shift) & ((1u16 << bpp_u) - 1) as u8
                    };
                    buf[out_row * w * effective_c + i] = val;
                } else {
                    let nb = bpp_u / 8;
                    let dst = (out_row * w * effective_c + i) * nb;
                    for b in 0..nb {
                        let s = sample_byte0 + b;
                        buf[dst + b] = data[s];
                    }
                }
            }
            pos += padded_row;
            if pos > data.len() {
                pos = data.len();
            }
        }
    } else if compression == BMP_RLE_8 || compression == BMP_RLE_4 {
        // Decode into an index plane of size w*h (indexed images only here).
        let mut plane = vec![0u8; w * h];
        let mut index = 0usize;
        let mut pos = global;
        let row_length = w; // one byte per pixel for 8-bit indexed
        'outer: loop {
            if pos + 1 >= data.len() {
                break;
            }
            let first = data[pos];
            let second = data[pos + 1];
            pos += 2;
            if first == 0 {
                if second == 1 {
                    break;
                } else if second == 2 {
                    if pos + 1 >= data.len() {
                        break;
                    }
                    let x_delta = data[pos] as usize;
                    let y_delta = data[pos + 1] as usize;
                    pos += 2;
                    index += y_delta * row_length + x_delta;
                } else if second > 2 {
                    // Absolute mode.
                    let count = second as usize;
                    if compression == BMP_RLE_8 {
                        for _ in 0..count {
                            if pos >= data.len() || index >= plane.len() {
                                break 'outer;
                            }
                            plane[index] = data[pos];
                            index += 1;
                            pos += 1;
                        }
                        if count % 2 == 1 {
                            pos += 1; // word alignment
                        }
                    } else {
                        // RLE_4 absolute: two nibbles per byte.
                        let mut i = 0;
                        while i < count {
                            if pos >= data.len() {
                                break 'outer;
                            }
                            let byte = data[pos];
                            pos += 1;
                            let first_nibble = byte & 0xf;
                            let second_nibble = (byte >> 4) & 0xf;
                            if index < plane.len() {
                                plane[index] = first_nibble;
                                index += 1;
                            }
                            if i + 1 < count && index < plane.len() {
                                plane[index] = second_nibble;
                                index += 1;
                            }
                            i += 2;
                        }
                        if count % 4 == 1 || count % 4 == 2 {
                            // align to word boundary (Java: count%4==2 -> skip 1)
                            if count % 4 == 2 {
                                pos += 1;
                            }
                        }
                    }
                }
            } else {
                let run = first as usize;
                if compression == BMP_RLE_8 {
                    for _ in 0..run {
                        if index >= plane.len() {
                            break;
                        }
                        plane[index] = second;
                        index += 1;
                    }
                } else {
                    let first_nibble = second & 0xf;
                    let second_nibble = (second >> 4) & 0xf;
                    for i in 0..run {
                        if index >= plane.len() {
                            break;
                        }
                        plane[index] = if i % 2 == 0 {
                            first_nibble
                        } else {
                            second_nibble
                        };
                        index += 1;
                    }
                }
            }
        }
        // RLE planes are stored bottom-up; flip into top-down output.
        for row in 0..h {
            let src = row * w;
            let out_row = if invert_y { row } else { h - 1 - row };
            buf[out_row * w..out_row * w + w].copy_from_slice(&plane[src..src + w]);
        }
    } else if compression == BMP_BITFIELDS {
        // Packed 16- or 32-bit pixels; extract each component via its mask and
        // scale to 8 bits. Output is interleaved R,G,B(,A) per pixel.
        let (rmask, gmask, bmask, amask) = bitfields.unwrap();
        let packed_bytes = (bpp_total / 8) as usize; // 2 for 16-bit, 4 for 32-bit
        let out_c = size_c as usize; // 3 (RGB) or 4 (RGBA)
                                     // Source row length (packed pixels), padded to a 4-byte boundary.
        let row_bytes = w * packed_bytes;
        let padded_row = (row_bytes + 3) & !3;
        let mut pos = global;
        for src_row in 0..h {
            let out_row = if invert_y { src_row } else { h - 1 - src_row };
            let row_end = pos.checked_add(row_bytes).ok_or_else(|| {
                BioFormatsError::InvalidData("BMP: pixel row offset overflow".into())
            })?;
            if row_end > data.len() {
                return Err(BioFormatsError::InvalidData(
                    "BMP: pixel data is shorter than expected".into(),
                ));
            }
            for px in 0..w {
                let sp = pos + px * packed_bytes;
                let pixel: u32 = match packed_bytes {
                    2 => u16::from_le_bytes([data[sp], data[sp + 1]]) as u32,
                    _ => rd_u32(&data, sp),
                };
                let dst = (out_row * w + px) * out_c;
                buf[dst] = extract_component(pixel, rmask);
                buf[dst + 1] = extract_component(pixel, gmask);
                buf[dst + 2] = extract_component(pixel, bmask);
                if out_c == 4 {
                    buf[dst + 3] = extract_component(pixel, amask);
                }
            }
            pos += padded_row;
            if pos > data.len() {
                pos = data.len();
            }
        }
    } else {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "BMP: unsupported compression {compression}"
        )));
    }

    // For multichannel images, swap BGR -> RGB (interleaved). BITFIELDS pixels
    // are already written in R,G,B(,A) order, so they are excluded.
    if size_c > 1 && !is_indexed && compression != BMP_BITFIELDS {
        let c = size_c as usize;
        let nb = bytes_per_sample;
        let n_pixels = buf.len() / (c * nb);
        for p in 0..n_pixels {
            let base = p * c * nb;
            // swap channel 0 and channel 2 (R and B); leave alpha (3) intact
            for b in 0..nb {
                buf.swap(base + b, base + 2 * nb + b);
            }
        }
    }

    let lookup_table = palette.map(|pal| {
        let mut red = vec![0u16; 256];
        let mut green = vec![0u16; 256];
        let mut blue = vec![0u16; 256];
        for i in 0..256 {
            red[i] = pal[0][i] as u16;
            green[i] = pal[1][i] as u16;
            blue[i] = pal[2][i] as u16;
        }
        LookupTable { red, green, blue }
    });

    let mut meta = ImageMetadata {
        size_x,
        size_y,
        size_z: 1,
        size_c,
        size_t: 1,
        pixel_type,
        bits_per_pixel: bpp.max(8) as u8,
        image_count: 1,
        dimension_order: DimensionOrder::XYCTZ,
        is_rgb,
        is_interleaved: true,
        is_indexed,
        is_little_endian: true,
        resolution_count: 1,
        lookup_table,
        ..Default::default()
    };
    use crate::common::metadata::MetadataValue;
    meta.series_metadata.insert(
        "X resolution".into(),
        MetadataValue::Int(pixel_size_x as i64),
    );
    meta.series_metadata.insert(
        "Y resolution".into(),
        MetadataValue::Int(pixel_size_y as i64),
    );

    Ok((meta, buf))
}

impl FormatReader for BmpReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("bmp"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.starts_with(b"BM")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let (meta, pixels) = load_bmp(path)?;
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
        // Output is interleaved with size_c samples per pixel (indexed -> 1).
        let channels = if meta.is_indexed {
            1
        } else {
            meta.size_c as usize
        };
        crop_full_plane("BMP", &full, meta, channels, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        use crate::common::metadata::MetadataValue;
        use crate::common::ome_metadata::OmeMetadata;
        let meta = self.meta.as_ref()?;
        let mut ome = OmeMetadata::from_image_metadata(meta);
        if let Some(img) = ome.images.get_mut(0) {
            // MetadataTools.populatePixels sets the image name to the basename.
            img.name = self
                .path
                .as_ref()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .map(|s| s.to_string());
            // BMPReader.java: resolution is stored as pixels-per-metre; convert
            // to microns-per-pixel via 1000000 / pixelsPerMetre. A non-positive
            // value yields no PhysicalSize (FormatTools.getPhysicalSizeX -> null).
            let phys = |key: &str| -> Option<f64> {
                match meta.series_metadata.get(key) {
                    Some(MetadataValue::Int(v)) if *v > 0 => Some(1_000_000.0 / *v as f64),
                    _ => None,
                }
            };
            img.physical_size_x = phys("X resolution");
            img.physical_size_y = phys("Y resolution");
        }
        Some(ome)
    }
}

use crate::common::writer::FormatWriter;

pub struct BmpWriter {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    wrote: bool,
}

impl BmpWriter {
    pub fn new() -> Self {
        BmpWriter {
            path: None,
            meta: None,
            wrote: false,
        }
    }
}

impl Default for BmpWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatWriter for BmpWriter {
    fn is_this_type(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("bmp"))
            .unwrap_or(false)
    }

    fn set_metadata(&mut self, meta: &ImageMetadata) -> Result<()> {
        let logical_c = if meta.is_rgb { 1 } else { meta.size_c.max(1) };
        let required_planes = meta
            .size_z
            .max(1)
            .checked_mul(logical_c)
            .and_then(|v| v.checked_mul(meta.size_t.max(1)))
            .ok_or_else(|| BioFormatsError::Format("BMP writer plane count overflow".into()))?;
        if required_planes > 1 || meta.image_count > 1 {
            return Err(BioFormatsError::UnsupportedFormat(
                "BMP writer supports only one plane".into(),
            ));
        }
        if meta.pixel_type != PixelType::Uint8 {
            return Err(BioFormatsError::UnsupportedFormat(
                "BMP writer only supports Uint8".into(),
            ));
        }
        if !meta.is_rgb || meta.size_c != 3 {
            return Err(BioFormatsError::UnsupportedFormat(
                "BMP writer only supports RGB Uint8 data".into(),
            ));
        }
        self.meta = Some(meta.clone());
        self.wrote = false;
        Ok(())
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.meta
            .as_ref()
            .ok_or_else(|| BioFormatsError::Format("set_metadata first".into()))?;
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        if self.path.is_some() && !self.wrote {
            return Err(BioFormatsError::Format(
                "BMP writer closed before plane 0 was written".into(),
            ));
        }
        self.path = None;
        self.meta = None;
        self.wrote = false;
        Ok(())
    }

    fn save_bytes(&mut self, plane_index: u32, data: &[u8]) -> Result<()> {
        if plane_index != 0 {
            return Err(BioFormatsError::Format(
                "BMP writer supports only one plane".into(),
            ));
        }
        if self.wrote {
            return Err(BioFormatsError::Format(
                "BMP writer already wrote plane 0".into(),
            ));
        }
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let (w, h) = (meta.size_x, meta.size_y);

        let img = image::RgbImage::from_raw(w, h, data.to_vec())
            .map(image::DynamicImage::ImageRgb8)
            .ok_or_else(|| BioFormatsError::InvalidData("bad data length".into()))?;

        img.save(path)
            .map_err(|e| BioFormatsError::Format(e.to_string()))?;
        self.wrote = true;
        Ok(())
    }

    fn can_do_stacks(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp_path(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("bioformats_bmp_{name}_{nanos}.bmp"))
    }

    #[test]
    fn mask_helpers() {
        // 5-6-5 masks.
        assert_eq!(mask_shift_scale(0xF800), Some((11, 0x1F)));
        assert_eq!(mask_shift_scale(0x07E0), Some((5, 0x3F)));
        assert_eq!(mask_shift_scale(0x001F), Some((0, 0x1F)));
        assert_eq!(mask_shift_scale(0), None);
        // Full-byte masks scale identically.
        assert_eq!(extract_component(0x00FF0000, 0x00FF0000), 0xFF);
        assert_eq!(extract_component(0x00000000, 0x00FF0000), 0x00);
        // Max value of a 5-bit channel scales to 255.
        assert_eq!(extract_component(0xF800, 0xF800), 0xFF);
        // Zero value -> 0.
        assert_eq!(extract_component(0x0000, 0xF800), 0x00);
    }

    /// Build a minimal BMP header for a BITFIELDS image.
    fn write_bmp(path: &Path, w: i32, h: i32, bpp: u16, masks: &[u32], pixel_data: &[u8]) {
        let palette_or_mask_bytes = masks.len() * 4;
        let header = 14 + 40 + palette_or_mask_bytes;
        let mut buf: Vec<u8> = Vec::new();
        // BITMAPFILEHEADER
        buf.extend_from_slice(b"BM");
        buf.extend_from_slice(&((header + pixel_data.len()) as u32).to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes()); // reserved
        buf.extend_from_slice(&(header as u32).to_le_bytes()); // pixel offset
                                                               // BITMAPINFOHEADER (40 bytes)
        buf.extend_from_slice(&40u32.to_le_bytes());
        buf.extend_from_slice(&w.to_le_bytes());
        buf.extend_from_slice(&h.to_le_bytes());
        buf.extend_from_slice(&1u16.to_le_bytes()); // planes
        buf.extend_from_slice(&bpp.to_le_bytes());
        buf.extend_from_slice(&3u32.to_le_bytes()); // BITFIELDS
        buf.extend_from_slice(&0u32.to_le_bytes()); // image size
        buf.extend_from_slice(&0i32.to_le_bytes()); // x ppm
        buf.extend_from_slice(&0i32.to_le_bytes()); // y ppm
        buf.extend_from_slice(&0u32.to_le_bytes()); // colors used
        buf.extend_from_slice(&0u32.to_le_bytes()); // colors important
        for m in masks {
            buf.extend_from_slice(&m.to_le_bytes());
        }
        buf.extend_from_slice(pixel_data);
        let mut f = File::create(path).unwrap();
        f.write_all(&buf).unwrap();
    }

    fn write_raw_bmp(path: &Path, w: i32, h: i32, bpp: u16, pixel_data: &[u8]) {
        let header = 14 + 40;
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(b"BM");
        buf.extend_from_slice(&((header + pixel_data.len()) as u32).to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(&(header as u32).to_le_bytes());
        buf.extend_from_slice(&40u32.to_le_bytes());
        buf.extend_from_slice(&w.to_le_bytes());
        buf.extend_from_slice(&h.to_le_bytes());
        buf.extend_from_slice(&1u16.to_le_bytes());
        buf.extend_from_slice(&bpp.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(&0i32.to_le_bytes());
        buf.extend_from_slice(&0i32.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(pixel_data);
        let mut f = File::create(path).unwrap();
        f.write_all(&buf).unwrap();
    }

    #[test]
    fn bitfields_16bit_565() {
        // 1x1 image, 5-6-5. Encode pure red (max R), pure green, pure blue.
        // Pixel packed as: R<<11 | G<<5 | B.
        let path = tmp_path("bf16");
        let red: u16 = 0x1F << 11; // R=31, G=0, B=0
        let row = [red.to_le_bytes()[0], red.to_le_bytes()[1], 0, 0]; // padded to 4 bytes
        write_bmp(&path, 1, 1, 16, &[0xF800, 0x07E0, 0x001F], &row);
        let (meta, buf) = load_bmp(&path).unwrap();
        std::fs::remove_file(&path).ok();
        assert_eq!(meta.size_c, 3);
        assert_eq!(meta.pixel_type, PixelType::Uint8);
        // Interleaved R,G,B; R should be 255, G and B 0.
        assert_eq!(buf[0], 255);
        assert_eq!(buf[1], 0);
        assert_eq!(buf[2], 0);
    }

    #[test]
    fn bitfields_32bit_rgba() {
        // 1x1, masks R=0x00FF0000 G=0x0000FF00 B=0x000000FF A=0xFF000000.
        let path = tmp_path("bf32");
        // pixel value: A=0x80 R=0x10 G=0x20 B=0x30 -> 0x80102030
        let pixel: u32 = 0x80102030;
        write_bmp(
            &path,
            1,
            1,
            32,
            &[0x00FF0000, 0x0000FF00, 0x000000FF, 0xFF000000],
            &pixel.to_le_bytes(),
        );
        let (meta, buf) = load_bmp(&path).unwrap();
        std::fs::remove_file(&path).ok();
        assert_eq!(meta.size_c, 4);
        assert_eq!(meta.pixel_type, PixelType::Uint8);
        // R,G,B,A interleaved.
        assert_eq!(buf[0], 0x10);
        assert_eq!(buf[1], 0x20);
        assert_eq!(buf[2], 0x30);
        assert_eq!(buf[3], 0x80);
    }

    #[test]
    fn raw_payload_rejects_truncated_rows() {
        let path = tmp_path("raw_truncated");
        // 2x2 24-bit rows need 6 pixel bytes each, padded to 8 bytes. This
        // provides only one complete row, so the second must not decode as
        // zero-filled pixels.
        write_raw_bmp(&path, 2, 2, 24, &[1, 2, 3, 4, 5, 6, 0, 0]);
        let err = load_bmp(&path).expect_err("truncated raw BMP payload should be rejected");
        std::fs::remove_file(&path).ok();
        assert!(
            matches!(err, BioFormatsError::InvalidData(message) if message.contains("shorter"))
        );
    }

    #[test]
    fn bitfields_payload_rejects_truncated_rows() {
        let path = tmp_path("bitfields_truncated");
        write_bmp(&path, 2, 1, 16, &[0xF800, 0x07E0, 0x001F], &[0x00, 0xF8]);
        let err = load_bmp(&path).expect_err("truncated bitfields BMP payload should be rejected");
        std::fs::remove_file(&path).ok();
        assert!(
            matches!(err, BioFormatsError::InvalidData(message) if message.contains("shorter"))
        );
    }
}
