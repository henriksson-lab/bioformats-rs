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
fn rd_i16(b: &[u8], off: usize) -> i16 {
    i16::from_le_bytes([b[off], b[off + 1]])
}

struct MsbBitReader<'a> {
    data: &'a [u8],
    bit_pos: usize,
}

impl<'a> MsbBitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, bit_pos: 0 }
    }

    fn read_bits(&mut self, n: u32) -> Option<u8> {
        if self.bit_pos + n as usize > self.data.len() * 8 {
            return None;
        }
        let mut value = 0u8;
        for _ in 0..n {
            let byte_index = self.bit_pos / 8;
            let bit_index = 7 - (self.bit_pos % 8);
            value = (value << 1) | ((self.data[byte_index] >> bit_index) & 1);
            self.bit_pos += 1;
        }
        Some(value)
    }

    fn skip_bytes(&mut self, n: usize) {
        self.bit_pos += n * 8;
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
    let pixel_type = match bpp {
        16 => PixelType::Uint16,
        32 => PixelType::Uint32,
        _ => PixelType::Uint8,
    };

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
    } else if compression == BMP_RLE_8 {
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
                }
            } else {
                let run = first as usize;
                for _ in 0..run {
                    if index >= plane.len() {
                        break;
                    }
                    plane[index] = second;
                    index += 1;
                }
            }
        }
        // Java BMPReader decodes RLE into an in-memory plane and then calls
        // readPlane without applying BMP bottom-up inversion.
        buf.copy_from_slice(&plane);
    } else if compression == BMP_RLE_4 {
        let mut plane = vec![0u8; w * h];
        let mut index = 0usize;
        let mut bits = MsbBitReader::new(&data[global..]);
        let row_length = (w * bpp_u) / 8;
        loop {
            let Some(first) = bits.read_bits(bpp_u as u32) else {
                break;
            };
            let Some(second) = bits.read_bits(bpp_u as u32) else {
                break;
            };
            if first == 0 {
                if second == 1 {
                    break;
                } else if second == 2 {
                    let Some(x_delta) = bits.read_bits(bpp_u as u32) else {
                        break;
                    };
                    let Some(y_delta) = bits.read_bits(bpp_u as u32) else {
                        break;
                    };
                    index += y_delta as usize * row_length + x_delta as usize;
                } else if second > 2 {
                    for i in (0..second as usize).step_by(2) {
                        let Some(absolute) = bits.read_bits(bpp_u as u32) else {
                            break;
                        };
                        let first_nibble = absolute & 0xf;
                        let second_nibble = (absolute >> 4) & 0xf;
                        if index < plane.len() {
                            plane[index] = first_nibble;
                            index += 1;
                        }
                        if i + 1 < second as usize && index < plane.len() {
                            plane[index] = second_nibble;
                            index += 1;
                        }
                    }
                    if second % 4 == 2 {
                        bits.skip_bytes(1);
                    }
                }
            } else {
                let first_nibble = second & 0xf;
                let second_nibble = (second >> 4) & 0xf;
                for i in 0..first as usize {
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
        buf.copy_from_slice(&plane);
    } else if compression == BMP_BITFIELDS {
        // Java BMPReader records compression 3 as "RGB bitmap with mask" but
        // has no decode branch for it; the checked output buffer is returned
        // unchanged.
    } else {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "BMP: unsupported compression {compression}"
        )));
    }

    // For multichannel images, swap BGR -> RGB (interleaved).
    if size_c > 1 && !is_indexed {
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
        let pixels = crate::common::writer::to_interleaved_samples(meta, data)?;

        let img = image::RgbImage::from_raw(w, h, pixels)
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

    fn write_compressed_bmp(
        path: &Path,
        w: i32,
        h: i32,
        bpp: u16,
        compression: u32,
        n_colors: u32,
        pixel_data: &[u8],
    ) {
        let palette_bytes = n_colors as usize * 4;
        let header = 14 + 40 + palette_bytes;
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
        buf.extend_from_slice(&compression.to_le_bytes());
        buf.extend_from_slice(&(pixel_data.len() as u32).to_le_bytes());
        buf.extend_from_slice(&0i32.to_le_bytes());
        buf.extend_from_slice(&0i32.to_le_bytes());
        buf.extend_from_slice(&n_colors.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        for i in 0..n_colors {
            let v = i as u8;
            buf.extend_from_slice(&[v, v, v, 0]);
        }
        buf.extend_from_slice(pixel_data);
        let mut f = File::create(path).unwrap();
        f.write_all(&buf).unwrap();
    }

    #[test]
    fn bitfields_16bit_matches_java_undecoded_zero_buffer() {
        let path = tmp_path("bf16");
        let red: u16 = 0x1F << 11;
        let row = [red.to_le_bytes()[0], red.to_le_bytes()[1], 0, 0];
        write_bmp(&path, 1, 1, 16, &[0xF800, 0x07E0, 0x001F], &row);
        let (meta, buf) = load_bmp(&path).unwrap();
        std::fs::remove_file(&path).ok();
        assert_eq!(meta.size_c, 1);
        assert_eq!(meta.pixel_type, PixelType::Uint16);
        assert_eq!(buf, [0, 0]);
    }

    #[test]
    fn bitfields_32bit_matches_java_undecoded_zero_buffer() {
        let path = tmp_path("bf32");
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
        assert_eq!(buf, [0, 0, 0, 0]);
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
    fn rle8_matches_java_plane_without_vertical_flip() {
        let path = tmp_path("rle8_no_flip");
        // Absolute-mode four pixels, then EOF. Java decodes into a temporary
        // plane and readPlane reads it directly; it does not apply invertY.
        write_compressed_bmp(&path, 2, 2, 8, BMP_RLE_8, 4, &[0, 4, 1, 2, 3, 4, 0, 1]);
        let (meta, buf) = load_bmp(&path).unwrap();
        std::fs::remove_file(&path).ok();
        assert!(meta.is_indexed);
        assert_eq!(buf, [1, 2, 3, 4]);
    }

    #[test]
    fn rle4_reads_java_nibble_stream() {
        let path = tmp_path("rle4_nibbles");
        // Java reads RLE4 control values using readBits(4), so byte 0x4a is
        // first=4, second=10. The encoded run alternates second&0xf with
        // second>>4, yielding 10,0,10,0, then byte 0x01 is EOF.
        write_compressed_bmp(&path, 4, 1, 4, BMP_RLE_4, 16, &[0x4a, 0x01]);
        let (meta, buf) = load_bmp(&path).unwrap();
        std::fs::remove_file(&path).ok();
        assert!(!meta.is_indexed);
        assert_eq!(buf, [10, 0, 10, 0]);
    }

    #[test]
    fn bitfields_payload_does_not_require_decodable_rows_like_java() {
        let path = tmp_path("bitfields_truncated");
        write_bmp(&path, 2, 1, 16, &[0xF800, 0x07E0, 0x001F], &[0x00, 0xF8]);
        let (meta, buf) = load_bmp(&path).unwrap();
        std::fs::remove_file(&path).ok();
        assert_eq!(meta.size_x, 2);
        assert_eq!(buf, [0, 0, 0, 0]);
    }
}
