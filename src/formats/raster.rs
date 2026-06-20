//! Readers (and writers where possible) for additional raster formats via the `image` crate:
//! GIF, TGA, WebP, PNM, HDR/RGBE, OpenEXR, DDS, Farbfeld.
//!
//! All share the same generic implementation; the only difference is the extension/magic check.
//!
//! Animated GIFs are read as an image stack (one plane per frame), matching the
//! Java `GIFReader`; animated PNG (APNG) is still rejected.
//! Indexed/paletted inputs that the `image` crate can decode, including GIF and TGA,
//! are expanded to concrete samples and reported as non-indexed RGB/RGBA data.

use image::GenericImageView;
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, LookupTable, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;
use crate::common::writer::FormatWriter;

// ---- generic helper ---------------------------------------------------------

#[derive(Clone, Copy)]
enum RasterBehavior {
    Still,
}

fn load_image(path: &Path, behavior: RasterBehavior) -> Result<(ImageMetadata, Vec<u8>)> {
    match behavior {
        RasterBehavior::Still => {}
    }

    let img = image::open(path).map_err(|e| BioFormatsError::Format(e.to_string()))?;
    let (w, h) = img.dimensions();

    let (pixel_type, spp, raw): (PixelType, u32, Vec<u8>) = match img {
        image::DynamicImage::ImageLuma8(b) => (PixelType::Uint8, 1, b.into_raw()),
        image::DynamicImage::ImageLumaA8(b) => (PixelType::Uint8, 2, b.into_raw()),
        image::DynamicImage::ImageRgb8(b) => (PixelType::Uint8, 3, b.into_raw()),
        image::DynamicImage::ImageRgba8(b) => (PixelType::Uint8, 4, b.into_raw()),
        image::DynamicImage::ImageLuma16(b) => {
            let raw: Vec<u8> = b.into_raw().iter().flat_map(|v| v.to_le_bytes()).collect();
            (PixelType::Uint16, 1, raw)
        }
        image::DynamicImage::ImageLumaA16(b) => {
            let raw: Vec<u8> = b.into_raw().iter().flat_map(|v| v.to_le_bytes()).collect();
            (PixelType::Uint16, 2, raw)
        }
        image::DynamicImage::ImageRgb16(b) => {
            let raw: Vec<u8> = b.into_raw().iter().flat_map(|v| v.to_le_bytes()).collect();
            (PixelType::Uint16, 3, raw)
        }
        image::DynamicImage::ImageRgba16(b) => {
            let raw: Vec<u8> = b.into_raw().iter().flat_map(|v| v.to_le_bytes()).collect();
            (PixelType::Uint16, 4, raw)
        }
        image::DynamicImage::ImageRgb32F(b) => {
            let raw: Vec<u8> = b.into_raw().iter().flat_map(|v| v.to_le_bytes()).collect();
            (PixelType::Float32, 3, raw)
        }
        image::DynamicImage::ImageRgba32F(b) => {
            let raw: Vec<u8> = b.into_raw().iter().flat_map(|v| v.to_le_bytes()).collect();
            (PixelType::Float32, 4, raw)
        }
        other => {
            let rgb = other.to_rgb8();
            (PixelType::Uint8, 3, rgb.into_raw())
        }
    };

    let bpp = (pixel_type.bytes_per_sample() as u8) * 8;
    let is_rgb = spp >= 3;
    let meta = ImageMetadata {
        size_x: w,
        size_y: h,
        size_z: 1,
        size_c: spp,
        size_t: 1,
        pixel_type,
        bits_per_pixel: bpp,
        image_count: 1,
        dimension_order: DimensionOrder::XYCZT,
        is_rgb,
        is_interleaved: true,
        is_indexed: false,
        is_little_endian: true,
        resolution_count: 1,
        ..Default::default()
    };
    Ok((meta, raw))
}

// ---- generic reader struct --------------------------------------------------

struct GenericReader {
    exts: &'static [&'static str],
    /// Returns true if the header matches.
    magic_fn: fn(&[u8]) -> bool,
    behavior: RasterBehavior,
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    pixels: Option<Vec<u8>>,
}

impl GenericReader {
    fn new(
        exts: &'static [&'static str],
        magic_fn: fn(&[u8]) -> bool,
        behavior: RasterBehavior,
    ) -> Self {
        GenericReader {
            exts,
            magic_fn,
            behavior,
            path: None,
            meta: None,
            pixels: None,
        }
    }
}

impl FormatReader for GenericReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        self.exts.iter().any(|&e| ext.as_deref() == Some(e))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        (self.magic_fn)(header)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let (meta, pixels) = load_image(path, self.behavior)?;
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
        if self.meta.is_none() || s != 0 {
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

    fn open_bytes(&mut self, idx: u32) -> Result<Vec<u8>> {
        if idx != 0 {
            return Err(BioFormatsError::PlaneOutOfRange(idx));
        }
        self.pixels.clone().ok_or(BioFormatsError::NotInitialized)
    }

    fn open_bytes_region(&mut self, idx: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let full = self.open_bytes(idx)?;
        let meta = self.meta.as_ref().unwrap();
        crop_full_plane("raster", &full, meta, meta.size_c as usize, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, idx: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = ((meta.size_x - tw) / 2, (meta.size_y - th) / 2);
        self.open_bytes_region(idx, tx, ty, tw, th)
    }
}

// ---- public constructors for each format ------------------------------------

pub fn gif_reader() -> impl FormatReader {
    GifReader::new()
}

/// Multi-frame GIF reader.
///
/// Faithful to the Java `GIFReader`, which reads every frame of an (animated)
/// GIF as a separate plane, producing an image stack (`sizeT = imageCount`).
/// Frames are exposed as indexed 8-bit planes (`sizeC = 1`) with the active
/// colour table, matching Java `GIFReader`.
pub struct GifReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    frames: Vec<Vec<u8>>,
    color_tables: Vec<LookupTable>,
    planes_read: Vec<bool>,
    transparency: bool,
    trans_index: u8,
}

impl GifReader {
    pub fn new() -> Self {
        GifReader {
            path: None,
            meta: None,
            frames: Vec::new(),
            color_tables: Vec::new(),
            planes_read: Vec::new(),
            transparency: false,
            trans_index: 0,
        }
    }
}

impl Default for GifReader {
    fn default() -> Self {
        Self::new()
    }
}

fn load_gif_frames(path: &Path) -> Result<(ImageMetadata, Vec<Vec<u8>>, Vec<LookupTable>)> {
    let decoded = decode_gif_indexed(path)?;
    if decoded.frames.is_empty() {
        return Err(BioFormatsError::InvalidData(
            "GIF contains no frames".into(),
        ));
    }

    let image_count = decoded.frames.len() as u32;
    let meta = ImageMetadata {
        size_x: decoded.width,
        size_y: decoded.height,
        size_z: 1,
        size_c: 1,
        size_t: image_count,
        pixel_type: PixelType::Uint8,
        bits_per_pixel: 8,
        image_count,
        // Java GIFReader uses XYCTZ (frames vary over T).
        dimension_order: DimensionOrder::XYCTZ,
        is_rgb: false,
        is_interleaved: true,
        is_indexed: true,
        is_little_endian: true,
        resolution_count: 1,
        lookup_table: decoded.color_tables.first().cloned(),
        series_metadata: [
            (
                "Use transparency".to_string(),
                MetadataValue::Bool(decoded.transparency),
            ),
            (
                "Transparency index".to_string(),
                MetadataValue::Int(decoded.trans_index as i64),
            ),
            (
                "Interlace".to_string(),
                MetadataValue::Bool(decoded.interlace),
            ),
            (
                "Block size".to_string(),
                MetadataValue::Int(decoded.block_size as i64),
            ),
            (
                "Global lookup table size".to_string(),
                MetadataValue::Int(decoded.global_lut_size as i64),
            ),
        ]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    Ok((meta, decoded.frames, decoded.color_tables))
}

struct GifDecoded {
    width: u32,
    height: u32,
    frames: Vec<Vec<u8>>,
    color_tables: Vec<LookupTable>,
    transparency: bool,
    trans_index: u8,
    interlace: bool,
    block_size: u8,
    global_lut_size: usize,
}

struct GifCursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> GifCursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn read_u8(&mut self) -> Result<u8> {
        let value = *self
            .data
            .get(self.pos)
            .ok_or_else(|| BioFormatsError::InvalidData("truncated GIF".into()))?;
        self.pos += 1;
        Ok(value)
    }

    fn read_u16_le(&mut self) -> Result<u16> {
        let lo = self.read_u8()?;
        let hi = self.read_u8()?;
        Ok(u16::from_le_bytes([lo, hi]))
    }

    fn read_exact(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(len)
            .ok_or_else(|| BioFormatsError::InvalidData("GIF offset overflow".into()))?;
        let bytes = self
            .data
            .get(self.pos..end)
            .ok_or_else(|| BioFormatsError::InvalidData("truncated GIF".into()))?;
        self.pos = end;
        Ok(bytes)
    }

    fn skip(&mut self, len: usize) -> Result<()> {
        self.read_exact(len).map(|_| ())
    }
}

fn read_gif_lut(c: &mut GifCursor<'_>, size: usize) -> Result<LookupTable> {
    let bytes = c.read_exact(3 * size)?;
    let mut red = vec![0u16; 256];
    let mut green = vec![0u16; 256];
    let mut blue = vec![0u16; 256];
    for i in 0..size {
        red[i] = bytes[3 * i] as u16;
        green[i] = bytes[3 * i + 1] as u16;
        blue[i] = bytes[3 * i + 2] as u16;
    }
    Ok(LookupTable { red, green, blue })
}

fn read_gif_sub_blocks(c: &mut GifCursor<'_>, last_block_size: &mut u8) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let size = c.read_u8()?;
        *last_block_size = size;
        if size == 0 {
            break;
        }
        out.extend_from_slice(c.read_exact(size as usize)?);
    }
    Ok(out)
}

fn skip_gif_sub_blocks(c: &mut GifCursor<'_>, last_block_size: &mut u8) -> Result<()> {
    loop {
        let size = c.read_u8()?;
        *last_block_size = size;
        if size == 0 {
            return Ok(());
        }
        c.skip(size as usize)?;
    }
}

fn decode_gif_lzw(min_code_size: u8, data: &[u8], expected_pixels: usize) -> Vec<u8> {
    const MAX_STACK_SIZE: usize = 4096;
    let data_size = min_code_size as usize;
    let clear = 1usize << data_size;
    let eoi = clear + 1;
    let mut available = clear + 2;
    let mut old_code: Option<usize> = None;
    let mut code_size = data_size + 1;
    let mut code_mask = (1usize << code_size) - 1;
    let mut prefix = vec![0usize; MAX_STACK_SIZE];
    let mut suffix = vec![0u8; MAX_STACK_SIZE];
    let mut pixel_stack = Vec::<u8>::with_capacity(MAX_STACK_SIZE + 1);
    for (i, item) in suffix
        .iter_mut()
        .enumerate()
        .take(clear.min(MAX_STACK_SIZE))
    {
        *item = i as u8;
    }

    let mut out = Vec::with_capacity(expected_pixels);
    let mut datum = 0usize;
    let mut bits = 0usize;
    let mut bi = 0usize;
    let mut first = 0usize;

    while out.len() < expected_pixels {
        if pixel_stack.is_empty() {
            while bits < code_size {
                let Some(byte) = data.get(bi) else {
                    out.resize(expected_pixels, 0);
                    return out;
                };
                datum |= (*byte as usize) << bits;
                bits += 8;
                bi += 1;
            }

            let mut code = datum & code_mask;
            datum >>= code_size;
            bits -= code_size;

            if code > available || code == eoi {
                break;
            }
            if code == clear {
                code_size = data_size + 1;
                code_mask = (1usize << code_size) - 1;
                available = clear + 2;
                old_code = None;
                continue;
            }
            if old_code.is_none() {
                pixel_stack.push(suffix[code]);
                old_code = Some(code);
                first = code;
                continue;
            }

            let in_code = code;
            if code == available {
                pixel_stack.push(first as u8);
                code = old_code.unwrap();
            }
            while code > clear {
                pixel_stack.push(suffix[code]);
                code = prefix[code];
            }
            first = suffix[code] as usize;
            if available >= MAX_STACK_SIZE {
                break;
            }
            pixel_stack.push(first as u8);
            prefix[available] = old_code.unwrap();
            suffix[available] = first as u8;
            available += 1;

            if (available & code_mask) == 0 && available < MAX_STACK_SIZE {
                code_size += 1;
                code_mask += available;
            }
            old_code = Some(in_code);
        }
        if let Some(pixel) = pixel_stack.pop() {
            out.push(pixel);
        }
    }

    out.resize(expected_pixels, 0);
    out
}

#[derive(Default)]
struct GifGraphicControl {
    dispose: u8,
    transparency: bool,
    trans_index: u8,
}

fn gif_set_pixels(
    canvas_w: usize,
    canvas_h: usize,
    frames: &[Vec<u8>],
    last_dispose: u8,
    rect: (usize, usize, usize, usize),
    interlace: bool,
    pixels: &[u8],
) -> Vec<u8> {
    let mut dest = vec![0u8; canvas_w * canvas_h];
    let mut last_image = None;
    if last_dispose > 0 {
        if last_dispose == 3 {
            let n = frames.len().saturating_sub(2);
            if n > 0 {
                last_image = Some(n - 1);
            }
        }
        if let Some(idx) = last_image {
            if let Some(prev) = frames.get(idx) {
                dest.copy_from_slice(prev);
            }
        }
    }

    let (ix, iy, iw, ih) = rect;
    let mut pass = 1;
    let mut inc = 8usize;
    let mut iline = 0usize;
    for i in 0..ih {
        let mut line = i;
        if interlace {
            if iline >= ih {
                pass += 1;
                match pass {
                    2 => iline = 4,
                    3 => {
                        iline = 2;
                        inc = 4;
                    }
                    4 => {
                        iline = 1;
                        inc = 2;
                    }
                    _ => {}
                }
            }
            line = iline;
            iline += inc;
        }
        line += iy;
        if line < canvas_h {
            let row_start = line * canvas_w;
            let mut dx = row_start + ix;
            let dlim = (dx + iw).min(row_start + canvas_w);
            let mut sx = i * iw;
            while dx < dlim {
                dest[dx] = pixels.get(sx).copied().unwrap_or(0);
                dx += 1;
                sx += 1;
            }
        }
    }
    dest
}

fn decode_gif_indexed(path: &Path) -> Result<GifDecoded> {
    let bytes = std::fs::read(path)?;
    let mut c = GifCursor::new(&bytes);
    let ident = c.read_exact(6)?;
    if !ident.starts_with(b"GIF") {
        return Err(BioFormatsError::Format("Not a valid GIF file.".into()));
    }

    let width = c.read_u16_le()? as u32;
    let height = c.read_u16_le()? as u32;
    let packed = c.read_u8()?;
    let gct_flag = (packed & 0x80) != 0;
    let global_lut_size = 2usize << (packed & 7);
    c.skip(2)?;

    let global_lut = if gct_flag {
        Some(read_gif_lut(&mut c, global_lut_size)?)
    } else {
        None
    };

    let mut frames = Vec::<Vec<u8>>::new();
    let mut color_tables = Vec::<LookupTable>::new();
    let mut graphic = GifGraphicControl::default();
    let mut last_dispose = 0u8;
    let mut last_block_size = 0u8;
    let mut last_interlace = false;

    loop {
        let code = match c.read_u8() {
            Ok(v) => v,
            Err(_) => break,
        };
        match code {
            0x2c => {
                let ix = c.read_u16_le()? as usize;
                let iy = c.read_u16_le()? as usize;
                let iw = c.read_u16_le()? as usize;
                let ih = c.read_u16_le()? as usize;
                let packed = c.read_u8()?;
                let lct_flag = (packed & 0x80) != 0;
                let interlace = (packed & 0x40) != 0;
                let lct_size = 2usize << (packed & 7);
                let active_lut = if lct_flag {
                    read_gif_lut(&mut c, lct_size)?
                } else {
                    global_lut
                        .clone()
                        .ok_or_else(|| BioFormatsError::Format("Color table not found.".into()))?
                };

                let min_code_size = c.read_u8()?;
                let lzw_data = read_gif_sub_blocks(&mut c, &mut last_block_size)?;
                let pixels = decode_gif_lzw(min_code_size, &lzw_data, iw * ih);
                let frame = gif_set_pixels(
                    width as usize,
                    height as usize,
                    &frames,
                    last_dispose,
                    (ix, iy, iw, ih),
                    interlace,
                    &pixels,
                );
                frames.push(frame);
                color_tables.push(active_lut);
                last_interlace = interlace;
                last_dispose = graphic.dispose;
            }
            0x21 => {
                let ext = c.read_u8()?;
                if ext == 0xf9 {
                    let block_len = c.read_u8()?;
                    if block_len >= 4 {
                        let packed = c.read_u8()?;
                        graphic.dispose = (packed & 0x1c) >> 1;
                        graphic.transparency = (packed & 1) != 0;
                        c.skip(2)?;
                        graphic.trans_index = c.read_u8()?;
                        if block_len > 4 {
                            c.skip((block_len - 4) as usize)?;
                        }
                    } else {
                        c.skip(block_len as usize)?;
                    }
                    let terminator = c.read_u8()?;
                    last_block_size = terminator;
                } else {
                    skip_gif_sub_blocks(&mut c, &mut last_block_size)?;
                }
            }
            0x3b => break,
            _ => {}
        }
    }

    Ok(GifDecoded {
        width,
        height,
        frames,
        color_tables,
        transparency: graphic.transparency,
        trans_index: graphic.trans_index,
        interlace: last_interlace,
        block_size: last_block_size,
        global_lut_size,
    })
}

impl FormatReader for GifReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("gif"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, h: &[u8]) -> bool {
        h.starts_with(b"GIF")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let (meta, frames, color_tables) = load_gif_frames(path)?;
        self.path = Some(path.to_path_buf());
        self.meta = Some(meta);
        self.frames = frames;
        self.color_tables = color_tables;
        self.planes_read = vec![false; self.frames.len()];
        self.transparency = self
            .meta
            .as_ref()
            .and_then(|m| m.series_metadata.get("Use transparency"))
            .and_then(|v| match v {
                MetadataValue::Bool(b) => Some(*b),
                _ => None,
            })
            .unwrap_or(false);
        self.trans_index = self
            .meta
            .as_ref()
            .and_then(|m| m.series_metadata.get("Transparency index"))
            .and_then(|v| match v {
                MetadataValue::Int(i) => u8::try_from(*i).ok(),
                _ => None,
            })
            .unwrap_or(0);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.frames.clear();
        self.color_tables.clear();
        self.planes_read.clear();
        self.transparency = false;
        self.trans_index = 0;
        Ok(())
    }

    fn series_count(&self) -> usize {
        usize::from(self.meta.is_some())
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.meta.is_none() || s != 0 {
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

    fn open_bytes(&mut self, idx: u32) -> Result<Vec<u8>> {
        let i = idx as usize;
        if i >= self.frames.len() {
            return Err(BioFormatsError::PlaneOutOfRange(idx));
        }

        // Java GIFReader applies transparent pixels lazily when a plane is first
        // read, inheriting from the preceding plane.
        if i > 0 && self.transparency && !self.planes_read[i] {
            let prev = if self.planes_read[i - 1] {
                self.frames[i - 1].clone()
            } else {
                self.open_bytes(idx - 1)?
            };
            let lut = self.color_tables.get(i).cloned();
            let transparent_rgb = if self.trans_index >= 127 {
                0
            } else {
                self.trans_index as u32
            };
            if let Some(lut) = lut {
                for (pixel, prev_pixel) in self.frames[i].iter_mut().zip(prev.iter()) {
                    let p = *pixel as usize;
                    let rgb = ((lut.red[p] as u32) << 16)
                        | ((lut.green[p] as u32) << 8)
                        | (lut.blue[p] as u32);
                    if rgb == transparent_rgb {
                        *pixel = *prev_pixel;
                    }
                }
            }
        }
        self.planes_read[i] = true;
        Ok(self.frames[i].clone())
    }

    fn open_bytes_region(&mut self, idx: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let full = self.open_bytes(idx)?;
        let meta = self.meta.as_ref().unwrap();
        crop_full_plane("gif", &full, meta, meta.size_c as usize, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, idx: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = ((meta.size_x - tw) / 2, (meta.size_y - th) / 2);
        self.open_bytes_region(idx, tx, ty, tw, th)
    }

    fn lookup_table(&mut self, plane_index: u32) -> Result<Option<LookupTable>> {
        if self.meta.is_none() {
            return Err(BioFormatsError::NotInitialized);
        }
        Ok(self.color_tables.get(plane_index as usize).cloned())
    }
}

pub fn tga_reader() -> impl FormatReader {
    TargaReader::new()
}

// ---- Truevision Targa reader ------------------------------------------------
//
// Faithful port of Java `loci.formats.in.TargaReader` (and the
// `loci.formats.codec.TargaRLECodec` used for the RLE image types).
//
// Targa image types:
//   1  = uncompressed color-mapped (indexed)
//   2  = uncompressed truecolor (RGB)
//   3  = uncompressed grayscale
//   9  = RLE color-mapped (indexed)
//   10 = RLE truecolor (RGB)
//   11 = RLE grayscale

/// State captured during `init_file`, mirroring the Java reader's fields.
///
/// (Java's `colorMap` field is exposed here through `ImageMetadata.lookup_table`
/// rather than a per-state copy, so it is not duplicated on this struct.)
struct TargaState {
    /// File offset of the pixel data (after header + color map).
    offset: usize,
    /// True for RLE image types (9/10/11).
    compressed: bool,
    /// Origin/orientation bits from the image descriptor (0..=3).
    orientation: i32,
    /// Bits per pixel (8/16/24/32).
    bits: i32,
}

struct TargaReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    /// Whole-file bytes, read at `set_id` (Java seeks within the open stream).
    data: Vec<u8>,
    state: Option<TargaState>,
}

impl TargaReader {
    fn new() -> Self {
        TargaReader {
            path: None,
            meta: None,
            data: Vec::new(),
            state: None,
        }
    }
}

/// Targa RLE decompression — port of `TargaRLECodec.decompress`.
///
/// `data` is the byte stream starting at the first RLE packet; `max_bytes` is
/// the decompressed plane size; `bits_per_sample` is the pixel depth in bits.
///
/// Each packet begins with a count byte `n` read as a signed value:
///   * `n >= 0`           — raw packet: the next `(n + 1)` pixels are literal.
///   * `n < 0`            — run packet: one pixel repeated `(n & 0x7f) + 1` times.
///
/// Deviation from Java: the upstream `TargaRLECodec` treats `n == -128` (`0x80`)
/// as a no-op (it falls through both branches). Per the Targa specification a
/// `0x80` count byte is a valid one-pixel run packet, and spec-compliant
/// encoders (e.g. the `image` crate's TGA writer) emit it. We therefore handle
/// it as a run packet so such files decode correctly; for any encoder that
/// never emits `0x80` the output is identical to Java's.
fn targa_rle_decompress(data: &[u8], max_bytes: usize, bits_per_sample: i32) -> Vec<u8> {
    let mut output: Vec<u8> = Vec::with_capacity(max_bytes);
    let bpp = (bits_per_sample / 8) as usize;
    let mut pos = 0usize;
    while output.len() < max_bytes {
        if pos >= data.len() {
            break;
        }
        // Java reads a signed byte `n`.
        let n = data[pos] as i8;
        pos += 1;
        if n >= 0 {
            // 0 <= n <= 127: raw packet of (n + 1) pixels.
            let count = bpp * (n as usize + 1);
            for _ in 0..count {
                if pos >= data.len() {
                    break;
                }
                output.push(data[pos]);
                pos += 1;
            }
        } else {
            // -128 <= n <= -1: run packet of (n & 0x7f) + 1 repeats of one pixel.
            let len = ((n as i32) & 0x7f) as usize + 1;
            if pos + bpp > data.len() {
                break;
            }
            let pixel = &data[pos..pos + bpp];
            pos += bpp;
            for _ in 0..len {
                output.extend_from_slice(pixel);
            }
        }
    }
    output
}

/// Parse the Targa header + color map — port of the field-reading portion of
/// `TargaReader.initFile`. Returns the populated state, metadata, and the
/// `identification` string.
fn targa_init_file(data: &[u8]) -> Result<(TargaState, ImageMetadata, String)> {
    if data.len() < 18 {
        return Err(BioFormatsError::Format("TGA file too short".into()));
    }
    let little_endian = true;
    let read_u8 = |p: usize| -> u32 { data[p] as u32 };
    let read_u16 = |p: usize| -> u32 { u16::from_le_bytes([data[p], data[p + 1]]) as u32 };

    let n_identification_chars = read_u8(0) as usize;
    let _has_color_map = read_u8(1) == 1; // color map type byte
    let image_type = data[2] as i8 as i32;
    let compressed = image_type == 9 || image_type == 10 || image_type == 11;

    // color map definition
    let color_map_origin = read_u16(3);
    let color_map_length = read_u16(5) as usize;
    let bits_per_entry = read_u8(7) as usize;

    // skip 4 bytes (x/y origin), then image spec
    let size_x = read_u16(12);
    let size_y = read_u16(14);
    let bits = read_u8(16) as i32;

    let image_descriptor = read_u8(17);
    let orientation = ((image_descriptor & 0x30) >> 4) as i32;

    let mut pos = 18usize;
    let id_end = (pos + n_identification_chars).min(data.len());
    let identification = String::from_utf8_lossy(&data[pos..id_end]).into_owned();
    pos = id_end;

    let mut color_map: Option<[Vec<u8>; 3]> = None;
    if color_map_length > 0 && bits_per_entry > 0 {
        let mut cm: [Vec<u8>; 3] = [
            vec![0u8; color_map_length],
            vec![0u8; color_map_length],
            vec![0u8; color_map_length],
        ];
        let entry_bytes = bits_per_entry / 8;
        for i in 0..color_map_length {
            if pos + entry_bytes > data.len() {
                break;
            }
            let v = &data[pos..pos + entry_bytes];
            pos += entry_bytes;
            if v.len() == 4 || v.len() == 3 {
                cm[0][i] = v[2];
                cm[1][i] = v[1];
                cm[2][i] = v[0];
            } else if v.len() == 2 {
                let pixel = if little_endian {
                    u16::from_le_bytes([v[0], v[1]])
                } else {
                    u16::from_be_bytes([v[0], v[1]])
                } as u32;
                cm[0][i] = ((pixel & 0x7c00) >> 10) as u8;
                cm[1][i] = ((pixel & 0x3e0) >> 5) as u8;
                cm[2][i] = (pixel & 0x1f) as u8;
            }
        }
        color_map = Some(cm);
    }

    let offset = pos;

    // core metadata (mirrors the second half of Java initFile)
    let is_rgb = image_type == 2 || image_type == 10;
    let size_c: u32 = if is_rgb { 3 } else { 1 };
    let is_indexed = color_map.is_some() && !is_rgb;
    // Java: m.bitsPerPixel = bits == 32 ? 8 : bits / sizeC
    let bits_per_pixel: u8 = if bits == 32 {
        8
    } else {
        (bits / size_c as i32) as u8
    };

    let mut meta = ImageMetadata {
        size_x,
        size_y,
        size_z: 1,
        size_c,
        size_t: 1,
        pixel_type: PixelType::Uint8,
        bits_per_pixel,
        image_count: 1,
        dimension_order: DimensionOrder::XYCZT,
        is_rgb,
        is_interleaved: true,
        is_indexed,
        is_little_endian: true,
        resolution_count: 1,
        ..Default::default()
    };

    // populate metadata hashtable (exact Java key names)
    let m = &mut meta.series_metadata;
    m.insert(
        "Color map present".into(),
        MetadataValue::Bool(_has_color_map),
    );
    m.insert("Image type".into(), MetadataValue::Int(image_type as i64));
    m.insert(
        "Color map origin".into(),
        MetadataValue::Int(color_map_origin as i64),
    );
    m.insert(
        "Color map length".into(),
        MetadataValue::Int(color_map_length as i64),
    );
    m.insert(
        "Bits per color map entry".into(),
        MetadataValue::Int(bits_per_entry as i64),
    );
    m.insert("Image width".into(), MetadataValue::Int(size_x as i64));
    m.insert("Image height".into(), MetadataValue::Int(size_y as i64));
    m.insert("Bits per pixel".into(), MetadataValue::Int(bits as i64));
    m.insert(
        "Identification".into(),
        MetadataValue::String(identification.clone()),
    );
    m.insert(
        "Image orientation".into(),
        MetadataValue::Int(orientation as i64),
    );
    m.insert("Pixel offset".into(), MetadataValue::Int(offset as i64));

    // expose color map as a lookup table for indexed images
    if is_indexed {
        if let Some(cm) = &color_map {
            meta.lookup_table = Some(LookupTable {
                red: cm[0].iter().map(|&b| b as u16).collect(),
                green: cm[1].iter().map(|&b| b as u16).collect(),
                blue: cm[2].iter().map(|&b| b as u16).collect(),
            });
        }
    }

    let state = TargaState {
        offset,
        compressed,
        orientation,
        bits,
    };
    Ok((state, meta, identification))
}

/// Unpack one plane, applying RLE decompression, orientation flips, and
/// channel reordering — port of `TargaReader.openBytes`.
fn targa_open_plane(data: &[u8], meta: &ImageMetadata, state: &TargaState) -> Vec<u8> {
    let size_x = meta.size_x as i64;
    let size_y = meta.size_y as i64;
    let size_c = meta.size_c as i64;
    let bits = state.bits;
    let orientation = state.orientation;

    let plane_size = (size_x * size_y * size_c) as usize;
    let mut buf = vec![0u8; plane_size];

    // bytes per pixel, rounded up to a multiple of 8 bits (Java's bpp loop)
    let mut bpp_bits = bits;
    while bpp_bits % 8 != 0 {
        bpp_bits += 1;
    }
    let bpp = (bpp_bits / 8) as i64;

    // source bytes for this plane
    let src: Vec<u8> = if state.compressed {
        targa_rle_decompress(&data[state.offset..], plane_size, bits)
    } else {
        data[state.offset.min(data.len())..].to_vec()
    };

    // full requested region is the whole plane
    let (x, y, w, h) = (0i64, 0i64, size_x, size_y);

    let row_skip = if orientation < 2 { size_y - h - y } else { y };
    let col_skip = if orientation % 2 == 1 {
        size_x - w - x
    } else {
        x
    };

    // sequential cursor into the (decompressed) source
    let mut sp: usize = 0;
    let read_byte = |sp: &mut usize| -> u8 {
        let v = if *sp < src.len() { src[*sp] } else { 0 };
        *sp += 1;
        v
    };
    let read_short = |sp: &mut usize| -> i32 {
        // little-endian, matching the Java stream order
        let lo = if *sp < src.len() { src[*sp] } else { 0 } as i32;
        let hi = if *sp + 1 < src.len() { src[*sp + 1] } else { 0 } as i32;
        *sp += 2;
        (hi << 8) | lo
    };
    let skip = |sp: &mut usize, n: i64| {
        if n > 0 {
            *sp = sp.saturating_add(n as usize);
        }
    };

    skip(&mut sp, row_skip * size_x * bpp);
    for row in 0..h {
        if sp >= src.len() {
            break;
        }
        skip(&mut sp, col_skip * bpp);
        for col in 0..w {
            if sp >= src.len() {
                break;
            }
            let row_index = if orientation < 2 { h - row - 1 } else { row };
            let col_index = if orientation % 2 == 1 {
                w - col - 1
            } else {
                col
            };
            let index = (size_c * (row_index * w + col_index)) as usize;
            if bpp == 2 {
                let v = read_short(&mut sp);
                if index + 2 < buf.len() {
                    buf[index] = ((v & 0x7c00) >> 10) as u8;
                    buf[index + 1] = ((v & 0x3e0) >> 5) as u8;
                    buf[index + 2] = (v & 0x1f) as u8;
                }
            } else if bpp == 4 {
                let b2 = read_byte(&mut sp);
                let b1 = read_byte(&mut sp);
                let b0 = read_byte(&mut sp);
                skip(&mut sp, 1);
                if index + 2 < buf.len() {
                    buf[index + 2] = b2;
                    buf[index + 1] = b1;
                    buf[index] = b0;
                }
            } else {
                let mut c = size_c - 1;
                while c >= 0 {
                    let b = read_byte(&mut sp);
                    let bi = index + c as usize;
                    if bi < buf.len() {
                        buf[bi] = b;
                    }
                    c -= 1;
                }
            }
        }
        skip(&mut sp, bpp * (size_x - w - col_skip));
    }
    buf
}

impl FormatReader for TargaReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("tga") | Some("tpic"))
    }

    /// Byte-level heuristic mirroring Java `TargaReader.isThisType(stream)`:
    /// the Java base reader only validates the suffix here (there is no
    /// override), so accept any header — detection relies on the extension.
    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let data = std::fs::read(path)?;
        let (state, meta, _identification) = targa_init_file(&data)?;
        self.path = Some(path.to_path_buf());
        self.data = data;
        self.meta = Some(meta);
        self.state = Some(state);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.data = Vec::new();
        self.state = None;
        Ok(())
    }

    fn series_count(&self) -> usize {
        usize::from(self.meta.is_some())
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.meta.is_none() || s != 0 {
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

    fn open_bytes(&mut self, idx: u32) -> Result<Vec<u8>> {
        if idx != 0 {
            return Err(BioFormatsError::PlaneOutOfRange(idx));
        }
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let state = self.state.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        Ok(targa_open_plane(&self.data, meta, state))
    }

    fn open_bytes_region(&mut self, idx: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let full = self.open_bytes(idx)?;
        let meta = self.meta.as_ref().unwrap();
        crop_full_plane("tga", &full, meta, meta.size_c as usize, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, idx: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = ((meta.size_x - tw) / 2, (meta.size_y - th) / 2);
        self.open_bytes_region(idx, tx, ty, tw, th)
    }
}

pub fn webp_reader() -> impl FormatReader {
    GenericReader::new(
        &["webp"],
        |h| h.len() >= 12 && &h[0..4] == b"RIFF" && &h[8..12] == b"WEBP",
        RasterBehavior::Still,
    )
}

pub fn pnm_reader() -> impl FormatReader {
    PnmReader::new()
}

struct PnmReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    pixels: Option<Vec<u8>>,
}

impl PnmReader {
    fn new() -> Self {
        Self {
            path: None,
            meta: None,
            pixels: None,
        }
    }
}

fn pnm_is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\r' | b'\n' | b'\t' | 0x0b | 0x0c)
}

fn pnm_next_token(data: &[u8], pos: &mut usize) -> Option<String> {
    loop {
        while data.get(*pos).copied().is_some_and(pnm_is_space) {
            *pos += 1;
        }
        if data.get(*pos) != Some(&b'#') {
            break;
        }
        while let Some(&b) = data.get(*pos) {
            *pos += 1;
            if b == b'\r' || b == b'\n' {
                break;
            }
        }
    }

    let start = *pos;
    while data
        .get(*pos)
        .copied()
        .is_some_and(|b| !pnm_is_space(b) && b != b'#')
    {
        *pos += 1;
    }
    (*pos > start).then(|| String::from_utf8_lossy(&data[start..*pos]).into_owned())
}

fn pnm_pixel_offset(data: &[u8], mut pos: usize) -> usize {
    loop {
        while data.get(pos).copied().is_some_and(pnm_is_space) {
            pos += 1;
        }
        if data.get(pos) != Some(&b'#') {
            return pos;
        }
        while let Some(&b) = data.get(pos) {
            pos += 1;
            if b == b'\r' || b == b'\n' {
                break;
            }
        }
    }
}

fn pnm_ascii_pixels(data: &[u8], pixel_type: PixelType) -> Vec<u8> {
    let mut out = Vec::new();
    let mut value = 0u32;
    let mut in_number = false;
    for &b in data {
        if b.is_ascii_digit() {
            value = value.saturating_mul(10).saturating_add((b - b'0') as u32);
            in_number = true;
        } else if in_number {
            if pixel_type == PixelType::Uint16 {
                out.extend_from_slice(&(value as u16).to_le_bytes());
            } else {
                out.push(value as u8);
            }
            value = 0;
            in_number = false;
        }
    }
    if in_number {
        if pixel_type == PixelType::Uint16 {
            out.extend_from_slice(&(value as u16).to_le_bytes());
        } else {
            out.push(value as u8);
        }
    }
    out
}

fn load_pnm(path: &Path) -> Result<(ImageMetadata, Vec<u8>)> {
    let data = std::fs::read(path)?;
    if data.len() < 2 || data[0] != b'P' || !data[1].is_ascii_digit() {
        return Err(BioFormatsError::Format("Not a valid PNM file.".into()));
    }

    let magic = std::str::from_utf8(&data[..2])
        .map_err(|_| BioFormatsError::Format("Not a valid PNM file.".into()))?;
    let raw_bits = matches!(magic, "P4" | "P5" | "P6");
    let size_c = if matches!(magic, "P3" | "P6") { 3 } else { 1 };
    let black_and_white = matches!(magic, "P1" | "P4");

    let mut pos = 2usize;
    let size_x = pnm_next_token(&data, &mut pos)
        .ok_or_else(|| BioFormatsError::Format("PNM width missing".into()))?
        .parse::<u32>()
        .map_err(|_| BioFormatsError::Format("Invalid PNM width".into()))?;
    let size_y = pnm_next_token(&data, &mut pos)
        .ok_or_else(|| BioFormatsError::Format("PNM height missing".into()))?
        .parse::<u32>()
        .map_err(|_| BioFormatsError::Format("Invalid PNM height".into()))?;
    let pixel_type = if black_and_white {
        PixelType::Uint8
    } else {
        let max = pnm_next_token(&data, &mut pos)
            .ok_or_else(|| BioFormatsError::Format("PNM max value missing".into()))?
            .parse::<u32>()
            .map_err(|_| BioFormatsError::Format("Invalid PNM max value".into()))?;
        if max > 255 {
            PixelType::Uint16
        } else {
            PixelType::Uint8
        }
    };

    let bytes_per_sample = pixel_type.bytes_per_sample() as usize;
    let plane_len = (size_x as usize)
        .checked_mul(size_y as usize)
        .and_then(|v| v.checked_mul(size_c as usize))
        .and_then(|v| v.checked_mul(bytes_per_sample))
        .ok_or_else(|| BioFormatsError::Format("PNM plane size overflow".into()))?;

    let offset = pnm_pixel_offset(&data, pos);
    let mut pixels = if raw_bits {
        data.get(offset..).unwrap_or(&[]).to_vec()
    } else {
        pnm_ascii_pixels(data.get(offset..).unwrap_or(&[]), pixel_type)
    };
    pixels.resize(plane_len, 0);
    pixels.truncate(plane_len);

    let meta = ImageMetadata {
        size_x,
        size_y,
        size_z: 1,
        size_c,
        size_t: 1,
        pixel_type,
        bits_per_pixel: (bytes_per_sample as u8) * 8,
        image_count: 1,
        dimension_order: DimensionOrder::XYCZT,
        is_rgb: size_c == 3,
        is_interleaved: true,
        is_indexed: false,
        is_little_endian: true,
        resolution_count: 1,
        series_metadata: [(
            "Black and white".to_string(),
            MetadataValue::Bool(black_and_white),
        )]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    Ok((meta, pixels))
}

impl FormatReader for PnmReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(
            ext.as_deref(),
            Some("pbm") | Some("pam") | Some("pgm") | Some("ppm") | Some("pnm")
        )
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.len() >= 2 && header[0] == b'P' && header[1].is_ascii_digit()
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let (meta, pixels) = load_pnm(path)?;
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
        if self.meta.is_none() || s != 0 {
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

    fn open_bytes(&mut self, idx: u32) -> Result<Vec<u8>> {
        if idx != 0 {
            return Err(BioFormatsError::PlaneOutOfRange(idx));
        }
        self.pixels.clone().ok_or(BioFormatsError::NotInitialized)
    }

    fn open_bytes_region(&mut self, idx: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let full = self.open_bytes(idx)?;
        let meta = self.meta.as_ref().unwrap();
        crop_full_plane("pnm", &full, meta, meta.size_c as usize, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, idx: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = ((meta.size_x - tw) / 2, (meta.size_y - th) / 2);
        self.open_bytes_region(idx, tx, ty, tw, th)
    }
}

pub fn hdr_reader() -> impl FormatReader {
    // Radiance HDR as accepted by the image crate decoder.
    GenericReader::new(
        &["hdr"],
        |h| h.starts_with(b"#?RADIANCE"),
        RasterBehavior::Still,
    )
}

pub fn exr_reader() -> impl FormatReader {
    // OpenEXR: magic 0x76 0x2f 0x31 0x01
    GenericReader::new(
        &["exr"],
        |h| h.starts_with(&[0x76, 0x2f, 0x31, 0x01]),
        RasterBehavior::Still,
    )
}

pub fn dds_reader() -> impl FormatReader {
    GenericReader::new(&["dds"], |h| h.starts_with(b"DDS "), RasterBehavior::Still)
}

pub fn farbfeld_reader() -> impl FormatReader {
    GenericReader::new(
        &["ff", "farbfeld"],
        |h| h.starts_with(b"farbfeld"),
        RasterBehavior::Still,
    )
}

// ---- TGA writer (via image crate) -------------------------------------------

pub struct TgaWriter {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    wrote: bool,
}

impl TgaWriter {
    pub fn new() -> Self {
        TgaWriter {
            path: None,
            meta: None,
            wrote: false,
        }
    }
}

impl Default for TgaWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatWriter for TgaWriter {
    fn is_this_type(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("tga"))
            .unwrap_or(false)
    }
    fn set_metadata(&mut self, meta: &ImageMetadata) -> Result<()> {
        let logical_c = if meta.is_rgb { 1 } else { meta.size_c.max(1) };
        let required_planes = meta
            .size_z
            .max(1)
            .checked_mul(logical_c)
            .and_then(|v| v.checked_mul(meta.size_t.max(1)))
            .ok_or_else(|| BioFormatsError::Format("TGA writer plane count overflow".into()))?;
        if required_planes > 1 || meta.image_count > 1 {
            return Err(BioFormatsError::UnsupportedFormat(
                "TGA writer supports only one plane".into(),
            ));
        }
        if meta.pixel_type != PixelType::Uint8 {
            return Err(BioFormatsError::UnsupportedFormat(
                "TGA writer only supports Uint8 data".into(),
            ));
        }
        if meta.size_c != 1 && !(meta.is_rgb && matches!(meta.size_c, 3 | 4)) {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "TGA writer supports grayscale or RGB/RGBA Uint8 data, got {} channels",
                meta.size_c
            )));
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
        self.path = None;
        self.meta = None;
        self.wrote = false;
        Ok(())
    }
    fn save_bytes(&mut self, idx: u32, data: &[u8]) -> Result<()> {
        if idx != 0 {
            return Err(BioFormatsError::Format("TGA: single plane only".into()));
        }
        if self.wrote {
            return Err(BioFormatsError::Format(
                "TGA writer supports only one plane".into(),
            ));
        }
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let pixels = crate::common::writer::to_interleaved_samples(meta, data)?;
        let (w, h) = (meta.size_x, meta.size_y);
        let spp = meta.size_c as usize;
        let img: image::DynamicImage = match spp {
            1 => image::GrayImage::from_raw(w, h, pixels)
                .map(image::DynamicImage::ImageLuma8)
                .ok_or_else(|| BioFormatsError::InvalidData("bad length".into()))?,
            3 => image::RgbImage::from_raw(w, h, pixels)
                .map(image::DynamicImage::ImageRgb8)
                .ok_or_else(|| BioFormatsError::InvalidData("bad length".into()))?,
            4 => image::RgbaImage::from_raw(w, h, pixels)
                .map(image::DynamicImage::ImageRgba8)
                .ok_or_else(|| BioFormatsError::InvalidData("bad length".into()))?,
            _ => {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "TGA: spp={}",
                    spp
                )))
            }
        };
        img.save(path)
            .map_err(|e| BioFormatsError::Format(e.to_string()))?;
        self.wrote = true;
        Ok(())
    }
    fn can_do_stacks(&self) -> bool {
        false
    }
}

// ---- PNM writer (via image crate) -------------------------------------------

pub struct PnmWriter {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    wrote: bool,
}

impl PnmWriter {
    pub fn new() -> Self {
        PnmWriter {
            path: None,
            meta: None,
            wrote: false,
        }
    }
}

impl Default for PnmWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatWriter for PnmWriter {
    fn is_this_type(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| {
                matches!(
                    e.to_ascii_lowercase().as_str(),
                    "pnm" | "pgm" | "ppm" | "pbm"
                )
            })
            .unwrap_or(false)
    }

    fn set_metadata(&mut self, meta: &ImageMetadata) -> Result<()> {
        let logical_c = if meta.is_rgb { 1 } else { meta.size_c.max(1) };
        let required_planes = meta
            .size_z
            .max(1)
            .checked_mul(logical_c)
            .and_then(|v| v.checked_mul(meta.size_t.max(1)))
            .ok_or_else(|| BioFormatsError::Format("PNM writer plane count overflow".into()))?;
        if required_planes > 1 || meta.image_count > 1 {
            return Err(BioFormatsError::UnsupportedFormat(
                "PNM writer supports only one plane".into(),
            ));
        }
        if !matches!(meta.pixel_type, PixelType::Uint8 | PixelType::Uint16) {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "PNM writer only supports Uint8/Uint16 data, got {:?}",
                meta.pixel_type
            )));
        }
        if meta.size_c != 1 && !(meta.is_rgb && meta.size_c == 3) {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "PNM writer supports grayscale or RGB data, got {} channels",
                meta.size_c
            )));
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
                "PNM writer closed before plane 0 was written".into(),
            ));
        }
        self.path = None;
        self.meta = None;
        self.wrote = false;
        Ok(())
    }

    fn save_bytes(&mut self, idx: u32, data: &[u8]) -> Result<()> {
        if idx != 0 {
            return Err(BioFormatsError::Format(
                "PNM writer supports only one plane".into(),
            ));
        }
        if self.wrote {
            return Err(BioFormatsError::Format(
                "PNM writer already wrote plane 0".into(),
            ));
        }
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let pixels = crate::common::writer::to_interleaved_samples(meta, data)?;
        let spp = meta.size_c as usize;
        let magic = match spp {
            1 => "P5",
            3 => "P6",
            _ => {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "PNM writer: unsupported {:?} spp={}",
                    meta.pixel_type, spp
                )));
            }
        };
        let max = match meta.pixel_type {
            PixelType::Uint8 => 255,
            PixelType::Uint16 => 65535,
            _ => {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "PNM writer: unsupported {:?} spp={}",
                    meta.pixel_type, spp
                )));
            }
        };
        let mut out = format!("{magic}\n{} {}\n{max}\n", meta.size_x, meta.size_y).into_bytes();
        out.extend_from_slice(&pixels);
        std::fs::write(path, out).map_err(BioFormatsError::Io)?;
        self.wrote = true;
        Ok(())
    }

    fn can_do_stacks(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod targa_tests {
    use super::*;
    use std::io::Write;

    fn tmp_path(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("bioformats_tga_{name}_{nanos}.tga"))
    }

    /// Build a minimal Targa header.
    ///
    /// `descriptor` controls orientation; `0x20` selects a top-left origin so
    /// stored rows are already top-to-bottom (orientation bits == 2, no flip).
    fn header(
        id_len: u8,
        cmap_type: u8,
        image_type: u8,
        cmap_len: u16,
        cmap_entry_bits: u8,
        w: u16,
        h: u16,
        bits: u8,
        descriptor: u8,
    ) -> Vec<u8> {
        let mut v = Vec::new();
        v.push(id_len);
        v.push(cmap_type);
        v.push(image_type);
        v.extend_from_slice(&0u16.to_le_bytes()); // cmap origin
        v.extend_from_slice(&cmap_len.to_le_bytes());
        v.push(cmap_entry_bits);
        v.extend_from_slice(&[0u8; 4]); // x/y origin (skipped)
        v.extend_from_slice(&w.to_le_bytes());
        v.extend_from_slice(&h.to_le_bytes());
        v.push(bits);
        v.push(descriptor);
        v
    }

    fn write_and_read(name: &str, bytes: &[u8]) -> (ImageMetadata, Vec<u8>) {
        let p = tmp_path(name);
        std::fs::File::create(&p).unwrap().write_all(bytes).unwrap();
        let mut r = tga_reader();
        r.set_id(&p).unwrap();
        let meta = r.metadata().clone();
        let px = r.open_bytes(0).unwrap();
        let _ = std::fs::remove_file(&p);
        (meta, px)
    }

    /// A 2x2 truecolor image: pixels stored in BGR, top-left origin.
    /// Expected decoded RGB (row-major):
    ///   (0,0) red, (1,0) green, (0,1) blue, (1,1) white
    fn uncompressed_truecolor() -> Vec<u8> {
        let mut v = header(0, 0, 2, 0, 0, 2, 2, 24, 0x20);
        // BGR order on disk
        v.extend_from_slice(&[0x00, 0x00, 0xFF]); // red
        v.extend_from_slice(&[0x00, 0xFF, 0x00]); // green
        v.extend_from_slice(&[0xFF, 0x00, 0x00]); // blue
        v.extend_from_slice(&[0xFF, 0xFF, 0xFF]); // white
        v
    }

    #[test]
    fn truecolor_uncompressed() {
        let (meta, px) = write_and_read("tc", &uncompressed_truecolor());
        assert_eq!((meta.size_x, meta.size_y), (2, 2));
        assert_eq!(meta.size_c, 3);
        assert!(meta.is_rgb);
        assert!(!meta.is_indexed);
        assert_eq!(meta.pixel_type, PixelType::Uint8);
        // decoded as interleaved RGB, row-major
        assert_eq!(
            px,
            vec![
                0xFF, 0x00, 0x00, // red
                0x00, 0xFF, 0x00, // green
                0x00, 0x00, 0xFF, // blue
                0xFF, 0xFF, 0xFF, // white
            ]
        );
    }

    #[test]
    fn truecolor_rle_matches_uncompressed() {
        // Same 4 pixels as the uncompressed case, RLE-encoded (type 10).
        // Each pixel differs, so encode as a single raw packet of 4 pixels.
        let mut v = header(0, 0, 10, 0, 0, 2, 2, 24, 0x20);
        v.push(3); // raw packet: count = n + 1 = 4
        v.extend_from_slice(&[0x00, 0x00, 0xFF]); // red (BGR)
        v.extend_from_slice(&[0x00, 0xFF, 0x00]); // green
        v.extend_from_slice(&[0xFF, 0x00, 0x00]); // blue
        v.extend_from_slice(&[0xFF, 0xFF, 0xFF]); // white

        let (_, rle_px) = write_and_read("rle", &v);
        let (_, raw_px) = write_and_read("rle_ref", &uncompressed_truecolor());
        assert_eq!(rle_px, raw_px);
    }

    #[test]
    fn truecolor_rle_run_packet() {
        // 4 identical red pixels via a run packet, plus correctness of the
        // run-length expansion in targa_rle_decompress.
        let mut v = header(0, 0, 10, 0, 0, 2, 2, 24, 0x20);
        // run packet: high bit set, len = (n & 0x7f) + 1 = 4
        v.push(0x83);
        v.extend_from_slice(&[0x00, 0x00, 0xFF]); // red (BGR)

        let (_, px) = write_and_read("run", &v);
        assert_eq!(
            px,
            vec![0xFF, 0x00, 0x00, 0xFF, 0x00, 0x00, 0xFF, 0x00, 0x00, 0xFF, 0x00, 0x00,]
        );
    }

    #[test]
    fn color_mapped() {
        // 2x2 indexed image (type 1), 4-entry 24-bit palette.
        // Palette stored BGR; decoded LUT is RGB.
        let mut v = header(0, 1, 1, 4, 24, 2, 2, 8, 0x20);
        // palette entries (BGR on disk)
        v.extend_from_slice(&[0x00, 0x00, 0xFF]); // index 0 -> red
        v.extend_from_slice(&[0x00, 0xFF, 0x00]); // index 1 -> green
        v.extend_from_slice(&[0xFF, 0x00, 0x00]); // index 2 -> blue
        v.extend_from_slice(&[0xFF, 0xFF, 0xFF]); // index 3 -> white
                                                  // pixel indices, top-left origin
        v.extend_from_slice(&[0, 1, 2, 3]);

        let (meta, px) = write_and_read("cmap", &v);
        assert_eq!((meta.size_x, meta.size_y), (2, 2));
        assert_eq!(meta.size_c, 1);
        assert!(!meta.is_rgb);
        assert!(meta.is_indexed);
        // pixel data are the raw indices
        assert_eq!(px, vec![0, 1, 2, 3]);
        // lookup table reconstructs RGB
        let lut = meta.lookup_table.expect("indexed image has a LUT");
        assert_eq!(lut.red, vec![0xFF, 0x00, 0x00, 0xFF]);
        assert_eq!(lut.green, vec![0x00, 0xFF, 0x00, 0xFF]);
        assert_eq!(lut.blue, vec![0x00, 0x00, 0xFF, 0xFF]);
    }

    #[test]
    fn rle_decompress_unit() {
        // raw packet of 2 pixels (1 byte each) followed by a run of 3.
        // bytes: [0x01, A, B, 0x82, C] with bpp=1
        let data = [0x01u8, 0x10, 0x20, 0x82, 0x30];
        let out = targa_rle_decompress(&data, 5, 8);
        assert_eq!(out, vec![0x10, 0x20, 0x30, 0x30, 0x30]);
    }
}
