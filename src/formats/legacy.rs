//! Legacy and obscure format readers.
//!
//! - KodakBipReader: Kodak thermal camera (.bip)
//! - WoolzReader: Woolz graph-based image format (.wlz) — extension-only placeholder
//! - PictReader: Apple PICT format (.pict, .pct), bounded bitmap/pixmap support

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::codec::decompress_packbits;
use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, LookupTable, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;

fn region_crop(full: &[u8], meta: &ImageMetadata, x: u32, y: u32, w: u32, h: u32) -> Vec<u8> {
    let bps = meta.pixel_type.bytes_per_sample();
    let row = meta.size_x as usize * bps;
    let out_row = w as usize * bps;
    let mut out = Vec::with_capacity(h as usize * out_row);
    for r in 0..h as usize {
        let src = &full[(y as usize + r) * row..];
        out.extend_from_slice(&src[x as usize * bps..x as usize * bps + out_row]);
    }
    out
}

// ── KodakBipReader ────────────────────────────────────────────────────────────

const KODAK_BIP_HEADER_SIZE: u64 = 512;

pub struct KodakBipReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl KodakBipReader {
    pub fn new() -> Self {
        KodakBipReader {
            path: None,
            meta: None,
        }
    }
}

impl Default for KodakBipReader {
    fn default() -> Self {
        Self::new()
    }
}

fn parse_kodak_bip(path: &Path) -> Result<ImageMetadata> {
    let data = std::fs::read(path).map_err(BioFormatsError::Io)?;

    let (width, height) = if data.len() >= 22 {
        // Offset 16: width (u16 LE), offset 20: height (u16 LE)
        let w = u16::from_le_bytes([data[16], data[17]]) as u32;
        let h = u16::from_le_bytes([data[20], data[21]]) as u32;
        let w = if w == 0 { 512 } else { w };
        let h = if h == 0 { 512 } else { h };
        (w, h)
    } else {
        (512, 512)
    };

    Ok(ImageMetadata {
        size_x: width,
        size_y: height,
        size_z: 1,
        size_c: 1,
        size_t: 1,
        pixel_type: PixelType::Uint16,
        bits_per_pixel: 16,
        image_count: 1,
        dimension_order: DimensionOrder::XYZCT,
        is_rgb: false,
        is_interleaved: false,
        is_indexed: false,
        is_little_endian: true,
        resolution_count: 1,
        series_metadata: HashMap::new(),
        lookup_table: None,
        modulo_z: None,
        modulo_c: None,
        modulo_t: None,
    })
}

impl FormatReader for KodakBipReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("bip"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let meta = parse_kodak_bip(path)?;
        self.path = Some(path.to_path_buf());
        self.meta = Some(meta);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
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
        if plane_index != 0 {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let bps = meta.pixel_type.bytes_per_sample();
        let plane_bytes = meta.size_x as usize * meta.size_y as usize * bps;
        let path = self
            .path
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?
            .clone();
        let mut f = std::fs::File::open(&path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(KODAK_BIP_HEADER_SIZE))
            .map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; plane_bytes];
        let _ = f.read(&mut buf).map_err(BioFormatsError::Io)?;
        Ok(buf)
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
        Ok(region_crop(&full, meta, x, y, w, h))
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

// ── WoolzReader ───────────────────────────────────────────────────────────────

pub struct WoolzReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl WoolzReader {
    pub fn new() -> Self {
        WoolzReader {
            path: None,
            meta: None,
        }
    }
}

impl Default for WoolzReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for WoolzReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("wlz"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, _path: &Path) -> Result<()> {
        Err(BioFormatsError::UnsupportedFormat(
            "Woolz format reading is not yet implemented".to_string(),
        ))
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
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

    fn open_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "Woolz format reading is not yet implemented".to_string(),
        ))
    }

    fn open_bytes_region(
        &mut self,
        _plane_index: u32,
        _x: u32,
        _y: u32,
        _w: u32,
        _h: u32,
    ) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "Woolz format reading is not yet implemented".to_string(),
        ))
    }

    fn open_thumb_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "Woolz format reading is not yet implemented".to_string(),
        ))
    }
}

// ── PictReader ────────────────────────────────────────────────────────────────

const PICT_CLIP_RGN: u16 = 0x0001;
const PICT_BITSRECT: u16 = 0x0090;
const PICT_BITSRGN: u16 = 0x0091;
const PICT_PACKBITSRECT: u16 = 0x0098;
const PICT_PACKBITSRGN: u16 = 0x0099;
const PICT_PIXMAP_9A: u16 = 0x009a;
const PICT_END: u16 = 0x00ff;
const PICT_LONGCOMMENT: u16 = 0x00a1;
const PICT_JPEG: u16 = 0x0018;
const PICT_TYPE_1: u16 = 0x0a9f;
const PICT_TYPE_2: u16 = 0x9190;

pub struct PictReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    pixels: Option<Vec<u8>>,
}

impl PictReader {
    pub fn new() -> Self {
        PictReader {
            path: None,
            meta: None,
            pixels: None,
        }
    }
}

impl Default for PictReader {
    fn default() -> Self {
        Self::new()
    }
}

struct PictDecoded {
    meta: ImageMetadata,
    pixels: Vec<u8>,
}

struct PictCursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> PictCursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn seek(&mut self, pos: usize) -> Result<()> {
        if pos > self.data.len() {
            return Err(BioFormatsError::Format(
                "PICT: seek past end of file".into(),
            ));
        }
        self.pos = pos;
        Ok(())
    }

    fn position(&self) -> usize {
        self.pos
    }

    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    fn read_u8(&mut self) -> Result<u8> {
        if self.pos >= self.data.len() {
            return Err(BioFormatsError::Format(
                "PICT: unexpected end of file".into(),
            ));
        }
        let v = self.data[self.pos];
        self.pos += 1;
        Ok(v)
    }

    fn read_u16(&mut self) -> Result<u16> {
        let b = self.read_exact(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    fn read_i16(&mut self) -> Result<i16> {
        Ok(self.read_u16()? as i16)
    }

    fn read_exact(&mut self, len: usize) -> Result<&'a [u8]> {
        if self.pos + len > self.data.len() {
            return Err(BioFormatsError::Format(
                "PICT: unexpected end of file".into(),
            ));
        }
        let out = &self.data[self.pos..self.pos + len];
        self.pos += len;
        Ok(out)
    }

    fn skip(&mut self, len: usize) -> Result<()> {
        self.read_exact(len).map(|_| ())
    }
}

fn new_pict_meta(
    width: u32,
    height: u32,
    size_c: u32,
    is_rgb: bool,
    is_indexed: bool,
    lookup_table: Option<LookupTable>,
    version_one: bool,
) -> ImageMetadata {
    let mut series_metadata = HashMap::new();
    series_metadata.insert(
        "Version".into(),
        MetadataValue::Int(if version_one { 1 } else { 2 }),
    );
    ImageMetadata {
        size_x: width,
        size_y: height,
        size_z: 1,
        size_c,
        size_t: 1,
        pixel_type: PixelType::Uint8,
        bits_per_pixel: 8,
        image_count: 1,
        dimension_order: DimensionOrder::XYCZT,
        is_rgb,
        is_interleaved: false,
        is_indexed,
        is_little_endian: false,
        resolution_count: 1,
        series_metadata,
        lookup_table,
        modulo_z: None,
        modulo_c: None,
        modulo_t: None,
    }
}

fn parse_pict(path: &Path) -> Result<PictDecoded> {
    let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
    let mut c = PictCursor::new(&data);
    c.seek(518)?;
    let mut height = c.read_i16()?.max(0) as u32;
    let mut width = c.read_i16()?.max(0) as u32;

    let ver_opcode = c.read_u8()?;
    let ver_number = c.read_u8()?;
    let version_one = if ver_opcode == 0x11 && ver_number == 0x01 {
        true
    } else if ver_opcode == 0x00 && ver_number == 0x11 {
        let ver_number2 = c.read_u16()?;
        if ver_number2 != 0x02ff {
            return Err(BioFormatsError::Format(format!(
                "Invalid PICT file: {ver_number2}"
            )));
        }
        c.skip(18)?;
        let y = c.read_i16()?;
        let x = c.read_i16()?;
        if y > 0 {
            height = y as u32;
        }
        if x > 0 {
            width = x as u32;
        }
        c.skip(4)?;
        false
    } else {
        return Err(BioFormatsError::Format("Invalid PICT file".into()));
    };

    let mut decoded = None;
    loop {
        let opcode = if version_one {
            if c.remaining() == 0 {
                break;
            }
            c.read_u8()? as u16
        } else {
            if c.position() & 1 != 0 {
                c.skip(1)?;
            }
            if c.remaining() < 2 {
                break;
            }
            c.read_u16()?
        };

        match opcode {
            PICT_BITSRECT | PICT_BITSRGN | PICT_PACKBITSRECT | PICT_PACKBITSRGN => {
                let row_bytes = c.read_u16()?;
                decoded = Some(parse_pict_image(
                    &mut c,
                    opcode,
                    row_bytes,
                    version_one,
                    width,
                    height,
                )?);
            }
            PICT_PIXMAP_9A => {
                decoded = Some(parse_pict_image(
                    &mut c,
                    opcode,
                    0,
                    version_one,
                    width,
                    height,
                )?);
            }
            PICT_CLIP_RGN => {
                let len = c.read_u16()? as usize;
                if len < 2 {
                    return Err(BioFormatsError::Format("PICT: invalid clip region".into()));
                }
                c.skip(len - 2)?;
            }
            PICT_LONGCOMMENT => {
                c.skip(2)?;
                let len = c.read_u16()? as usize;
                c.skip(len)?;
            }
            PICT_JPEG => {
                return Err(BioFormatsError::UnsupportedFormat(
                    "PICT JPEG payloads are not implemented".into(),
                ));
            }
            PICT_TYPE_1 | PICT_TYPE_2 => {
                let len = c.read_u8()? as usize;
                c.skip(len)?;
            }
            PICT_END => break,
            _ if c.remaining() == 0 => break,
            _ => {}
        }
    }

    decoded.ok_or_else(|| BioFormatsError::Format("PICT: no bitmap or pixmap payload".into()))
}

fn parse_pict_image(
    c: &mut PictCursor<'_>,
    opcode: u16,
    row_bytes_raw: u16,
    version_one: bool,
    fallback_width: u32,
    fallback_height: u32,
) -> Result<PictDecoded> {
    let is_bitmap = version_one || (row_bytes_raw & 0x8000) == 0;
    if is_bitmap && opcode != PICT_PIXMAP_9A {
        let row_bytes = (row_bytes_raw & 0x3fff) as usize;
        let (width, height) = read_pict_image_rect(c, opcode, fallback_width, fallback_height)?;
        let rows = read_pict_rows(c, opcode, row_bytes, width as usize, height as usize, 1, 1)?;
        let mut pixels = Vec::with_capacity(width as usize * height as usize);
        for row in rows {
            pixels.extend(expand_pict_bits(1, &row, width as usize)?);
        }
        let meta = new_pict_meta(width, height, 1, false, false, None, version_one);
        return Ok(PictDecoded { meta, pixels });
    }

    let mut row_bytes = if opcode == PICT_PIXMAP_9A {
        0
    } else {
        (row_bytes_raw & 0x3fff) as usize
    };
    let (width, height) = read_pict_image_rect(c, opcode, fallback_width, fallback_height)?;
    let pixel_size = c.read_u16()?;
    let comp_count = c.read_u16()? as usize;
    c.skip(14)?;

    let lookup_table = if opcode == PICT_PIXMAP_9A {
        match pixel_size {
            32 => row_bytes = width as usize * comp_count,
            16 => row_bytes = width as usize * 2,
            _ => {
                return Err(BioFormatsError::UnsupportedFormat(
                    "PICT vector pixmap payloads are not implemented".into(),
                ));
            }
        }
        None
    } else {
        c.skip(4)?;
        c.skip(2)?;
        let count = c.read_u16()? as usize + 1;
        let mut red = Vec::with_capacity(count);
        let mut green = Vec::with_capacity(count);
        let mut blue = Vec::with_capacity(count);
        for _ in 0..count {
            c.skip(2)?;
            red.push((c.read_u8()? as u16) << 8);
            c.skip(1)?;
            green.push((c.read_u8()? as u16) << 8);
            c.skip(1)?;
            blue.push((c.read_u8()? as u16) << 8);
            c.skip(1)?;
        }
        Some(LookupTable { red, green, blue })
    };

    c.skip(18)?;
    if opcode == PICT_BITSRGN || opcode == PICT_PACKBITSRGN {
        c.skip(2)?;
    }

    let rows = read_pict_rows(
        c,
        opcode,
        row_bytes,
        width as usize,
        height as usize,
        pixel_size,
        comp_count,
    )?;
    rows_to_pict_decoded(
        rows,
        width,
        height,
        pixel_size,
        comp_count,
        lookup_table,
        version_one,
    )
}

fn read_pict_image_rect(
    c: &mut PictCursor<'_>,
    opcode: u16,
    fallback_width: u32,
    fallback_height: u32,
) -> Result<(u32, u32)> {
    if opcode == PICT_PIXMAP_9A {
        c.skip(6)?;
    }
    let top = c.read_i16()?;
    let left = c.read_i16()?;
    let bottom = c.read_i16()?;
    let right = c.read_i16()?;
    let width = if right > left {
        (right - left) as u32
    } else {
        fallback_width
    };
    let height = if bottom > top {
        (bottom - top) as u32
    } else {
        fallback_height
    };
    c.skip(18)?;
    if width == 0 || height == 0 {
        return Err(BioFormatsError::Format(
            "PICT: missing image dimensions".into(),
        ));
    }
    Ok((width, height))
}

fn read_pict_rows(
    c: &mut PictCursor<'_>,
    _opcode: u16,
    row_bytes: usize,
    width: usize,
    height: usize,
    pixel_size: u16,
    comp_count: usize,
) -> Result<Vec<Vec<u8>>> {
    let compressed = row_bytes >= 8 || pixel_size == 32;
    let mut rows = Vec::with_capacity(height);
    for _ in 0..height {
        let row = if compressed {
            let raw_len = if row_bytes > 250 {
                c.read_u16()? as usize
            } else {
                c.read_u8()? as usize
            };
            let packed = c.read_exact(raw_len)?;
            if pixel_size == 16 {
                unpack_pict_16_packbits(packed, width)?
            } else {
                let mut out = decompress_packbits(packed)?;
                let min_len = match pixel_size {
                    24 | 32 => width.saturating_mul(comp_count),
                    _ => row_bytes.max(width),
                };
                if out.len() < min_len {
                    out.resize(min_len, 0);
                }
                out
            }
        } else {
            c.read_exact(row_bytes)?.to_vec()
        };
        rows.push(row);
    }
    Ok(rows)
}

fn rows_to_pict_decoded(
    rows: Vec<Vec<u8>>,
    width: u32,
    height: u32,
    pixel_size: u16,
    comp_count: usize,
    lookup_table: Option<LookupTable>,
    version_one: bool,
) -> Result<PictDecoded> {
    let plane = width as usize * height as usize;
    match pixel_size {
        1 | 2 | 4 => {
            let mut pixels = Vec::with_capacity(plane);
            for row in rows {
                pixels.extend(expand_pict_bits(pixel_size as u8, &row, width as usize)?);
            }
            let meta = new_pict_meta(
                width,
                height,
                1,
                false,
                lookup_table.is_some(),
                lookup_table,
                version_one,
            );
            Ok(PictDecoded { meta, pixels })
        }
        8 => {
            let mut pixels = Vec::with_capacity(plane);
            for row in rows {
                if row.len() < width as usize {
                    return Err(BioFormatsError::Format("PICT: short 8-bit row".into()));
                }
                pixels.extend_from_slice(&row[..width as usize]);
            }
            let meta = new_pict_meta(
                width,
                height,
                1,
                false,
                lookup_table.is_some(),
                lookup_table,
                version_one,
            );
            Ok(PictDecoded { meta, pixels })
        }
        16 => {
            let mut pixels = vec![0u8; plane * 3];
            for (y, row) in rows.iter().enumerate() {
                for x in 0..width as usize {
                    let off = x * 2;
                    if off + 1 >= row.len() {
                        continue;
                    }
                    let v = u16::from_be_bytes([row[off], row[off + 1]]);
                    let base = y * width as usize + x;
                    pixels[base] = ((v & 0x7c00) >> 7) as u8;
                    pixels[plane + base] = ((v & 0x03e0) >> 2) as u8;
                    pixels[2 * plane + base] = ((v & 0x001f) << 3) as u8;
                }
            }
            let meta = new_pict_meta(width, height, 3, true, false, None, version_one);
            Ok(PictDecoded { meta, pixels })
        }
        24 | 32 => {
            let channels = comp_count.max(3);
            let mut pixels = vec![0u8; plane * 3];
            for (y, row) in rows.iter().enumerate() {
                for q in 0..3 {
                    let src_channel = if channels > 3 { channels - 3 + q } else { q };
                    let src = src_channel * width as usize;
                    let dst = q * plane + y * width as usize;
                    if src < row.len() {
                        let len = (width as usize).min(row.len() - src);
                        pixels[dst..dst + len].copy_from_slice(&row[src..src + len]);
                    }
                }
            }
            let meta = new_pict_meta(width, height, 3, true, false, None, version_one);
            Ok(PictDecoded { meta, pixels })
        }
        other => Err(BioFormatsError::UnsupportedFormat(format!(
            "PICT pixel size {other} is not implemented"
        ))),
    }
}

fn expand_pict_bits(bit_size: u8, input: &[u8], out_len: usize) -> Result<Vec<u8>> {
    if !matches!(bit_size, 1 | 2 | 4) {
        return Err(BioFormatsError::Format(format!(
            "PICT cannot expand {bit_size}-bit pixels"
        )));
    }
    let mut out = Vec::with_capacity(out_len);
    let count = 8 / bit_size;
    let mask = (1u8 << bit_size) - 1;
    for &byte in input {
        for i in 0..count {
            if out.len() == out_len {
                return Ok(out);
            }
            let shift = 8 - bit_size * (i + 1);
            out.push((byte >> shift) & mask);
        }
    }
    out.resize(out_len, 0);
    Ok(out)
}

fn unpack_pict_16_packbits(input: &[u8], width: usize) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(width * 2);
    let mut i = 0usize;
    while i < input.len() && out.len() < width * 2 {
        let header = input[i] as i8;
        i += 1;
        if header >= 0 {
            let count = header as usize + 1;
            for _ in 0..count {
                if i + 1 >= input.len() {
                    return Err(BioFormatsError::InvalidData(
                        "PICT PackBits16: literal run overruns input".into(),
                    ));
                }
                out.extend_from_slice(&input[i..i + 2]);
                i += 2;
            }
        } else if header != -128 {
            if i + 1 >= input.len() {
                return Err(BioFormatsError::InvalidData(
                    "PICT PackBits16: repeat run missing sample".into(),
                ));
            }
            let sample = [input[i], input[i + 1]];
            i += 2;
            for _ in 0..((-header as usize) + 1) {
                out.extend_from_slice(&sample);
            }
        }
    }
    out.resize(width * 2, 0);
    Ok(out)
}

fn pict_region_crop(full: &[u8], meta: &ImageMetadata, x: u32, y: u32, w: u32, h: u32) -> Vec<u8> {
    let bps = meta.pixel_type.bytes_per_sample();
    if meta.is_rgb && !meta.is_interleaved {
        let plane = meta.size_x as usize * meta.size_y as usize * bps;
        let out_plane = w as usize * h as usize * bps;
        let mut out = vec![0; out_plane * meta.size_c as usize];
        for c in 0..meta.size_c as usize {
            let src_plane = &full[c * plane..(c + 1) * plane];
            for r in 0..h as usize {
                let src = (y as usize + r) * meta.size_x as usize * bps + x as usize * bps;
                let dst = c * out_plane + r * w as usize * bps;
                out[dst..dst + w as usize * bps]
                    .copy_from_slice(&src_plane[src..src + w as usize * bps]);
            }
        }
        return out;
    }
    region_crop(full, meta, x, y, w, h)
}

impl FormatReader for PictReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("pict") | Some("pct"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        matches!(
            header.get(522..524),
            Some([0x11, 0x01]) | Some([0x00, 0x11])
        )
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let decoded = parse_pict(path)?;
        self.path = Some(path.to_path_buf());
        self.meta = Some(decoded.meta);
        self.pixels = Some(decoded.pixels);
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
        self.pixels
            .as_ref()
            .cloned()
            .ok_or(BioFormatsError::NotInitialized)
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
        if x.checked_add(w).is_none_or(|v| v > meta.size_x)
            || y.checked_add(h).is_none_or(|v| v > meta.size_y)
        {
            return Err(BioFormatsError::InvalidData("region out of bounds".into()));
        }
        Ok(pict_region_crop(&full, meta, x, y, w, h))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "bioformats_legacy_{}_{}.pct",
            name,
            std::process::id()
        ))
    }

    fn push_u16(out: &mut Vec<u8>, v: u16) {
        out.extend_from_slice(&v.to_be_bytes());
    }

    fn pict_v2_prefix(width: u16, height: u16) -> Vec<u8> {
        let mut out = vec![0; 512];
        push_u16(&mut out, 0);
        push_u16(&mut out, 0);
        push_u16(&mut out, 0);
        push_u16(&mut out, height);
        push_u16(&mut out, width);
        out.extend_from_slice(&[0x00, 0x11]);
        push_u16(&mut out, 0x02ff);
        out.extend_from_slice(&[0; 18]);
        push_u16(&mut out, height);
        push_u16(&mut out, width);
        out.extend_from_slice(&[0; 4]);
        out
    }

    fn append_pixmap_8(out: &mut Vec<u8>, width: u16, height: u16, rows: &[&[u8]]) {
        if out.len() & 1 != 0 {
            out.push(0);
        }
        push_u16(out, PICT_PACKBITSRECT);
        push_u16(out, 0x8000 | width);
        push_u16(out, 0);
        push_u16(out, 0);
        push_u16(out, height);
        push_u16(out, width);
        out.extend_from_slice(&[0; 18]);
        push_u16(out, 8);
        push_u16(out, 1);
        out.extend_from_slice(&[0; 14]);
        out.extend_from_slice(&[0; 4]);
        push_u16(out, 0);
        push_u16(out, 1);
        for i in 0..2u8 {
            push_u16(out, i as u16);
            out.extend_from_slice(&[i, 0, i, 0, i, 0]);
        }
        out.extend_from_slice(&[0; 18]);
        for row in rows {
            out.push(row.len() as u8);
            out.extend_from_slice(row);
        }
        if out.len() & 1 != 0 {
            out.push(0);
        }
        push_u16(out, PICT_END);
    }

    #[test]
    fn pict_v2_packbits_indexed_rows_decode_and_crop() {
        let path = tmp("packbits");
        let mut data = pict_v2_prefix(8, 2);
        append_pixmap_8(
            &mut data,
            8,
            2,
            &[
                &[7, 1, 2, 3, 4, 5, 6, 7, 8],
                &[7, 9, 10, 11, 12, 13, 14, 15, 16],
            ],
        );
        std::fs::write(&path, data).unwrap();

        let mut reader = PictReader::new();
        reader.set_id(&path).unwrap();
        let meta = reader.metadata();
        assert_eq!((meta.size_x, meta.size_y), (8, 2));
        assert!(meta.is_indexed);
        assert_eq!(
            reader.open_bytes(0).unwrap(),
            vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]
        );
        assert_eq!(
            reader.open_bytes_region(0, 2, 0, 3, 2).unwrap(),
            vec![3, 4, 5, 11, 12, 13]
        );

        std::fs::remove_file(path).ok();
    }

    #[test]
    fn pict_v1_bitmap_bits_expand() {
        let path = tmp("bitmap");
        let mut data = vec![0; 512];
        push_u16(&mut data, 0);
        push_u16(&mut data, 0);
        push_u16(&mut data, 0);
        push_u16(&mut data, 2);
        push_u16(&mut data, 8);
        data.extend_from_slice(&[0x11, 0x01, PICT_BITSRECT as u8]);
        push_u16(&mut data, 1);
        push_u16(&mut data, 0);
        push_u16(&mut data, 0);
        push_u16(&mut data, 2);
        push_u16(&mut data, 8);
        data.extend_from_slice(&[0; 18]);
        data.extend_from_slice(&[0b1010_0101, 0b0101_1010, PICT_END as u8]);
        std::fs::write(&path, data).unwrap();

        let mut reader = PictReader::new();
        reader.set_id(&path).unwrap();
        assert_eq!(
            reader.open_bytes(0).unwrap(),
            vec![1, 0, 1, 0, 0, 1, 0, 1, 0, 1, 0, 1, 1, 0, 1, 0]
        );

        std::fs::remove_file(path).ok();
    }
}
