//! Legacy and obscure format readers.
//!
//! - KodakBipReader: Kodak thermal camera (.bip)
//! - PictReader: Apple PICT format (.pict, .pct), bounded bitmap/pixmap support

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::codec::decompress_packbits;
use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, LookupTable, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::{crop_full_plane, validate_region};

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

/// Kodak Molecular Imaging `.bip` reader, ported from the Java `KodakReader`.
///
/// The format is big-endian with 32-bit float pixels. Dimensions and the pixel
/// offset are located by scanning for the `GBiH` (dimensions) and `BSfD`
/// (pixels) tag markers; `DTag` is the file magic.
pub struct KodakBipReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    pixel_offset: u64,
}

impl KodakBipReader {
    pub fn new() -> Self {
        KodakBipReader {
            path: None,
            meta: None,
            pixel_offset: 0,
        }
    }
}

impl Default for KodakBipReader {
    fn default() -> Self {
        Self::new()
    }
}

/// Find the byte offset of `marker` within `data`, starting the search at
/// `from`. Mirrors the Java `findString` helper (which leaves the pointer at
/// the start of the marker).
fn kodak_find(data: &[u8], marker: &[u8], from: usize) -> Option<usize> {
    if marker.is_empty() || from >= data.len() {
        return None;
    }
    data[from..]
        .windows(marker.len())
        .position(|w| w == marker)
        .map(|p| from + p)
}

fn parse_kodak_bip(path: &Path) -> Result<(ImageMetadata, u64)> {
    let data = std::fs::read(path).map_err(BioFormatsError::Io)?;

    const DIMENSIONS_STRING: &[u8] = b"GBiH";
    const PIXELS_STRING: &[u8] = b"BSfD";

    // findString(DIMENSIONS_STRING); skipBytes(len + 20); readInt sizeX/sizeY.
    let dim_pos = kodak_find(&data, DIMENSIONS_STRING, 0).ok_or_else(|| {
        BioFormatsError::UnsupportedFormat("Kodak .bip: dimensions marker not found".into())
    })?;
    let dim_data = dim_pos + DIMENSIONS_STRING.len() + 20;
    if dim_data + 8 > data.len() {
        return Err(BioFormatsError::Format(
            "Kodak .bip: truncated dimensions block".into(),
        ));
    }
    let width = u32::from_be_bytes([
        data[dim_data],
        data[dim_data + 1],
        data[dim_data + 2],
        data[dim_data + 3],
    ]);
    let height = u32::from_be_bytes([
        data[dim_data + 4],
        data[dim_data + 5],
        data[dim_data + 6],
        data[dim_data + 7],
    ]);
    if width == 0 || height == 0 {
        return Err(BioFormatsError::Format(
            "Kodak .bip: missing image dimensions".into(),
        ));
    }

    // findString(PIXELS_STRING); pixelOffset = filePointer + len + 20.
    let pix_pos = kodak_find(&data, PIXELS_STRING, 0).ok_or_else(|| {
        BioFormatsError::UnsupportedFormat("Kodak .bip: pixel marker not found".into())
    })?;
    let pixel_offset = (pix_pos + PIXELS_STRING.len() + 20) as u64;

    let meta = ImageMetadata {
        size_x: width,
        size_y: height,
        size_z: 1,
        size_c: 1,
        size_t: 1,
        pixel_type: PixelType::Float32,
        bits_per_pixel: 32,
        image_count: 1,
        dimension_order: DimensionOrder::XYCZT,
        is_rgb: false,
        is_interleaved: false,
        is_indexed: false,
        is_little_endian: false, // Kodak .bip is big-endian
        resolution_count: 1,
        series_metadata: HashMap::new(),
        lookup_table: None,
        modulo_z: None,
        modulo_c: None,
        modulo_t: None,
    };
    Ok((meta, pixel_offset))
}

impl FormatReader for KodakBipReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("bip"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // MAGIC_STRING "DTag" appears within the file header.
        header.windows(4).any(|w| w == b"DTag")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let (meta, pixel_offset) = parse_kodak_bip(path)?;
        self.path = Some(path.to_path_buf());
        self.meta = Some(meta);
        self.pixel_offset = pixel_offset;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.pixel_offset = 0;
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
        f.seek(SeekFrom::Start(self.pixel_offset))
            .map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; plane_bytes];
        f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
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
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        crop_full_plane("Kodak BIP", &full, meta, 1, x, y, w, h)
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

/// Decode a QuickTime-compressed (JPEG) PICT payload (opcode 0x0018).
///
/// Faithful to the Java `PictReader`: the JPEG data begins two bytes after the
/// opcode (`jpegOffsets[0] = filePointer + 2`). Additional embedded JPEG
/// streams (tiles) are located by scanning for SOI (0xFFD8) markers after the
/// first EOI (0xFFD9). Each stream is concatenated and decoded, then exposed as
/// interleaved 3-channel RGB (`sizeC = 3`, `rgb = true`, `interleaved = true`).
fn decode_pict_jpeg(
    c: &mut PictCursor<'_>,
    fallback_width: u32,
    fallback_height: u32,
    version_one: bool,
) -> Result<PictDecoded> {
    let data = c.data;
    // jpegOffsets[0] = filePointer + 2 (current position is just past opcode).
    let first = c.position() + 2;
    if first >= data.len() {
        return Err(BioFormatsError::Format(
            "PICT: truncated JPEG payload".into(),
        ));
    }

    // Collect the start offsets of each embedded JPEG stream, following the
    // Java scan: skip 16-bit words until the first EOI (0xFFD9), then for each
    // subsequent SOI (0xFFD8) record offset (filePointer - 2).
    let mut offsets = vec![first];
    let mut pos = first;
    // advance to first EOI
    while pos + 1 < data.len() {
        let v = u16::from_be_bytes([data[pos], data[pos + 1]]);
        pos += 2;
        if v == 0xffd9 {
            break;
        }
    }
    while pos + 1 < data.len() {
        let v = u16::from_be_bytes([data[pos], data[pos + 1]]);
        pos += 2;
        if v == 0xffd8 {
            offsets.push(pos - 2);
        }
    }

    // Concatenate each JPEG stream (from its offset up to the next offset, or
    // end of file for the last) and decode. For the common single-stream case
    // this decodes the JPEG from `first` to end of file.
    let mut combined: Vec<u8> = Vec::new();
    for (i, &start) in offsets.iter().enumerate() {
        let end = offsets.get(i + 1).copied().unwrap_or(data.len());
        if start >= end || end > data.len() {
            continue;
        }
        let decoded = crate::common::codec::decompress_jpeg(&data[start..end])?;
        combined.extend_from_slice(&decoded);
    }
    if combined.is_empty() {
        return Err(BioFormatsError::Format(
            "PICT: embedded JPEG produced no pixels".into(),
        ));
    }

    // Derive dimensions from the decoded RGB buffer length when possible.
    let (width, height) = if fallback_width > 0 && fallback_height > 0 {
        (fallback_width, fallback_height)
    } else {
        return Err(BioFormatsError::Format(
            "PICT: JPEG payload without known dimensions".into(),
        ));
    };

    let expected = width as usize * height as usize * 3;
    if combined.len() < expected {
        combined.resize(expected, 0);
    } else if combined.len() > expected {
        combined.truncate(expected);
    }

    let mut meta = new_pict_meta(width, height, 3, true, false, None, version_one);
    meta.is_interleaved = true;
    Ok(PictDecoded {
        meta,
        pixels: combined,
    })
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
                decoded = Some(decode_pict_jpeg(&mut c, width, height, version_one)?);
                break;
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

    decoded.ok_or_else(|| {
        BioFormatsError::UnsupportedFormat(
            "PICT: no supported bitmap/pixmap payload found; native vector-only PICT drawing is unsupported"
                .into(),
        )
    })
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
                    format!(
                        "PICT vector pixmap payloads with {pixel_size}-bit pixels are unsupported; only direct 16/32-bit pixmaps are decoded"
                    ),
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
                let out = decompress_packbits(packed)?;
                let min_len = match pixel_size {
                    24 | 32 => width.saturating_mul(comp_count),
                    _ => row_bytes.max(width),
                };
                if out.len() < min_len {
                    return Err(BioFormatsError::InvalidData(format!(
                        "PICT PackBits row decoded to {} bytes, expected at least {min_len}",
                        out.len()
                    )));
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
            "PICT pixel size {other} is unsupported; supported bitmap/pixmap pixel sizes are 1, 2, 4, 8, 16, 24, and 32"
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
            if out.len() == out_len {
                return Ok(out);
            }
        }
    }
    Err(BioFormatsError::InvalidData(format!(
        "PICT bit row expanded to {} pixels, expected {out_len}",
        out.len()
    )))
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
    if out.len() != width * 2 {
        return Err(BioFormatsError::InvalidData(format!(
            "PICT PackBits16 row decoded to {} bytes, expected {}",
            out.len(),
            width * 2
        )));
    }
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
    if meta.is_rgb && meta.is_interleaved {
        // Interleaved RGB (e.g. JPEG-in-PICT): samples are packed per pixel.
        let spp = meta.size_c as usize;
        let row = meta.size_x as usize * spp * bps;
        let out_row = w as usize * spp * bps;
        let mut out = Vec::with_capacity(h as usize * out_row);
        for r in 0..h as usize {
            let src = &full[(y as usize + r) * row..];
            let start = x as usize * spp * bps;
            out.extend_from_slice(&src[start..start + out_row]);
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
        self.meta
            .as_ref()
            .unwrap_or(crate::common::reader::uninitialized_metadata())
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
        validate_region("PICT", meta.size_x, meta.size_y, x, y, w, h)?;
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

    fn append_vector_pixmap_9a_header(out: &mut Vec<u8>, width: u16, height: u16, pixel_size: u16) {
        if out.len() & 1 != 0 {
            out.push(0);
        }
        push_u16(out, PICT_PIXMAP_9A);
        out.extend_from_slice(&[0; 6]);
        push_u16(out, 0);
        push_u16(out, 0);
        push_u16(out, height);
        push_u16(out, width);
        out.extend_from_slice(&[0; 18]);
        push_u16(out, pixel_size);
        push_u16(out, 1);
        out.extend_from_slice(&[0; 14]);
    }

    fn append_pixmap_with_pixel_size(
        out: &mut Vec<u8>,
        width: u16,
        height: u16,
        pixel_size: u16,
        rows: &[&[u8]],
    ) {
        if out.len() & 1 != 0 {
            out.push(0);
        }
        push_u16(out, PICT_PACKBITSRECT);
        push_u16(out, 0x8000 | 2);
        push_u16(out, 0);
        push_u16(out, 0);
        push_u16(out, height);
        push_u16(out, width);
        out.extend_from_slice(&[0; 18]);
        push_u16(out, pixel_size);
        push_u16(out, 1);
        out.extend_from_slice(&[0; 14]);
        out.extend_from_slice(&[0; 4]);
        push_u16(out, 0);
        push_u16(out, 0);
        push_u16(out, 0);
        out.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
        out.extend_from_slice(&[0; 18]);
        for row in rows {
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
        let err = reader.open_bytes_region(0, 0, 0, 0, 1).unwrap_err();
        assert!(
            err.to_string()
                .contains("width and height must be non-zero"),
            "unexpected error: {err}"
        );

        std::fs::remove_file(path).ok();
    }

    #[test]
    fn pict_v2_packbits_short_row_is_rejected() {
        let path = tmp("packbits_short");
        let mut data = pict_v2_prefix(8, 1);
        append_pixmap_8(&mut data, 8, 1, &[&[3, 1, 2, 3, 4]]);
        std::fs::write(&path, data).unwrap();

        let mut reader = PictReader::new();
        let err = reader.set_id(&path).unwrap_err();
        assert!(err.to_string().contains("PackBits row decoded"));

        std::fs::remove_file(path).ok();
    }

    #[test]
    fn pict_v2_vector_pixmap_9a_boundary_is_explicit_unsupported() {
        let path = tmp("vector_pixmap_9a");
        let mut data = pict_v2_prefix(2, 1);
        append_vector_pixmap_9a_header(&mut data, 2, 1, 8);
        std::fs::write(&path, data).unwrap();

        let mut reader = PictReader::new();
        let err = reader.set_id(&path).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::UnsupportedFormat(ref message)
                if message.contains("vector pixmap payloads with 8-bit pixels are unsupported")),
            "unexpected error: {err}"
        );

        std::fs::remove_file(path).ok();
    }

    #[test]
    fn pict_v2_unsupported_pixmap_pixel_size_is_explicit() {
        let path = tmp("unsupported_pixel_size");
        let mut data = pict_v2_prefix(2, 1);
        append_pixmap_with_pixel_size(&mut data, 2, 1, 12, &[&[0, 0]]);
        std::fs::write(&path, data).unwrap();

        let mut reader = PictReader::new();
        let err = reader.set_id(&path).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::UnsupportedFormat(ref message)
                if message.contains("PICT pixel size 12 is unsupported")),
            "unexpected error: {err}"
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
